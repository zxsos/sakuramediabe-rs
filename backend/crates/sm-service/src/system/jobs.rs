//! 宿主的任务目录：上游 `JOB_REGISTRY` 在 HTTP 侧的投影。
//!
//! # 上游对应
//!
//! `src/api/routers/system/jobs.py` 从 `JOB_REGISTRY_BY_KEY` 查任务，再由
//! `src/schema/system/jobs.py` 的 `JobMetadataResource` 决定吐什么。本模块只放
//! **数据**：目录由组合根（它同时知道内建任务与插件任务）装配好后交给路由层。
//!
//! # 为什么是「快照」而不是活的注册表
//!
//! 插件的注册表在 `sm-plugins` 里、由组合根持有，而 `sm-api` 不能依赖
//! `sm-server`（反向依赖会成环）。所以这里放一份**纯数据**，组合根每次加载或
//! 重建插件表之后换一份新的。
//!
//! # 两处已知的缺字段（都不影响手动触发）
//!
//! - `cron_setting`：内建任务的那一半是 `scheduler.<field>` 形式的静态值
//!   （`movie_heat_cron` 之类），目前没带进 `JobSpec`，所以内建任务这里是
//!   `None`。插件任务那条路能算：`plugins.job_crons.<plugin_id>.<task_key>`。
//! - `params_schema`：只带「有没有」，不带 schema 正文（正文是 proto 的
//!   `google.protobuf.Struct`，转成 `serde_json::Value` 的那一层还没写）。

/// 手动触发的响应。上游 `schema/system/jobs.py:21-24`。
///
/// # 为什么放在这里（骨架期它被定义过**两次**，而且两份不一样）
///
/// | 位置 | 字段 |
/// |---|---|
/// | `catalog/movie_task.rs` | `{ task_run_id, task_name, trigger_type }` —— **上游没有后两个键** |
/// | `sm-api/routes/jobs.rs` | `{ task_run_id, task_key, state }` ✓ |
///
/// 前者的 `task_name` / `trigger_type` 是编的：`trigger_type` 在**请求**那一侧
/// 就定了（这个端点恒为 `manual`），回显它没有信息量；客户端真正要的是
/// `task_key`（拿它去查目录里的任务元数据）与 `state`（入队后的初始状态）。
///
/// 现在只留这一份，路由层 `use` 它。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManualJobTriggerResponse {
    /// 任务运行 id。**轮询它**看进度。
    pub task_run_id: i32,
    /// 任务键。
    pub task_key: String,
    /// 入队后的初始状态，手动触发恒为 `pending`（`queued` 是 worker 领取时那一步）。
    pub state: String,
}

/// 目录里的一项。字段与上游 `JobMetadataResource` 同名同义。
#[derive(Debug, Clone, PartialEq)]
pub struct JobCatalogEntry {
    pub task_key: String,
    pub log_name: String,
    pub cli_name: String,
    pub cli_help: String,
    /// 来源插件。内建任务为 `None`。
    pub plugin_id: Option<String>,
    /// 配置里覆盖 cron 用的键。见模块文档。
    pub cron_setting: Option<String>,
    /// 当前生效的 cron 表达式。`manual_only` 的任务为 `None`。
    pub cron_expr: Option<String>,
    pub manual_trigger_allowed: bool,
    /// 是否声明了参数 schema —— 决定「手动触发要不要带 body」。
    pub has_params_schema: bool,
}

/// 任务目录。**顺序 = 上游 `JOB_REGISTRY` 的顺序**（内建在前，插件按
/// `plugins.enabled`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobCatalog {
    entries: Vec<JobCatalogEntry>,
}

impl JobCatalog {
    pub fn new(entries: Vec<JobCatalogEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[JobCatalogEntry] {
        &self.entries
    }

    pub fn get(&self, task_key: &str) -> Option<&JobCatalogEntry> {
        self.entries.iter().find(|entry| entry.task_key == task_key)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(task_key: &str, cron: Option<&str>) -> JobCatalogEntry {
        JobCatalogEntry {
            task_key: task_key.to_owned(),
            log_name: task_key.to_owned(),
            cli_name: task_key.to_owned(),
            cli_help: task_key.to_owned(),
            plugin_id: None,
            cron_setting: None,
            cron_expr: cron.map(str::to_owned),
            manual_trigger_allowed: true,
            has_params_schema: false,
        }
    }

    #[test]
    fn lookup_by_task_key_and_missing_keys_are_none() {
        let catalog = JobCatalog::new(vec![entry("a", Some("* * * * *")), entry("b", None)]);
        assert_eq!(
            catalog.get("a").map(|e| e.cron_expr.as_deref()),
            Some(Some("* * * * *"))
        );
        // `manual_only` 的任务没有 cron 表达式，但**仍然要在目录里** ——
        // 否则手动触发会把它判成「未知任务」。
        assert_eq!(catalog.get("b").map(|e| e.cron_expr.clone()), Some(None));
        assert!(catalog.get("nope").is_none());
    }

    /// 手动触发响应**只有**上游那三个键。
    ///
    /// 骨架期那份多出来的 `task_name` / `trigger_type` 会把契约带偏：
    /// 客户端按 `task_key` 查目录、按 `state` 轮询，而多余的两个键永远不会
    /// 被读到 —— 直到有人以为「既然有 task_name 就不必再查目录了」。
    #[test]
    fn the_manual_trigger_response_has_exactly_three_keys() {
        let response = ManualJobTriggerResponse {
            task_run_id: 7,
            task_key: "movie_heat_update".to_owned(),
            state: "pending".to_owned(),
        };
        let value = serde_json::to_value(&response).expect("可序列化");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["state", "task_key", "task_run_id"]);
    }
}
