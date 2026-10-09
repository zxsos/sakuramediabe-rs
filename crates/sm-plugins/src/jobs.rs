//! 插件任务的注册表与加载期校验。
//!
//! # 上游对应
//!
//! `proto/plugin.proto` 的 `JobDefinition` / `RunJobRequest` / `JobEvent`。
//! 协议里把责任划得很清楚（都是 proto 注释的原话）：
//!
//! - `task_key`：**在全部内建任务与插件任务中唯一**；
//! - `default_cron`：五段式 cron，**宿主负责解析与调度，插件不再自带 cron 库**；
//! - `manual_only`：`true` 时不参与定时调度，只能人工触发；
//! - `required_capabilities`：**宿主据此做兼容性检查**。
//!
//! 本模块就是那几条约束的落点：注册时把「能不能调度」「能力够不够」算清楚，
//! 而不是等触发时才炸。
//!
//! # 为什么 cron 在加载期就要判死
//!
//! 上游 `JobDefinition._validate_cron_source` 用 `CronTrigger.from_crontab`
//! 验 `default_cron`，不合法直接抛 —— 也就是**加载插件时就拒绝**：坏插件连
//! 注册表都进不去，更不会拖垮调度器（上游的坏插件一律记进 `PLUGIN_LOAD_ERRORS`
//! 并隔离，见 `src/scheduler/registry.py:_build_job_registry`）。
//!
//! 判据用 `sm_core::crontab::is_valid_crontab`，它与调度求值共用同一套规则
//! （那个模块的文档写了为什么必须是同一个转换点），所以不会出现「加载期放行、
//! 调度期解析失败」。只数段数不够：`99 99 * * *` 是五段，却没有任何时刻匹配。
//!
//! # 进程/流式执行不在本模块
//!
//! `RunJob` 返回 `stream JobEvent`，且注释要求「实现必须支持取消：宿主会在超时
//! 或重启时直接断开流」。那属于调用层（含取消与超时），本模块只管声明。

use std::collections::HashMap;

use sm_plugin_api::v1::JobDefinition;

/// 一个插件任务的注册条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRegistration {
    /// 全局唯一（含内建任务）。
    pub task_key: String,
    pub log_name: String,
    pub cli_name: String,
    pub cli_help: String,
    /// 五段式 cron；`manual_only` 时不参与调度，但仍保留原值以便展示。
    pub default_cron: String,
    /// 只能人工触发。
    pub manual_only: bool,
    /// 是否声明了参数 schema —— 决定任务中心要不要渲染表单。
    pub has_params_schema: bool,
    /// 该任务需要的能力。
    pub required_capabilities: Vec<i32>,
    pub plugin_id: String,
}

impl JobRegistration {
    /// cron 是否能被宿主解析出下一次触发时刻。
    ///
    /// 与调度器用的是同一套规则（[`sm_core::crontab::is_valid_crontab`]），
    /// 所以「加载期通过」等价于「调度期也能编译」。
    pub fn cron_is_valid(&self) -> bool {
        sm_core::crontab::is_valid_crontab(&self.default_cron)
    }

    /// 是否可被定时调度：非 manual_only 且 cron 可解析。
    pub fn is_schedulable(&self) -> bool {
        !self.manual_only && self.cron_is_valid()
    }

    /// 插件声明的能力是否覆盖了这个任务需要的全部能力。
    pub fn capabilities_satisfied_by(&self, plugin_capabilities: &[i32]) -> bool {
        self.required_capabilities
            .iter()
            .all(|required| plugin_capabilities.contains(required))
    }

    /// 缺哪些能力（空 = 都满足）。
    pub fn missing_capabilities(&self, plugin_capabilities: &[i32]) -> Vec<i32> {
        self.required_capabilities
            .iter()
            .copied()
            .filter(|required| !plugin_capabilities.contains(required))
            .collect()
    }
}

/// 注册任务的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobProblem {
    /// `task_key` 与已注册的重复（含内建任务 —— 由调用方先灌入内建 key）。
    DuplicateTaskKey { key: String },
    /// 非 manual_only 却没给可解析的 cron。
    InvalidCron { key: String, cron: String },
    /// 插件没声明该任务需要的能力。
    MissingCapabilities { key: String, capabilities: Vec<i32> },
}

impl JobProblem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateTaskKey { .. } => "job_key_duplicated",
            Self::InvalidCron { .. } => "job_cron_invalid",
            Self::MissingCapabilities { .. } => "job_capability_missing",
        }
    }
}

/// 任务注册表。**顺序 = 注册顺序。**
#[derive(Debug, Clone, Default)]
pub struct JobRegistry {
    by_key: HashMap<String, JobRegistration>,
    order: Vec<String>,
    /// 内建任务的 key —— 插件不得占用。
    builtin_keys: Vec<String>,
}

impl JobRegistry {
    /// 用内建任务 key 建表。插件若声明了同名 key，注册即失败。
    pub fn with_builtin(builtin_keys: Vec<String>) -> Self {
        Self {
            builtin_keys,
            ..Default::default()
        }
    }

    /// 校验并注册一个任务。
    pub fn insert(
        &mut self,
        entry: JobRegistration,
        plugin_capabilities: &[i32],
    ) -> Vec<JobProblem> {
        let mut problems = Vec::new();

        if self.by_key.contains_key(&entry.task_key) || self.builtin_keys.contains(&entry.task_key)
        {
            problems.push(JobProblem::DuplicateTaskKey {
                key: entry.task_key.clone(),
            });
        }
        // 只有「本该被调度」的任务才要求 cron 合法。手动任务的 `default_cron`
        // 只是展示用（上游甚至不允许它出现），不拿来解析。
        if !entry.manual_only && !entry.cron_is_valid() {
            problems.push(JobProblem::InvalidCron {
                key: entry.task_key.clone(),
                cron: entry.default_cron.clone(),
            });
        }
        let missing = entry.missing_capabilities(plugin_capabilities);
        if !missing.is_empty() {
            problems.push(JobProblem::MissingCapabilities {
                key: entry.task_key.clone(),
                capabilities: missing,
            });
        }

        if problems.is_empty() {
            let key = entry.task_key.clone();
            self.order.push(key.clone());
            self.by_key.insert(key, entry);
        }
        problems
    }

    pub fn get(&self, key: &str) -> Option<&JobRegistration> {
        self.by_key.get(key)
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// 可被定时调度的任务（交给 `sm-scheduler`），**按注册顺序**。
    pub fn schedulable(&self) -> Vec<&JobRegistration> {
        self.order
            .iter()
            .filter_map(|key| self.by_key.get(key))
            .filter(|entry| entry.is_schedulable())
            .collect()
    }

    /// 只能人工触发的任务。
    pub fn manual_only(&self) -> Vec<&JobRegistration> {
        self.order
            .iter()
            .filter_map(|key| self.by_key.get(key))
            .filter(|entry| entry.manual_only)
            .collect()
    }
}

/// 把注册声明里的任务收进注册表。
///
/// 与 [`crate::loader::collect_providers`] 同构：只收声明，不管执行。
pub fn collect_jobs(
    registry: &mut JobRegistry,
    plugin_id: &str,
    jobs: &[JobDefinition],
    plugin_capabilities: &[i32],
) -> Vec<JobProblem> {
    let mut problems = Vec::new();
    for job in jobs {
        let entry = JobRegistration {
            task_key: job.task_key.clone(),
            log_name: job.log_name.clone(),
            cli_name: job.cli_name.clone(),
            cli_help: job.cli_help.clone(),
            default_cron: job.default_cron.clone(),
            manual_only: job.manual_only,
            has_params_schema: job.params_schema.is_some(),
            required_capabilities: job.required_capabilities.clone(),
            plugin_id: plugin_id.to_owned(),
        };
        problems.extend(registry.insert(entry, plugin_capabilities));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registration::capability;

    fn job(key: &str, cron: &str, manual_only: bool, required: Vec<i32>) -> JobDefinition {
        JobDefinition {
            task_key: key.to_owned(),
            log_name: key.to_owned(),
            cli_name: key.to_owned(),
            cli_help: String::new(),
            default_cron: cron.to_owned(),
            manual_only,
            params_schema: None,
            required_capabilities: required,
        }
    }

    #[test]
    fn a_schedulable_job_is_registered_and_listed() {
        let mut registry = JobRegistry::with_builtin(vec!["builtin.sync".to_owned()]);
        let problems = collect_jobs(
            &mut registry,
            "local",
            &[job("local.cleanup", "0 3 * * *", false, vec![])],
            &[],
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(registry.len(), 1);

        let schedulable = registry.schedulable();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].task_key, "local.cleanup");
        assert!(registry.manual_only().is_empty());
    }

    #[test]
    fn a_job_may_not_take_a_builtin_task_key() {
        let mut registry = JobRegistry::with_builtin(vec!["builtin.sync".to_owned()]);
        let problems = collect_jobs(
            &mut registry,
            "local",
            &[job("builtin.sync", "0 3 * * *", false, vec![])],
            &[],
        );
        assert_eq!(
            problems,
            vec![JobProblem::DuplicateTaskKey {
                key: "builtin.sync".to_owned()
            }]
        );
        // 校验失败就不该进表。
        assert!(registry.is_empty());
        assert_eq!(problems[0].code(), "job_key_duplicated");
    }

    #[test]
    fn a_scheduled_job_must_have_a_parseable_cron() {
        let mut registry = JobRegistry::default();
        // 四段：形状就不对 → 加载期就报，而不是等调度器炸。
        let problems = collect_jobs(
            &mut registry,
            "p",
            &[job("x", "0 3 * *", false, vec![])],
            &[],
        );
        assert_eq!(
            problems,
            vec![JobProblem::InvalidCron {
                key: "x".to_owned(),
                cron: "0 3 * *".to_owned()
            }]
        );
        assert!(registry.is_empty());

        // 五段但语义非法（没有时刻匹配得到）。只数段数的话这条会漏过去，
        // 然后在调度器构造时让**整个**调度器起不来 —— 而上游是隔离这一个插件。
        let mut registry = JobRegistry::default();
        let problems = collect_jobs(
            &mut registry,
            "p",
            &[job("x", "99 99 * * *", false, vec![])],
            &[],
        );
        assert_eq!(
            problems,
            vec![JobProblem::InvalidCron {
                key: "x".to_owned(),
                cron: "99 99 * * *".to_owned()
            }]
        );
        assert!(registry.is_empty());

        // manual_only 的任务不参与调度，cron 不用解析（上游根本不许它声明）。
        let mut registry = JobRegistry::default();
        let problems = collect_jobs(&mut registry, "p", &[job("y", "", true, vec![])], &[]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(registry.manual_only().len(), 1);
        assert!(registry.schedulable().is_empty());
    }

    #[test]
    fn required_capabilities_are_checked_against_the_plugin_declaration() {
        let mut registry = JobRegistry::default();
        // 任务要转存能力，但插件没声明。
        let problems = collect_jobs(
            &mut registry,
            "p",
            &[job(
                "z",
                "0 3 * * *",
                false,
                vec![capability::TRANSFER_TARGET],
            )],
            &[capability::DOWNLOAD],
        );
        assert_eq!(
            problems,
            vec![JobProblem::MissingCapabilities {
                key: "z".to_owned(),
                capabilities: vec![capability::TRANSFER_TARGET]
            }]
        );
        assert!(registry.is_empty());

        // 声明齐全就能注册。
        let mut registry = JobRegistry::default();
        let problems = collect_jobs(
            &mut registry,
            "p",
            &[job("z", "0 3 * * *", false, vec![capability::DOWNLOAD])],
            &[capability::DOWNLOAD],
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(registry.len(), 1);
    }
}
