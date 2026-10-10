//! 任务队列内核的集成测试，对应上游
//! `src/service/system/task_queue_service.py`。
//!
//! # 为什么这一批**必须**连真库
//!
//! 队列的每一条核心保证都由 PostgreSQL 实现，而不是由 Rust 代码实现：
//!
//! | 保证 | 靠什么 |
//! |---|---|
//! | 同 `task_key` 最多一个 pending/running | `UNIQUE(mutex_key)` |
//! | 并发领取互不阻塞、不重复 | `FOR UPDATE SKIP LOCKED` |
//! | 领取与置 running 不可分 | 单条 `UPDATE ... WHERE id = (SELECT ... FOR UPDATE)` |
//! | 回收与续租的并发安全 | `state = 'running'` 条件 + 行锁 |
//!
//! 换成 mock 或内存实现，这四条全部「通过」，而真实并发下会重复执行或永久
//! 卡死。`cargo test --lib` 覆盖不到任何一条。
//!
//! # 每条测试都断言**状态 + 副作用**
//!
//! 队列的错误几乎都是「看起来对但结果错」：跳过了该跑的、回收了刚续租的。
//! 所以只断言「返回了 Ok」没有意义 —— 这里一律回读数据库确认终态。

use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun, TaskLanes};
use sm_db::system::activity::{task_state, BackgroundTaskRun};
use sm_db::testing::TestDb;
use sm_service::system::task_queue::{
    ConflictPolicy, EnqueueOutcome, TaskQueueService, BOOTSTRAP_QUEUE_TASK_KEYS,
    DEFAULT_LEASE_SECONDS, FAILURE_CODE_QUEUE_LEASE_EXPIRED, INTERNAL_FAILURE_CODE_KEY,
    INTERRUPTED_ERROR_MESSAGE, LEASE_EXPIRED_ERROR_MESSAGE,
};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn svc(db: &TestDb) -> TaskQueueService {
    TaskQueueService::new(db.pool())
}

/// 入队一个任务（绕开 service，直接用仓储）——用来造前置状态。
async fn seed_run(
    db: &TestDb,
    task_key: &str,
    scheduled_at: Option<chrono::NaiveDateTime>,
) -> BackgroundTaskRun {
    BackgroundTaskRunRepository::new(db.pool().clone())
        .enqueue(&NewTaskRun {
            task_key: task_key.to_owned(),
            task_name: format!("{task_key} 任务"),
            trigger_type: "manual".to_owned(),
            mutex_key: Some(format!("aps:{task_key}")),
            params: None,
            scheduled_at,
        })
        .await
        .expect("enqueue")
}

// ================================================================ 互斥键

#[tokio::test]
async fn the_mutex_key_is_the_aps_namespace() {
    let db = TestDb::require().await;
    let key = format!("mtx-{:06}", n());
    let run = seed_run(&db, &key, None).await;
    assert_eq!(
        run.mutex_key.as_deref(),
        Some(format!("aps:{key}").as_str()),
        "互斥键必须是 aps: 前缀 + task_key —— 存量库里已有这个前缀的行"
    );
    assert_eq!(TaskQueueService::mutex_key(&key), format!("aps:{key}"));
}

// ================================================================ 入队

#[tokio::test]
async fn a_first_enqueue_succeeds() {
    let db = TestDb::require().await;
    let key = format!("enq-{:06}", n());
    let outcome = svc(&db)
        .enqueue(&key, "manual", Some("手动任务"), None, ConflictPolicy::Skip)
        .await
        .expect("enqueue");

    assert!(outcome.is_enqueued(), "首次入队应当成功");
    let run = outcome.enqueued().expect("应有入队结果");
    assert_eq!(run.task_key, key);
    assert_eq!(run.state, task_state::PENDING);
    assert!(
        run.mutex_key.is_some(),
        "入队必须带互斥键，否则没有 coalesce"
    );
}

/// 同 `task_key` 第二次入队必须撞唯一约束 —— 这就是 coalesce。
#[tokio::test]
async fn a_second_enqueue_of_the_same_key_is_skipped() {
    let db = TestDb::require().await;
    let key = format!("coal-{:06}", n());
    let s = svc(&db);

    let first = s
        .enqueue(&key, "scheduled", None, None, ConflictPolicy::Skip)
        .await
        .expect("first");
    assert!(first.is_enqueued());

    let second = s
        .enqueue(&key, "scheduled", None, None, ConflictPolicy::Skip)
        .await
        .expect("second 不该报错");
    assert!(
        !second.is_enqueued(),
        "同 key 已在队列里，第二次触发必须按 coalesce 丢弃"
    );
}

/// `Skip` 策略**不查**阻塞方，`Raise` 查 —— 两种都返回 `Skipped`，但后者
/// 带 `blocking_task_run_id`，手动触发要靠它告诉用户「正在跑的是哪一条」。
#[tokio::test]
async fn only_the_raise_policy_reports_the_blocking_run() {
    let db = TestDb::require().await;
    let key = format!("blk-{:06}", n());
    let s = svc(&db);

    let first = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("first");
    let blocking_id = first.enqueued().expect("已入队").id;

    let skipped = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("skip 不查阻塞方");
    assert!(
        matches!(
            skipped,
            EnqueueOutcome::Skipped {
                blocking_task_run_id: None
            }
        ),
        "Skip 策略不查阻塞方，省掉一次往返"
    );

    let raised = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Raise)
        .await
        .expect("raise");
    match raised {
        EnqueueOutcome::Skipped {
            blocking_task_run_id: Some(id),
        } => assert_eq!(id, blocking_id, "阻塞方就是先入队那一行"),
        other => panic!("期望 Skipped 带阻塞方 id，实际 {other:?}"),
    }
}

/// 互斥键被占用时，**其它 task_key** 不受影响。
#[tokio::test]
async fn a_held_mutex_key_does_not_block_other_keys() {
    let db = TestDb::require().await;
    let a = format!("iso-a-{:06}", n());
    let b = format!("iso-b-{:06}", n());
    let s = svc(&db);

    s.enqueue(&a, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("a");
    let other = s
        .enqueue(&b, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("b 不该被 a 挡住");

    assert!(other.is_enqueued(), "互斥是按 task_key 独立的");
}

/// 互斥键在**任务结束后**释放 —— 否则重试永远撞唯一约束。
#[tokio::test]
async fn finishing_a_task_releases_the_mutex_key() {
    let db = TestDb::require().await;
    let key = format!("rel-{:06}", n());
    let s = svc(&db);
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let first = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("first");
    let run_id = first.enqueued().expect("已入队").id;

    // 领起来再完成
    let claimed = repo
        .claim(chrono::Duration::seconds(DEFAULT_LEASE_SECONDS))
        .await
        .expect("claim")
        .expect("应有可领任务");
    repo.finish(claimed.run.id, &Default::default())
        .await
        .expect("finish");

    let again = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("完成后应能再次入队");
    assert!(
        again.is_enqueued(),
        "完成释放了 mutex_key，同 key 必须能再排一次"
    );

    // run_id 只是为了确认第一行确实存在过
    assert!(run_id > 0);
}

// ================================================================ 领取

#[tokio::test]
async fn claiming_moves_the_row_to_running_with_a_lease() {
    let db = TestDb::require().await;
    let key = format!("clm-{:06}", n());
    let s = svc(&db);
    s.enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("enqueue");

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("应有可领任务");

    assert_eq!(claimed.run.task_key, key);
    assert_eq!(
        claimed.run.state,
        task_state::RUNNING,
        "领取与置 running 必须在同一条语句里，否则崩溃会留下 pending 被反复领"
    );
    assert!(claimed.run.lease_expires_at.is_some(), "领取必须发放租约");
    assert!(claimed.lease_expires_at > sm_db::common::time::now_utc());
}

/// 库里可能有别的测试留下的 pending 行，所以只断言「领到的不是这一条」。
/// 领到别人那种情况下这个测试会假失败 —— 改为断言总状态不变。
#[tokio::test]
async fn an_empty_queue_yields_none_rather_than_an_error() {
    let db = TestDb::require().await;
    let s = svc(&db);
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    // 把当前所有 pending 行都领走，确保队列空
    while repo
        .claim(chrono::Duration::seconds(DEFAULT_LEASE_SECONDS))
        .await
        .expect("claim")
        .is_some()
    {}

    let got = s.claim_next(None, None).await.expect("空队列不该报错");
    assert!(
        got.is_none(),
        "空队列返回 None —— 空闲 worker 反复领取是正常的"
    );
}

/// 未来的 `scheduled_at` 不可领。
#[tokio::test]
async fn a_future_scheduled_at_is_not_claimable() {
    let db = TestDb::require().await;
    let key = format!("fut-{:06}", n());
    let future = sm_db::common::time::now_utc() + chrono::Duration::hours(1);
    seed_run(&db, &key, Some(future)).await;

    let claimed = svc(&db).claim_next(None, None).await.expect("claim");
    // 库里可能有别的可领行；只有当领到的正好是这一条时才断言失败
    if let Some(c) = claimed {
        assert_ne!(c.run.task_key, key, "scheduled_at 在一小时后的行不该被领取");
    }
}

// ================================================================ 并发道（lanes）

#[tokio::test]
async fn an_include_lane_claims_only_those_keys() {
    let db = TestDb::require().await;
    let wanted = format!("lane-in-{:06}", n());
    let other = format!("lane-out-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &other, None).await;
    seed_run(&db, &wanted, None).await;

    let lanes = TaskLanes::including([wanted.clone()]);
    let claimed = s
        .claim_next(None, Some(lanes))
        .await
        .expect("claim")
        .expect("include 里有任务");

    assert_eq!(
        claimed.run.task_key, wanted,
        "include 道只该领 included 里的 key"
    );
}

#[tokio::test]
async fn an_exclude_lane_skips_those_keys() {
    let db = TestDb::require().await;
    let excluded = format!("lane-ex-{:06}", n());
    let allowed = format!("lane-ok-{:06}", n());
    let s = svc(&db);
    // 先入队被排除的那个，并让它保持 pending
    seed_run(&db, &excluded, None).await;
    seed_run(&db, &allowed, None).await;

    let lanes = TaskLanes::excluding([excluded.clone()]);
    let claimed = s
        .claim_next(None, Some(lanes))
        .await
        .expect("claim")
        .expect("exclude 后仍有可领任务");

    assert_eq!(
        claimed.run.task_key, allowed,
        "exclude 道不该领被排除的 key"
    );
}

#[tokio::test]
async fn unrestricted_lanes_add_no_condition() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    // 队列空着，unrestricted 不该把 SQL 条件写坏
    while repo
        .claim(chrono::Duration::seconds(DEFAULT_LEASE_SECONDS))
        .await
        .expect("claim")
        .is_some()
    {}

    let key = format!("free-{:06}", n());
    seed_run(&db, &key, None).await;
    let s = svc(&db);
    let lanes = TaskLanes::default();
    assert!(lanes.is_unrestricted());

    // `claim_next` 会把 unrestricted 的 lanes 过滤掉，于是这一行**照常可领**。
    // 若这里返回 None，说明空 lanes 被当成了「只领空集合」而领不到任何东西。
    let claimed = s
        .claim_next(None, Some(lanes))
        .await
        .expect("claim")
        .expect("unrestricted 应当照常领到那一行");
    assert_eq!(claimed.run.task_key, key);
}

// ================================================================ 续租

#[tokio::test]
async fn renewing_extends_the_lease_and_reports_the_count() {
    let db = TestDb::require().await;
    let key = format!("ren-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &key, None).await;

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");
    let before = claimed.lease_expires_at;

    let renewed = s
        .renew_leases(&[claimed.run.id], Some(DEFAULT_LEASE_SECONDS * 2))
        .await
        .expect("renew");
    assert_eq!(renewed, 1, "一个 running 任务应被续上一次");

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let after = repo
        .find_by_id(claimed.run.id)
        .await
        .expect("find")
        .expect("行还在");
    let after_lease = after.lease_expires_at.expect("running 行有租约");
    assert!(
        after_lease > before,
        "续租后到期时间必须推后：{after_lease:?} 应晚于 {before:?}"
    );
}

/// 已被回收（回 pending）的行**不能**被续租 —— 否则会复活别人的任务。
#[tokio::test]
async fn renewing_a_reclaimed_row_does_not_revive_it() {
    let db = TestDb::require().await;
    let key = format!("rev-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &key, None).await;

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    // 直接把它推回 pending（模拟被回收）
    repo.reclaim_stale(sm_db::common::time::now_utc() + chrono::Duration::hours(1))
        .await
        .expect("reclaim");

    let renewed = s
        .renew_leases(&[claimed.run.id], None)
        .await
        .expect("renew");
    assert_eq!(renewed, 0, "pending 行不该被续租复活");

    let after = repo
        .find_by_id(claimed.run.id)
        .await
        .expect("find")
        .expect("行还在");
    assert_eq!(after.state, task_state::PENDING, "状态必须仍是 pending");
}

#[tokio::test]
async fn renewing_an_empty_batch_is_a_noop() {
    let db = TestDb::require().await;
    let got = svc(&db).renew_leases(&[], None).await.expect("空批次");
    assert_eq!(got, 0);
}

// ================================================================ 租约过期回收

/// 租约过期**判失败**，并写入 `queue_lease_expired` 失败码。
#[tokio::test]
async fn an_expired_lease_is_failed_with_the_failure_code() {
    let db = TestDb::require().await;
    let key = format!("exp-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &key, None).await;

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");
    // 把租约推到过去
    let past = sm_db::common::time::now_utc() - chrono::Duration::hours(1);
    sqlx::query("UPDATE background_task_run SET lease_expires_at = $2 WHERE id = $1")
        .bind(claimed.run.id)
        .bind(past)
        .execute(db.pool())
        .await
        .expect("age the lease");

    let recovered = s.recover_expired_leases(None).await.expect("recover");
    assert!(
        recovered.iter().any(|r| r.id == claimed.run.id),
        "过期租约应被回收"
    );

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let after = repo
        .find_by_id(claimed.run.id)
        .await
        .expect("find")
        .expect("行还在");
    assert_eq!(
        after.state,
        task_state::FAILED,
        "过期租约判失败（对齐上游）"
    );
    assert_eq!(
        after.error_message.as_deref(),
        Some(LEASE_EXPIRED_ERROR_MESSAGE)
    );

    // 失败码写进 result_summary，客户端据此决定要不要提示重试
    let summary: serde_json::Value =
        serde_json::from_str(after.result_summary.as_deref().unwrap_or("{}")).expect("json");
    assert_eq!(
        summary[INTERNAL_FAILURE_CODE_KEY],
        FAILURE_CODE_QUEUE_LEASE_EXPIRED
    );
}

/// 租约**未**过期的不回收 —— 正在跑的任务不能被误伤。
#[tokio::test]
async fn a_live_lease_is_not_recovered() {
    let db = TestDb::require().await;
    let key = format!("live-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &key, None).await;

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");

    let recovered = s.recover_expired_leases(None).await.expect("recover");
    assert!(
        !recovered.iter().any(|r| r.id == claimed.run.id),
        "租约还活着，不该被回收"
    );

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let after = repo
        .find_by_id(claimed.run.id)
        .await
        .expect("find")
        .expect("行还在");
    assert_eq!(after.state, task_state::RUNNING, "必须仍是 running");
}

/// 回收**释放** mutex_key —— 否则下一次触发永远撞唯一约束。
#[tokio::test]
async fn recovering_a_stale_run_releases_its_mutex_key() {
    let db = TestDb::require().await;
    let key = format!("relstale-{:06}", n());
    let s = svc(&db);
    seed_run(&db, &key, None).await;

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");
    let past = sm_db::common::time::now_utc() - chrono::Duration::hours(1);
    sqlx::query("UPDATE background_task_run SET lease_expires_at = $2 WHERE id = $1")
        .bind(claimed.run.id)
        .bind(past)
        .execute(db.pool())
        .await
        .expect("age");

    s.recover_expired_leases(None).await.expect("recover");

    let again = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("回收后应能再次入队");
    assert!(
        again.is_enqueued(),
        "回收必须释放 mutex_key，否则同 key 永远排不进来"
    );
}

// ================================================================ 进程中断回收

/// 进程刚启动时，遗留的 `running` 行**不看租约**也要回收 —— 那些属于上一个
/// 已死的进程。
///
/// 走 `svc.enqueue` 而不是 `seed_run`：本条测试要守的正是「service 自己
/// 入队的行能被 `recover_interrupted_runs` 看到」。`seed_run` 传
/// `scheduled_at: None` 会被 `scheduled_at IS NOT NULL` 过滤掉，用它测
/// 就把真正要守的东西绕过去了。
#[tokio::test]
async fn interrupted_runs_are_failed_regardless_of_a_live_lease() {
    let db = TestDb::require().await;
    let key = format!("intr-{:06}", n());
    let s = svc(&db);
    s.enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("enqueue");

    let claimed = s
        .claim_next(None, None)
        .await
        .expect("claim")
        .expect("可领");
    // 租约还有 300 秒，是「活的」，但进程已经死了
    assert!(claimed.lease_expires_at > sm_db::common::time::now_utc());

    let recovered = s.recover_interrupted_runs().await.expect("recover");
    assert!(
        recovered.iter().any(|r| r.id == claimed.run.id),
        "遗留的 running 行必须被回收，否则要等租约到期才动"
    );

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let after = repo
        .find_by_id(claimed.run.id)
        .await
        .expect("find")
        .expect("行还在");
    assert_eq!(after.state, task_state::FAILED);
    assert_eq!(
        after.error_message.as_deref(),
        Some(INTERRUPTED_ERROR_MESSAGE)
    );
}

/// `enqueue` 必须写 `scheduled_at`（入队时间），否则上面那条回收永远看不到它。
///
/// 这条断言单独存在是因为它守的是一个**曾经真实存在过**的缺陷：
/// `enqueue` 传 `None` 时，`recover_interrupted_runs` 的
/// `scheduled_at IS NOT NULL` 会把所有队列行都排除掉。
#[tokio::test]
async fn an_enqueued_run_records_its_queue_time() {
    let db = TestDb::require().await;
    let key = format!("qtime-{:06}", n());
    let s = svc(&db);
    let outcome = s
        .enqueue(&key, "manual", None, None, ConflictPolicy::Skip)
        .await
        .expect("enqueue");
    let run = outcome.enqueued().expect("已入队");

    assert!(
        run.scheduled_at.is_some(),
        "scheduled_at 记录入队时间 —— 少了它，中断回收就找不到这一行"
    );
    assert!(
        run.scheduled_at.expect("刚断言过") <= sm_db::common::time::now_utc(),
        "入队时刻不该是未来"
    );
}

// ================================================================ 常量对齐

#[tokio::test]
async fn the_failure_messages_match_upstream_verbatim() {
    // 客户端可能按文案做本地化映射，改字面量等于改契约
    assert_eq!(
        LEASE_EXPIRED_ERROR_MESSAGE,
        "任务租约过期，执行进程已中断，任务按失败回收"
    );
    assert_eq!(INTERRUPTED_ERROR_MESSAGE, "任务执行进程重启，执行已中断");
    assert_eq!(DEFAULT_LEASE_SECONDS, 300);
    assert_eq!(
        BOOTSTRAP_QUEUE_TASK_KEYS,
        ["gfriends_filetree_refresh", "movie_similarity_recompute"]
    );
    assert_eq!(FAILURE_CODE_QUEUE_LEASE_EXPIRED, "queue_lease_expired");
    assert_eq!(INTERNAL_FAILURE_CODE_KEY, "_failure_code");
}
