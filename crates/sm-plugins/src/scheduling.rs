//! 插件任务接进 cron 触发。
//!
//! # 上游对应
//!
//! `src/start/aps.py:build_scheduler`：遍历 `JOB_REGISTRY`，跳过
//! `manual_only` 的任务，其余按 `resolve_job_cron_expr` 取 cron 挂成 APS 的
//! cron job —— **只入队**，执行在 worker。本模块只做那一步的映射：
//! 注册表的声明 → [`sm_scheduler::JobSpec`]。
//!
//! # 只有声明，没有执行
//!
//! 到点入队是 `sm-scheduler` 的事，执行（调 `PluginControl.RunJob`）在 worker
//! 侧。所以这里**不**持有 gRPC 客户端，也不碰 `run_job` —— 调度与执行之间
//! 只有 `background_task_run` 这一行队列状态。
//!
//! # 组合根还没接上
//!
//! `sm-server` 目前刻意不依赖 `sm-plugins`（插件进程的生命周期还没落地），
//! 所以这一层先在集成测试里被驱动。等插件能被拉起时，把
//! [`scheduler_specs`] 的结果并进 `sm_scheduler::builtin_jobs()` 即可 ——
//! 两边都是 `JobSpec`，调度器不区分来源。

use sm_scheduler::JobSpec;

use crate::jobs::JobRegistry;

/// 把注册表里可定时调度的任务转成调度器能消费的声明。
///
/// # 只交「本该被调度」的那部分
///
/// `manual_only` 的任务不出现在这里 —— 上游 `build_scheduler` 同样跳过它们：
/// 它们只能由任务中心 / CLI 带参数触发，挂到 cron 上没有意义。
///
/// 反过来，这里每一项都**保证**能被调度器编译：`default_cron` 在注册期就用
/// 同一套规则验过（[`crate::jobs::JobRegistration::cron_is_valid`]），所以
/// 某个插件填错 cron 不会让整个调度器起不来 —— 上游是隔离那一个插件。
///
/// # 展示名取 `cli_help`
///
/// 上游 `resolve_job_task_name` 是 `TASK_NAME_REGISTRY.get(task_key) or
/// cli_help`；插件任务的 `task_key` 不在那张内建表里，于是落到 `cli_help`
/// 这一路。
///
/// # 刻意不带 `plugin_id`
///
/// worker 执行时按 `task_key` 回查注册表就能拿到归属插件。声明里再存一份
/// 就是同一事实的两处状态，改一处漏一处会指向不同的插件。
pub fn scheduler_specs(registry: &JobRegistry) -> Vec<JobSpec> {
    registry
        .schedulable()
        .into_iter()
        .map(|entry| JobSpec {
            task_key: entry.task_key.clone(),
            display_name: entry.cli_help.clone(),
            cron: Some(entry.default_cron.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::collect_jobs;
    use sm_plugin_api::v1::JobDefinition;

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

    #[test]
    fn schedulable_jobs_become_specs_carrying_cli_help_and_default_cron() {
        let mut registry = JobRegistry::default();
        let problems = collect_jobs(
            &mut registry,
            "local",
            &[
                job("local.cleanup", "清理本地缓存", "0 3 * * *", false),
                job("local.migrate", "迁移旧目录", "0 4 * * *", true),
            ],
            &[],
        );
        assert!(problems.is_empty(), "{problems:?}");

        let specs = scheduler_specs(&registry);
        // manual_only 那条不在里面：上游 build_scheduler 同样跳过。
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].task_key, "local.cleanup");
        assert_eq!(specs[0].display_name, "清理本地缓存", "展示名是 cli_help");
        assert_eq!(specs[0].cron.as_deref(), Some("0 3 * * *"));
    }

    #[test]
    fn specs_keep_the_registration_order() {
        // 顺序是注册顺序：启动日志（`cron_info`）按这个顺序打，运维据此核对。
        let mut registry = JobRegistry::default();
        for key in ["b.sync", "a.sync"] {
            let problems = collect_jobs(
                &mut registry,
                "p",
                &[job(key, key, "* * * * *", false)],
                &[],
            );
            assert!(problems.is_empty(), "{problems:?}");
        }
        let specs = scheduler_specs(&registry);
        let keys: Vec<&str> = specs.iter().map(|s| s.task_key.as_str()).collect();
        assert_eq!(keys, vec!["b.sync", "a.sync"]);
    }
}
