//! 插件任务经 cron 触发入队的集成测试（真实 PostgreSQL）。
//!
//! # 逐条锁定的语义
//!
//! | 用例 | 断言 | 为什么重要 |
//! |---|---|---|
//! | 到点 | 入队一行，形状与内建任务一致 | 「接进 cron」的落点 |
//! | 时区 | 按**运行时时区**求值，不是 UTC | proto 写死「宿主负责解析与调度，插件不再自带 cron 库」 |
//! | 同场共存 | 内建 + 插件合成一份注册表 | 组合根将来的装配方式 |
//!
//! 上游出处：`src/start/aps.py:345-364`（`build_scheduler`）与
//! `src/scheduler/contracts.py:41-60`（`_validate_cron_source`）。
//!
//! # 为什么这个测试住在 `sm-server` 而不是 `sm-plugins`
//!
//! 它同时用到**注册表**（`sm-plugins`）、**调度器**（`sm-scheduler`）与
//! **组合根的映射**（`sm_server::plugins::job_specs`）。放在插件侧就要给
//! `sm-plugins` 加一条 `→ sm-scheduler` 的依赖边，而那条边会把 `sm-service`
//! 卷进依赖环。组合根是唯一同时看得见三方又不产生环的位置。

use std::time::Duration;

use sm_db::repo::BackgroundTaskRunRepository;
use sm_db::system::activity::{task_state, QUEUE_MUTEX_PREFIX};
use sm_db::testing::TestDb;
use sm_plugin_api::v1::JobDefinition;
use sm_plugins::jobs::{collect_jobs, JobRegistry};
use sm_scheduler::tick::Scheduler;
use sm_scheduler::RuntimeTimezone;
use sm_server::plugins::job_specs;

fn job(task_key: &str, cli_help: &str, default_cron: &str, manual_only: bool) -> JobDefinition {
    JobDefinition {
        task_key: task_key.to_owned(),
        log_name: task_key.to_owned(),
        cli_name: task_key.to_owned(),
        cli_help: cli_help.to_owned(),
        default_cron: default_cron.to_owned(),
        manual_only,
        params_schema: None,
        required_capabilities: Vec::new(),
    }
}

/// 注册一个插件的两个任务：一个可调度、一个 `manual_only`。
fn registry_with_two_jobs() -> JobRegistry {
    let mut registry = JobRegistry::with_builtin(vec!["download_task_sync".to_owned()]);
    let problems = collect_jobs(
        &mut registry,
        "local",
        &[
            job("local.cleanup", "清理本地缓存", "* * * * *", false),
            job("local.migrate", "迁移旧目录", "* * * * *", true),
        ],
        &[],
    );
    assert!(problems.is_empty(), "{problems:?}");
    registry
}

/// 没有配置覆盖时，调度声明直接取插件声明的 `default_cron`。
fn specs_of(registry: &JobRegistry) -> Vec<sm_scheduler::JobSpec> {
    job_specs(registry, &|_, _| None)
}

fn scheduler_with(db: &TestDb, registry: &JobRegistry) -> Scheduler {
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    Scheduler::with_timezone(
        repo,
        specs_of(registry),
        RuntimeTimezone::Utc,
        Duration::from_secs(1),
    )
    .expect("注册期验过 cron，这里必然可解析")
}

#[tokio::test]
async fn a_plugin_job_is_enqueued_at_its_cron_time_like_a_builtin_one() {
    let db = TestDb::require().await;
    let registry = registry_with_two_jobs();
    let scheduler = scheduler_with(&db, &registry);

    // `manual_only` 那条不进调度表 —— 上游 build_scheduler 同样跳过。
    assert_eq!(scheduler.task_keys(), vec!["local.cleanup"]);

    let fire = scheduler
        .next_fire_at("local.cleanup")
        .expect("已注册的任务必然有下一次触发时刻");
    let report = scheduler.tick_once(fire).await;
    assert_eq!(report.enqueued, vec!["local.cleanup"], "{report:?}");

    // 入队那一行的形状必须与内建任务完全一致：同一个互斥键前缀、同一个
    // trigger_type，否则 worker 侧要按来源分叉处理。
    let row = BackgroundTaskRunRepository::new(db.pool().clone())
        .find_by_mutex_key(&format!("{QUEUE_MUTEX_PREFIX}local.cleanup"))
        .await
        .expect("查询队列")
        .expect("应当入队一行");
    assert_eq!(row.task_key, "local.cleanup");
    assert_eq!(row.task_name, "清理本地缓存", "展示名取 cli_help");
    assert_eq!(row.trigger_type, "scheduled");
    assert_eq!(row.state, task_state::PENDING);
}

#[tokio::test]
async fn a_plugin_cron_is_evaluated_in_the_runtime_timezone() {
    // 「每天本地 00:15」而不是「每天 UTC 00:15」—— 宿主解析，插件不自带
    // cron 库，于是它必然跟着宿主的运行时时区走。
    let db = TestDb::require().await;
    let mut registry = JobRegistry::default();
    let problems = collect_jobs(
        &mut registry,
        "local",
        &[job("local.heat", "本地热度重算", "15 0 * * *", false)],
        &[],
    );
    assert!(problems.is_empty(), "{problems:?}");

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let scheduler = Scheduler::with_timezone(
        repo,
        specs_of(&registry),
        RuntimeTimezone::FixedUtcOffset(8 * 3600),
        Duration::from_secs(1),
    )
    .expect("插件 cron 应当可解析");

    let fire = scheduler
        .next_fire_at("local.heat")
        .expect("已注册的任务必然有下一次触发时刻");
    // 东八区的本地 00:15 = UTC 前一天 16:15。
    assert_eq!(fire.time().to_string(), "16:15:00", "{fire:?}");
}

#[tokio::test]
async fn plugin_jobs_sit_next_to_builtin_jobs_in_one_scheduler() {
    // 组合根将来的装配方式：内建与插件合成**一份**注册表 —— 调度器不区分
    // 来源，两者共用同一套到点判定与 coalesce。
    let db = TestDb::require().await;
    let registry = registry_with_two_jobs();
    let mut specs = sm_scheduler::builtin_jobs();
    specs.extend(specs_of(&registry));

    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let scheduler =
        Scheduler::with_timezone(repo, specs, RuntimeTimezone::Utc, Duration::from_secs(1))
            .expect("全部 cron 都应可解析");

    assert_eq!(
        scheduler.task_keys().len(),
        17,
        "16 个内建 + 1 个插件（手动那条不算）：{:?}",
        scheduler.task_keys()
    );
    assert!(scheduler.task_keys().contains(&"local.cleanup".to_owned()));
    // 插件不得占用内建键（注册期已拒），所以互斥键也各自独立 ——
    // 否则两个来源会共用 `aps:<task_key>` 互相顶掉。
    assert!(
        !scheduler.task_keys().contains(&"local.migrate".to_owned()),
        "manual_only 不进调度表"
    );
}
