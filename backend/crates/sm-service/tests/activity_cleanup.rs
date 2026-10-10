//! 活动中心保留期清理的集成测试（真实 PostgreSQL）。
//!
//! 钉住三件最容易写错的事：
//!
//! 1. 保留额**只在终态里算** —— `pending` 行无论多旧都不删（它是队列元素，
//!    删掉等于让已排队的任务凭空消失），而且它**不占**保留额；
//! 2. 删台账前**先把通知外键置空**，且通知本身保留；
//! 3. 通知用 `read_at` 而非 `created_at` 作窗口基准，未读一律保留。
//!
//! 另外把终态字面量钉在这里：本批修掉一个 bug —— `finish()` 曾写
//! `"succeeded"`，而上游白名单是 `{"pending","running","completed","failed"}`。
//!
//! 上游出处：`src/service/system/activity_cleanup_service.py`。

use sm_db::common::time::now_utc;
use sm_db::repo::{
    BackgroundTaskRunRepository, NewNotification, NewTaskRun, SystemNotificationRepository,
};
use sm_db::system::activity::task_state;
use sm_db::testing::TestDb;
use sm_service::system::activity_cleanup::{ActivityCleanupService, RetentionPolicy};

fn key(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}-{}", C.fetch_add(1, Ordering::Relaxed))
}

fn policy(task_runs_per_key: i64, read_notification_days: i64) -> RetentionPolicy {
    RetentionPolicy {
        task_runs_per_key,
        read_notification_days,
    }
}

async fn enqueue(db: &TestDb, task_key: &str) -> i32 {
    BackgroundTaskRunRepository::new(db.pool().clone())
        .enqueue(&NewTaskRun {
            task_key: task_key.to_owned(),
            task_name: "fixture".to_owned(),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            params: None,
            scheduled_at: None,
        })
        .await
        .expect("入队")
        .id
}

/// 排一条**已终态**的运行记录。
///
/// # 为什么直接写 `state` 而不是走 `claim` + `finish`
///
/// `claim` 按设计取**最老**的可领取行（`ORDER BY scheduled_at, id`）。而
/// 「pending 行永不被清理」那个用例必须先排一条 pending、再排终态行 ——
/// 于是 `claim` 会领走那条 pending 夹具并把它改成终态，恰好毁掉待观察的
/// 对象。批量夹具因此直接写终态。
///
/// 真实写入路径（`claim` → `finish`）由
/// [`finish_writes_the_state_upstream_expects`] 单独覆盖 —— 那里表里只有一行，
/// 不存在领错。
async fn terminal_run(db: &TestDb, task_key: &str) -> i32 {
    let id = enqueue(db, task_key).await;
    sqlx::query(
        "UPDATE background_task_run \
         SET state = $2, finished_at = $3 WHERE id = $1",
    )
    .bind(id)
    // 字面量写在这里而不是引常量：常量若被改错，这个断言会立刻指出。
    .bind("completed")
    .bind(now_utc())
    .execute(db.pool())
    .await
    .expect("置终态");
    id
}

/// 排一条运行记录并经 `claim` + `finish` 推到终态（验真实写入路径）。
async fn finished_run(db: &TestDb, task_key: &str) -> i32 {
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = enqueue(db, task_key).await;
    let claimed = repo
        .claim(chrono::Duration::seconds(60))
        .await
        .expect("领取")
        .expect("应当有可领取的任务");
    assert_eq!(claimed.run.id, id, "领取到的不是刚排的那条");
    repo.finish(id, &Default::default()).await.expect("完成");
    id
}

async fn state_of(db: &TestDb, id: i32) -> String {
    BackgroundTaskRunRepository::new(db.pool().clone())
        .find_by_id(id)
        .await
        .unwrap()
        .expect("行应当存在")
        .state
}

async fn count_key(db: &TestDb, task_key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM background_task_run WHERE task_key = $1")
        .bind(task_key)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn notify(db: &TestDb, title: &str, related: Option<i32>) -> i32 {
    SystemNotificationRepository::new(db.pool().clone())
        .notify(&NewNotification {
            category: "reminder".to_owned(),
            title: title.to_owned(),
            content: "c".to_owned(),
            event_type: None,
            dedupe_key: None,
            resource_type: None,
            resource_id: None,
            related_task_run_id: related,
            related_resource_type: None,
            related_resource_id: None,
        })
        .await
        .expect("发通知")
        .expect("不该被去重")
        .id
}

async fn notification_ids(db: &TestDb) -> Vec<i32> {
    sqlx::query_scalar::<_, i32>("SELECT id FROM system_notification ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// 把一条通知伪造成「N 天前读的」。
async fn backdate_read(db: &TestDb, id: i32, days: i64) {
    sqlx::query("UPDATE system_notification SET is_read = true, read_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now_utc() - chrono::Duration::days(days))
        .execute(db.pool())
        .await
        .unwrap();
}

// ================================================================ 终态字面量

#[tokio::test]
async fn finish_writes_the_state_upstream_expects() {
    // 上游白名单 ALLOWED_TASK_STATES = {pending, running, completed, failed}。
    // 此前这里写的是 "succeeded"，落在白名单外 —— 后果是按
    // ("completed","failed") 判终态的清理**永远删不到已完成的任务**。
    let db = TestDb::require().await;
    // 走真实的 claim → finish 写入路径（表里只有一行，不存在领错）
    let state = state_of(&db, finished_run(&db, &key("literal")).await).await;
    assert_eq!(state, "completed");
    assert!(task_state::is_valid(&state));
    assert!(task_state::is_terminal(&state));

    // 而 thumbnail_generation_state 的 SUCCEEDED 确实叫 "succeeded" ——
    // 两个状态机里有同名的常量、不同的字面量，看起来像笔误其实不是。
    assert!(!task_state::is_valid("succeeded"));
    assert!(!task_state::is_terminal("succeeded"));
    assert!(!task_state::is_terminal(task_state::PENDING));
    assert!(!task_state::is_terminal(task_state::RUNNING));
}

// ================================================================ 保留额

#[tokio::test]
async fn only_terminal_rows_beyond_the_quota_are_deleted() {
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let k = key("quota");
    for _ in 0..5 {
        terminal_run(&db, &k).await;
    }

    let stats = svc.cleanup(policy(2, 3)).await.expect("清理");
    assert_eq!(stats.deleted_task_runs, 3, "5 条保留 2 条 ⇒ 删 3 条");
    assert_eq!(count_key(&db, &k).await, 2, "只剩最新的 2 条");
}

#[tokio::test]
async fn a_pending_row_is_never_deleted_and_does_not_consume_the_quota() {
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let k = key("mix");
    let pending = enqueue(&db, &k).await;
    for _ in 0..4 {
        terminal_run(&db, &k).await;
    }

    svc.cleanup(policy(1, 3)).await.unwrap();

    assert_eq!(
        state_of(&db, pending).await,
        "pending",
        "pending 行必须在清理后仍存在 —— 哪怕它的 id 比所有终态行都小"
    );
    assert_eq!(
        count_key(&db, &k).await,
        2,
        "4 条终态留 1 条，pending 另计 —— 它不占保留额"
    );
}

#[tokio::test]
async fn quota_boundaries_behave() {
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());

    // 不足额度 → 不动
    let few = key("few");
    for _ in 0..2 {
        terminal_run(&db, &few).await;
    }
    assert_eq!(
        svc.cleanup(policy(200, 3)).await.unwrap().deleted_task_runs,
        0
    );
    assert_eq!(count_key(&db, &few).await, 2);

    // 额度 0 → 终态一条不留（不是「删 0 条」）
    let zero = key("zero");
    for _ in 0..3 {
        terminal_run(&db, &zero).await;
    }
    // 注意：额度 0 对**所有** key 生效，所以 `few` 那 2 条也会一起没 ——
    // 因此按 key 断言而不是看总数。
    svc.cleanup(policy(0, 3)).await.unwrap();
    assert_eq!(count_key(&db, &zero).await, 0, "额度 0 ⇒ 终态一条不留");
    assert_eq!(count_key(&db, &few).await, 0, "其它 key 同样适用额度 0");

    // 负值 → 拒（否则 OFFSET 变成「从末尾倒数」，语义是「删掉最新的 N 条」）
    let err = svc.cleanup(policy(-1, 3)).await.expect_err("负值必须被拒");
    assert_eq!((err.status, err.code()), (422, "validation_error"));
}

// ================================================================ 通知外键

#[tokio::test]
async fn notifications_survive_and_get_detached_when_their_run_is_deleted() {
    // 通知是独立实体：「某某任务失败了」这条事实在台账被回收后仍然成立。
    // 上游显式做同一件事，注释写明「避免悬挂引用，不依赖数据库级联行为」。
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let run_id = terminal_run(&db, &key("detach")).await;
    notify(&db, "任务失败", Some(run_id)).await;

    svc.cleanup(policy(0, 3)).await.unwrap();

    let links: Vec<(i32, Option<i32>)> =
        sqlx::query_as("SELECT id, related_task_run_id FROM system_notification ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(links.len(), 1, "通知**不该**被任务清理带走");
    assert_eq!(links[0].1, None, "外键必须置空，否则是指向已删行的悬挂引用");
}

// ================================================================ 通知保留期

#[tokio::test]
async fn only_read_notifications_past_the_window_are_deleted() {
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let notifications = SystemNotificationRepository::new(db.pool().clone());

    let long_read = notify(&db, "很久以前读过", None).await;
    backdate_read(&db, long_read, 30).await;

    let just_read = notify(&db, "刚读过", None).await;
    notifications.mark_read(just_read).await.unwrap();

    // 未读但创建于 30 天前：模拟「放了很久没人看」
    let unread = notify(&db, "仍未读", None).await;
    sqlx::query("UPDATE system_notification SET created_at = $2 WHERE id = $1")
        .bind(unread)
        .bind(now_utc() - chrono::Duration::days(30))
        .execute(db.pool())
        .await
        .unwrap();

    let stats = svc.cleanup(policy(200, 3)).await.unwrap();
    assert_eq!(stats.deleted_notifications, 1, "只该删那条 30 天前读过的");

    let left = notification_ids(&db).await;
    assert!(left.contains(&just_read), "刚读过的应保留");
    assert!(
        left.contains(&unread),
        "未读的一律保留 —— 用户还没看到，哪怕放了 30 天"
    );
    assert!(!left.contains(&long_read));
}

#[tokio::test]
async fn a_second_run_is_a_no_op() {
    // 调度器每天跑一次，重复跑必须幂等：不报错，也不二次删除。
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let k = key("idem");
    for _ in 0..4 {
        terminal_run(&db, &k).await;
    }
    let n_id = notify(&db, "x", None).await;
    backdate_read(&db, n_id, 30).await;

    let first = svc.cleanup(policy(1, 3)).await.unwrap();
    assert!(first.deleted_task_runs > 0 && first.deleted_notifications > 0);

    let second = svc.cleanup(policy(1, 3)).await.unwrap();
    assert_eq!(second.deleted_task_runs, 0, "第二轮没有可删的");
    assert_eq!(second.deleted_notifications, 0);
    assert_eq!(svc.cleanup(policy(1, 3)).await.unwrap(), second);
}

#[tokio::test]
async fn the_stats_carry_only_the_two_counters_this_slice_implements() {
    // 上游 stats 有第三个键 deleted_metadata_search_assets（catalog 域，未开工）。
    // 本批**不**返回它：填 0 会让任务中心显示「清理了 0 项」而实际是「没做」，
    // 比缺字段更难排查。
    let db = TestDb::require().await;
    let svc = ActivityCleanupService::new(db.pool());
    let json = serde_json::to_value(svc.cleanup(policy(200, 3)).await.unwrap()).unwrap();
    let keys: Vec<&String> = json.as_object().expect("对象").keys().collect();
    assert_eq!(keys.len(), 2, "实际序列化出的键：{keys:?}");
    assert!(json.get("deleted_task_runs").is_some());
    assert!(json.get("deleted_notifications").is_some());
    assert!(json.get("deleted_metadata_search_assets").is_none());
}
