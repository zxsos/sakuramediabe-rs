//! `Scheduler::tick_once` 的集成测试（真实 PostgreSQL）。
//!
//! # 逐条锁定的语义
//!
//! | 用例 | 断言 | 为什么重要 |
//! |---|---|---|
//! | 未到点 | 零入队 | 否则每轮都写库 |
//! | 到点 | **恰好一次** | 重复入队会让队列出现同 key 的多条 pending |
//! | 同轮再跑 | 零入队 | 时刻已推进，不该重复触发 |
//! | mutex 被占 | **跳过**（不报错） | 这就是 coalesce 语义 |
//! | 僵尸回收 | 先回收再入队 | 顺序反了说不通 |
//!
//! 任务键用固定字面量而不是每用例生成：每个测试有独立 schema，
//! `aps:<task_key>` 的唯一约束不会跨用例冲突。
//!
//! 上游出处：`src/start/aps.py:101-119`（`enqueue_scheduled_job`）与
//! `src/service/system/task_queue_service.py`（`conflict="skip"`）。

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun, TaskOutcome};
use sm_db::system::activity::task_state;
use sm_db::testing::TestDb;
use sm_scheduler::cron_spec::{JobSpec, RuntimeTimezone};
use sm_scheduler::tick::Scheduler;

const MINUTELY: &str = "tick_minutely";

/// 声明一个任务。键是运行期字符串（插件任务的键就是），所以统一走 `String`。
fn spec(task_key: &str, display_name: &str, cron: Option<&str>) -> JobSpec {
    JobSpec {
        task_key: task_key.to_owned(),
        display_name: display_name.to_owned(),
        cron: cron.map(str::to_owned),
    }
}

/// 每分钟触发的任务，便于把「下一次」算到可控的点。
fn minutely(task_key: &str) -> JobSpec {
    spec(task_key, "每分钟任务", Some("* * * * *"))
}

fn scheduler_with(db: &TestDb, jobs: Vec<JobSpec>) -> Scheduler {
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    Scheduler::with_timezone(repo, jobs, RuntimeTimezone::Utc, Duration::from_secs(1))
        .expect("注册表应当全部可解析")
}

/// 「该任务下一次触发之后 1 秒」—— 确定已到点的 `now`。
///
/// **必须读调度器自己的 `next_fire`**，不能另行用 `Utc::now()` 编译一份
/// 表达式来算：两次调用若跨过一个整分，算出的时刻会差一分钟，而调度器
/// 内部那次是权威的。用错基准的后果是「任务其实没到点」，测试却以为
/// 它该触发 —— 症状是断言 skipped 为空，而原因在测试自己。
fn due_now(scheduler: &Scheduler, task_key: &str) -> DateTime<Utc> {
    scheduler
        .next_fire_at(task_key)
        .expect("已注册的任务必然有下一次触发时刻")
        + chrono::Duration::seconds(1)
}

async fn count_for(db: &TestDb, task_key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM background_task_run WHERE task_key = $1")
        .bind(task_key)
        .fetch_one(db.pool())
        .await
        .expect("查询队列")
}

async fn count_pending(db: &TestDb, task_key: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM background_task_run WHERE task_key = $1 AND state = $2",
    )
    .bind(task_key)
    .bind(task_state::PENDING)
    .fetch_one(db.pool())
    .await
    .expect("查询队列")
}

/// 排一条同 `mutex_key` 的行，把互斥键占住。返回行 id。
async fn occupy_mutex(db: &TestDb, task_key: &str) -> i32 {
    BackgroundTaskRunRepository::new(db.pool().clone())
        .enqueue(&NewTaskRun {
            task_key: task_key.to_owned(),
            task_name: "占位".to_owned(),
            trigger_type: "manual".to_owned(),
            mutex_key: Some(format!("aps:{task_key}")),
            params: None,
            scheduled_at: None,
        })
        .await
        .expect("占位入队")
        .id
}

#[tokio::test]
async fn nothing_is_enqueued_before_the_fire_time() {
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);

    // 构造时的「现在」是真实时间，下一次触发在下一个整分，所以立刻
    // tick 必然不到点。
    let report = scheduler.tick_once(Utc::now()).await;
    assert!(report.is_noop(), "刚构造不该入队：{report:?}");
    assert_eq!(count_for(&db, MINUTELY).await, 0);
}

#[tokio::test]
async fn a_due_job_is_enqueued_exactly_once_and_not_again() {
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    let now = due_now(&scheduler, MINUTELY);

    let report = scheduler.tick_once(now).await;
    assert_eq!(
        report.enqueued,
        vec![MINUTELY],
        "应当恰好入队一次：{report:?}"
    );
    assert!(report.skipped.is_empty());
    assert_eq!(count_for(&db, MINUTELY).await, 1);

    // 同一时刻再跑：下次触发已推进到下一分钟，不该重复触发。
    let again = scheduler.tick_once(now).await;
    assert!(again.is_noop(), "同一时刻重复 tick 不该再入队：{again:?}");
    assert_eq!(count_for(&db, MINUTELY).await, 1);
}

#[tokio::test]
async fn the_enqueued_row_carries_the_upstream_shape() {
    // 入队的那一行必须与上游逐字段一致：trigger_type=scheduled、
    // mutex_key=aps:<task_key>、scheduled_at = 入队时刻。
    //
    // `scheduled_at` 这一项此前断言的是 `None`，而那**恰好是上游的反面**：
    // `src/service/system/activity/task_runs.py:161` 的
    // `BackgroundTaskRun.create(..., scheduled_at=now())` 写的是当前时刻，
    // 而 `task_queue_service` 的模块 docstring 也说「所有 task_run 都是队列
    // 托管行；scheduled_at 记录进入队列的时间」。
    //
    // 它不是可有可无的字段：`recover_interrupted_runs` 判定「上一个进程遗留
    // 的任务」靠的正是 `scheduled_at IS NOT NULL`。写 NULL 会让 cron 入队的
    // 行**永远**不被中断回收 —— 崩溃后只能等租约到期（最多 300 秒）才动。
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    let now = due_now(&scheduler, MINUTELY);
    scheduler.tick_once(now).await;

    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            Option<chrono::NaiveDateTime>,
        ),
    >(
        "SELECT task_key, task_name, trigger_type, mutex_key, scheduled_at \
         FROM background_task_run WHERE task_key = $1",
    )
    .bind(MINUTELY)
    .fetch_one(db.pool())
    .await
    .expect("读回刚入队的行");

    assert_eq!(row.0, MINUTELY);
    assert_eq!(row.1, "每分钟任务");
    assert_eq!(row.2, "scheduled", "上游 cron 触发的 trigger_type");
    assert_eq!(row.3.as_deref(), Some("aps:tick_minutely"));
    let scheduled_at = row.4.expect("scheduled_at 记录入队时间，不该是 NULL");
    assert!(
        scheduled_at <= sm_db::common::time::now_utc(),
        "scheduled_at 是入队时刻，不该在未来：{scheduled_at:?}"
    );
}

#[tokio::test]
async fn an_occupied_mutex_key_is_skipped_not_an_error() {
    let db = TestDb::require().await;
    let blocking_id = occupy_mutex(&db, MINUTELY).await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);

    let report = scheduler.tick_once(due_now(&scheduler, MINUTELY)).await;
    assert_eq!(report.skipped, vec![MINUTELY], "互斥键被占 → 跳过");
    assert!(report.enqueued.is_empty());
    assert!(report.failed.is_empty(), "跳过**不是**失败：{report:?}");
    assert_eq!(count_for(&db, MINUTELY).await, 1, "只有占位那一条");

    // 占位行仍是 pending：tick 不该动别人的行。
    let blocker = BackgroundTaskRunRepository::new(db.pool().clone())
        .find_by_id(blocking_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocker.state, task_state::PENDING);
    assert_eq!(blocker.mutex_key.as_deref(), Some("aps:tick_minutely"));
}

#[tokio::test]
async fn a_finished_job_releases_its_mutex_key_so_the_next_one_can_enqueue() {
    // 这是 sm-db 仓储文档里那张表的第三行：不释放互斥键的话，
    // 「定时任务第二次触发」会永久撞唯一约束。
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let blocking_id = occupy_mutex(&db, MINUTELY).await;

    let claimed = repo
        .claim(chrono::Duration::seconds(300))
        .await
        .unwrap()
        .expect("应当能领取到占位任务");
    assert_eq!(claimed.run.id, blocking_id);
    repo.finish(claimed.run.id, &TaskOutcome::default())
        .await
        .expect("完成");
    let after = repo.find_by_id(blocking_id).await.unwrap().unwrap();
    assert!(
        after.mutex_key.is_none(),
        "完成后必须释放互斥键，否则同 key 永久无法再入队"
    );

    // 现在 tick 就能入队了。
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    let report = scheduler.tick_once(due_now(&scheduler, MINUTELY)).await;
    assert_eq!(report.enqueued, vec![MINUTELY], "{report:?}");
    assert_eq!(count_pending(&db, MINUTELY).await, 1);
}

#[tokio::test]
async fn stale_leases_are_reclaimed_before_enqueueing() {
    // 顺序：先回收僵尸，再入队。否则一个刚被回收的任务会在同一轮里
    // 既被重排又被重新入队，读日志时说不清是哪一步造成的。
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let run = repo
        .enqueue(&NewTaskRun {
            task_key: MINUTELY.to_owned(),
            task_name: "僵尸".to_owned(),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            params: None,
            scheduled_at: None,
        })
        .await
        .unwrap();
    let claimed = repo
        .claim(chrono::Duration::seconds(300))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, run.id);
    // 把租约改到过去 —— `renew_lease` 只会往未来延，所以直接改库。
    sqlx::query("UPDATE background_task_run SET lease_expires_at = $2 WHERE id = $1")
        .bind(run.id)
        .bind(Utc::now().naive_utc() - chrono::Duration::hours(1))
        .execute(db.pool())
        .await
        .unwrap();

    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    let report = scheduler.tick_once(Utc::now()).await;

    assert_eq!(report.reclaimed, 1, "应当回收 1 个僵尸任务");
    let after = repo.find_by_id(run.id).await.unwrap().unwrap();
    assert_eq!(
        after.state,
        task_state::PENDING,
        "回收是回到 pending 而不是直接判失败 —— 任务可能已部分执行"
    );
    assert!(after.lease_expires_at.is_none());
}

#[tokio::test]
async fn a_manual_only_job_never_enqueues() {
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![spec(MINUTELY, "只能手动触发", None)]);
    // 手动任务根本不进调度表，于是 task_keys 里没有它。
    assert!(scheduler.task_keys().is_empty());
    for _ in 0..3 {
        let report = scheduler
            .tick_once(Utc::now() + chrono::Duration::hours(1))
            .await;
        assert!(report.is_noop(), "{report:?}");
    }
    assert_eq!(count_for(&db, MINUTELY).await, 0);
}

#[tokio::test]
async fn an_invalid_cron_fails_at_construction_not_at_tick() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let err = Scheduler::with_timezone(
        repo,
        vec![spec("broken", "坏 cron", Some("99 99 99"))],
        RuntimeTimezone::Utc,
        Duration::from_secs(1),
    )
    .expect_err("非法 cron 应在构造时失败，而不是每分钟 panic");
    assert_eq!(err.task_key, "broken");
}

#[tokio::test]
async fn the_full_builtin_registry_constructs_and_fires_nothing_immediately() {
    // 19 个任务全部注册成功（16 个带 cron），且构造那一刻没有任何一个
    // 到点 —— 除非某个表达式被转换成了「立刻触发」。
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let scheduler = Scheduler::with_timezone(
        repo,
        sm_scheduler::builtin_jobs(),
        RuntimeTimezone::Utc,
        Duration::from_secs(1),
    )
    .expect("全部内建 cron 都应可解析");
    assert_eq!(scheduler.task_keys().len(), 16, "16 个带 cron 的任务");

    let report = scheduler.tick_once(Utc::now()).await;
    assert!(report.is_noop(), "刚构造不该触发任何任务：{report:?}");
    assert_eq!(
        count_for(&db, "download_task_sync").await,
        0,
        "每分钟任务也不该在构造当刻触发"
    );
}

#[tokio::test]
async fn a_downtime_longer_than_the_cron_period_enqueues_once_not_once_per_minute() {
    // coalesce 的另一半语义：停机 1 小时后恢复，**只**入队一次，
    // 而不是把错过的 60 次补跑回来。实现上靠「推进到 now 之后的下一次」。
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    let now = due_now(&scheduler, MINUTELY) + chrono::Duration::hours(1);

    let report = scheduler.tick_once(now).await;
    assert_eq!(report.enqueued, vec![MINUTELY], "{report:?}");
    assert_eq!(count_for(&db, MINUTELY).await, 1, "补跑 1 次而不是 60 次");
}

#[tokio::test]
async fn a_task_key_absent_from_the_registry_is_never_enqueued() {
    // 注册表里没有的 key 不该凭空出现 —— 反过来证明 tick 只认注册表。
    let db = TestDb::require().await;
    let scheduler = scheduler_with(&db, vec![minutely(MINUTELY)]);
    scheduler
        .tick_once(due_now(&scheduler, MINUTELY) + chrono::Duration::days(1))
        .await;
    assert_eq!(count_for(&db, "not_a_registered_task").await, 0);
    assert_eq!(count_for(&db, "library_import").await, 0);
}

#[tokio::test]
async fn the_fire_time_is_utc_based_even_for_a_daily_job() {
    // UTC+8 下「本地 00:15」= UTC 前一天 16:15。锁住这个换算。
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let scheduler = Scheduler::with_timezone(
        repo,
        vec![spec("daily_heat", "每日热度", Some("15 0 * * *"))],
        RuntimeTimezone::FixedUtcOffset(8 * 3600),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(scheduler.timezone_name(), "fixed-offset");

    // 触发时刻读调度器自己的 `next_fire`，**不按日历推日期**。构造时那一次
    // 是按真实 `Utc::now()` 算的，而「现在落在 UTC 的哪一段」决定它算出来的
    // 那一刻在 UTC 的哪一天：本地 00:15 之前的半天里，下一次是**今天**
    // 16:15Z；之后才是明天。写死日期会在那天过后变红，按日历推则会在每天
    // UTC 00:00–16:00 之间变红 —— 症状都像调度算错了，原因却在用例自己。
    //
    // （也不要为此去放宽调度器的「陈旧触发」判定：忽略过于陈旧的触发本身
    // 就是它该有的行为。）
    let fire = scheduler
        .next_fire_at("daily_heat")
        .expect("已注册的任务必然有下一次触发时刻");
    // 换算的两侧都锁住：东八区墙钟是本地 00:15，同一瞬间在 UTC 是前一天 16:15。
    assert_eq!(
        fire.with_timezone(&FixedOffset::east_opt(8 * 3600).expect("东八区"))
            .time()
            .to_string(),
        "00:15:00",
        "本地（UTC+8）00:15"
    );
    assert_eq!(fire.time().to_string(), "16:15:00", "UTC 侧是前一天 16:15");

    let report = scheduler
        .tick_once(fire - chrono::Duration::seconds(1))
        .await;
    assert!(report.is_noop(), "差 1 秒还没到点：{report:?}");

    let report = scheduler.tick_once(fire).await;
    assert_eq!(report.enqueued, vec!["daily_heat"], "{report:?}");
}
