//! 组合根里的插件装配：按配置拉起插件、收三张注册表、交给调度器。
//!
//! # 上游对应
//!
//! 上游是**进程内 import**（`load_enabled_plugins`），拉起与注册都在同一行里发生。
//! 这里换成进程模型，所以装配多出两步：拉起（[`sm_plugins::supervisor::launch`]）
//! 与看门狗。协议见 `docs/adr/2026-10-05-plugin-lifecycle.md`。
//!
//! # 可执行文件的位置是**约定**，不是配置
//!
//! `<root_dir>/<plugin_id>/<plugin_id>`。`plugins` 节是 `sm_core::config_schema`
//! 里与上游**逐字段对齐**的四个键（`root_dir` / `enabled` / `job_crons` /
//! `settings`），加一个字段就要动那张表和它的对齐测试；而上游根本没有
//! 「启动命令」这个概念（它是 import）。所以用约定而不是配置。
//!
//! # cron 覆盖：与上游同一条规则
//!
//! `resolve_job_cron_expr`（`src/start/aps.py`）：插件任务先读
//! `plugins.job_crons[plugin_id][task_key]`，缺省才回退 `default_cron`。
//!
//! # 看门狗能做什么、不能做什么
//!
//! 能做：发现崩溃 → 退避重启 → **重建**三张注册表（provider / 任务 / 扩展点）。
//! 不能做：把重启后的任务重新挂进调度器 —— `sm_scheduler::Scheduler` 的任务清单
//! 在构造时定死。所以重启后「这个插件新加的 cron 任务」要等下次进程启动才生效；
//! 已经在调度表里的那些不受影响（入队照旧，能不能跑取决于插件活着没有）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sm_plugin_api::v1::RegisterResponse;
use sm_plugins::extensions::{collect_extensions, ExtensionRegistry};
use sm_plugins::jobs::{collect_jobs, JobProblem, JobRegistry};
use sm_plugins::loader::collect_providers;
use sm_plugins::registry::ProviderRegistry;
use sm_plugins::supervisor::{launch, restart_backoff, LaunchSpec, PluginProcess};
use sm_scheduler::JobSpec;
use sm_service::system::{JobCatalog, JobCatalogEntry};

/// 等插件就绪的上限。
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// 看门狗的轮询间隔。
///
/// 1 秒：插件崩溃是低频事件，而 `wait()` 要可变借用，轮询比 `select!` 简单得多。
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
/// 重启退避：基数与上限（指数增长，见 [`restart_backoff`]）。
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 从配置快照里读出的插件装配输入。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginConfig {
    pub root_dir: PathBuf,
    /// `plugins.enabled` —— 顺序即优先级（provider 与兜底链路都按它）。
    pub enabled: Vec<String>,
    /// `plugins.settings`：`plugin_id -> 私有配置`。
    pub settings: Map<String, Value>,
    /// `plugins.job_crons`：`plugin_id -> (task_key -> cron)`。
    pub job_crons: Map<String, Value>,
}

impl PluginConfig {
    /// 从配置快照读。`plugins` 是只读键（含凭据），只从这里读，不进 API 响应。
    pub fn from_snapshot(snapshot: &Value) -> Self {
        let plugins = snapshot.get("plugins");
        let root_dir = plugins
            .and_then(|section| section.get("root_dir"))
            .and_then(Value::as_str)
            .unwrap_or("/data/plugins");
        let enabled = plugins
            .and_then(|section| section.get("enabled"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            root_dir: PathBuf::from(root_dir),
            enabled,
            settings: object_at(plugins, "settings"),
            job_crons: object_at(plugins, "job_crons"),
        }
    }

    /// 插件可执行文件：`<root_dir>/<plugin_id>/<plugin_id>`。
    pub fn program_for(&self, plugin_id: &str) -> PathBuf {
        self.root_dir.join(plugin_id).join(plugin_id)
    }

    /// 插件数据目录：`<root_dir>/<plugin_id>/data`。宿主保证存在且重装时保留。
    pub fn data_dir_for(&self, plugin_id: &str) -> PathBuf {
        self.root_dir.join(plugin_id).join("data")
    }

    /// 该插件的私有配置。**没有**就是不给（不写配置文件）。
    pub fn settings_for(&self, plugin_id: &str) -> Option<Value> {
        self.settings.get(plugin_id).cloned()
    }

    /// 该任务的 cron 覆盖值。上游 `resolve_job_cron_expr`：覆盖优先于 `default_cron`。
    pub fn cron_override(&self, plugin_id: &str, task_key: &str) -> Option<&str> {
        self.job_crons
            .get(plugin_id)
            .and_then(|overrides| overrides.get(task_key))
            .and_then(Value::as_str)
    }

    fn launch_spec(&self, plugin_id: &str) -> LaunchSpec {
        LaunchSpec {
            plugin_id: plugin_id.to_owned(),
            // manifest 还没落地（上游 manifest.json 在插件目录里），所以这里
            // 以「插件 id = 目录名」为准 —— 与 `program_for` 的约定同源。
            manifest_id: plugin_id.to_owned(),
            program: self.program_for(plugin_id).display().to_string(),
            args: Vec::new(),
            data_dir: self.data_dir_for(plugin_id),
            settings: self.settings_for(plugin_id),
            settings_path: None,
            ready_timeout: READY_TIMEOUT,
        }
    }
}

fn object_at(section: Option<&Value>, field: &str) -> Map<String, Value> {
    section
        .and_then(|section| section.get(field))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// 一个已加载的插件。
#[derive(Debug)]
pub struct LoadedPlugin {
    pub plugin_id: String,
    /// `Register` 的回显 —— 重建三张注册表时还要用它。
    pub registration: RegisterResponse,
    pub process: PluginProcess,
    /// 已重启次数（退避用）。
    pub restarts: u32,
}

/// 全部已加载的插件与它们贡献的三张注册表。
#[derive(Debug)]
pub struct Plugins {
    config: PluginConfig,
    loaded: Vec<LoadedPlugin>,
    providers: ProviderRegistry,
    jobs: JobRegistry,
    extensions: ExtensionRegistry,
}

impl Plugins {
    /// 按配置逐个拉起插件。**单个插件失败不影响其它插件** —— 上游把坏插件记进
    /// `PLUGIN_LOAD_ERRORS` 并隔离，这里是「记一条 warn 然后跳过」。
    pub async fn load(config: PluginConfig) -> Self {
        let builtin_task_keys: Vec<String> = sm_scheduler::builtin_jobs()
            .into_iter()
            .map(|spec| spec.task_key)
            .collect();
        let mut plugins = Self {
            config,
            loaded: Vec::new(),
            providers: ProviderRegistry::new(),
            jobs: JobRegistry::with_builtin(builtin_task_keys),
            extensions: ExtensionRegistry::new(),
        };
        // `enabled` 的顺序就是优先级，不能并发拉起来打乱它。
        for plugin_id in plugins.config.enabled.clone() {
            match launch(&plugins.config.launch_spec(&plugin_id)).await {
                Ok(launched) => {
                    tracing::info!(plugin_id, "插件已就绪");
                    plugins.admit(plugin_id, launched.registration, launched.process);
                }
                Err(err) => {
                    // 起不来就跳过：一个坏插件不该让整个后端起不来
                    // （上游 `PLUGIN_LOAD_ERRORS` 是同一个意思）。
                    tracing::warn!(
                        plugin_id,
                        code = err.code(),
                        error = %format_args!("{err:?}"),
                        "插件加载失败，已跳过"
                    );
                }
            }
        }
        plugins
    }

    fn admit(&mut self, plugin_id: String, registration: RegisterResponse, process: PluginProcess) {
        // 一个插件可以有多个 provider，多个插件各有一张表 —— 合进宿主那一张，
        // 顺序由 `insert` 按 `enabled` 顺序续在后面。
        for entry in collect_providers(&registration).entries() {
            self.providers.insert(entry.clone());
        }
        self.collect(&registration);
        self.loaded.push(LoadedPlugin {
            plugin_id,
            registration,
            process,
            restarts: 0,
        });
    }

    /// 把一个注册响应里的任务与扩展点收进注册表。
    fn collect(&mut self, registration: &RegisterResponse) {
        for problem in collect_jobs(
            &mut self.jobs,
            &registration.plugin_id,
            &registration.jobs,
            &registration.capabilities,
        ) {
            self.warn_job_problem(&problem);
        }
        for problem in collect_extensions(&mut self.extensions, registration) {
            tracing::warn!(
                plugin_id = registration.plugin_id.as_str(),
                code = problem.code(),
                "插件扩展点声明被拒：{problem:?}"
            );
        }
    }

    fn warn_job_problem(&self, problem: &JobProblem) {
        tracing::warn!(code = problem.code(), "插件任务声明被拒：{problem:?}");
    }

    /// 交给调度器的声明：内建任务 + 全部插件的可调度任务。
    pub fn scheduler_specs(&self) -> Vec<JobSpec> {
        let specs: Vec<JobSpec> = self
            .jobs
            .schedulable()
            .into_iter()
            .map(|entry| {
                let cron = self
                    .config
                    .cron_override(&entry.plugin_id, &entry.task_key)
                    .map(str::to_owned)
                    .unwrap_or_else(|| entry.default_cron.clone());
                JobSpec {
                    task_key: entry.task_key.clone(),
                    log_name: entry.log_name.clone(),
                    cli_name: entry.cli_name.clone(),
                    display_name: entry.cli_help.clone(),
                    cron: Some(cron),
                    manual_trigger_allowed: true,
                }
            })
            .collect();
        // `schedulable()` 已经排掉了 `manual_only` —— 它们没有 cron，交给调度器
        // 也只会变成「永不触发」的条目。
        specs
    }

    /// 任务目录：内建任务 + 全部插件任务（**含 `manual_only`**）。
    ///
    /// 交给路由层的 `AppState`，供 `POST /system/jobs/{task_key}/run` 判断
    /// 「这个 key 存不存在、能不能手动触发」。
    pub fn catalog(&self) -> JobCatalog {
        let mut entries: Vec<JobCatalogEntry> = sm_scheduler::builtin_jobs()
            .into_iter()
            .map(|spec| JobCatalogEntry {
                task_key: spec.task_key,
                log_name: spec.log_name,
                cli_name: spec.cli_name,
                cli_help: spec.display_name,
                plugin_id: None,
                // 内建任务的 `cron_setting`（`movie_heat_cron` 那一串）还没带进
                // `JobSpec`，见 `sm_service::system::jobs` 的模块文档。
                cron_setting: None,
                cron_expr: spec.cron,
                manual_trigger_allowed: spec.manual_trigger_allowed,
                has_params_schema: false,
            })
            .collect();

        for entry in self.jobs.entries() {
            let cron = if entry.manual_only {
                None
            } else {
                Some(
                    self.config
                        .cron_override(&entry.plugin_id, &entry.task_key)
                        .map(str::to_owned)
                        .unwrap_or_else(|| entry.default_cron.clone()),
                )
            };
            entries.push(JobCatalogEntry {
                task_key: entry.task_key.clone(),
                log_name: entry.log_name.clone(),
                cli_name: entry.cli_name.clone(),
                cli_help: entry.cli_help.clone(),
                plugin_id: Some(entry.plugin_id.clone()),
                // 上游 `get_job_cron_setting`：插件任务的覆盖键长这个样子。
                cron_setting: Some(format!(
                    "plugins.job_crons.{}.{}",
                    entry.plugin_id, entry.task_key
                )),
                cron_expr: cron,
                manual_trigger_allowed: true,
                has_params_schema: entry.has_params_schema,
            });
        }
        JobCatalog::new(entries)
    }

    /// 把插件注册表里的**排行源**转成 `sm-service` 的快照类型。
    ///
    /// 交给路由层的 `AppState`，供 `GET /ranking-sources` 用。
    ///
    /// # 为什么这个转换必须发生在**组合根**
    ///
    /// `sm-plugins -> sm-scheduler -> sm-service` 已经是一条依赖链 ——
    /// `sm-service` 再依赖 `sm-plugins` 就成环；`sm-api` 同样不依赖它。
    /// 所以读注册表这件事只有 `sm-server` 能做（它依赖全部 crate）。
    ///
    /// 这与 `catalog()`（`JobRegistry` -> `JobCatalog`）是**同一个模式**，
    /// 不是新发明 —— `AppState::jobs` 就是这么来的。
    ///
    /// # 只取 `source_key` 与 `title`，**榜单定义是空的**
    ///
    /// 榜单定义（`supported_periods` / `default_period` / `descending`）来自
    /// 插件**加载期**的注册载荷（上游
    /// `register_plugin_ranking_sources(accepted, owners)`，
    /// `ranking_plugin_adapter.py:109`）。而
    /// `ProviderRegistration` 只有 `provider_key` / `display_name` /
    /// `plugin_id` / `capabilities` / `data_plane_endpoint` —— **没有 boards**。
    ///
    /// 所以这里产出的是**空 boards**。后果是
    /// `GET /ranking-sources/{key}/boards` 会返回 404
    /// `ranking_board_definitions_unavailable`（service 层显式报错，不是空数组）。
    ///
    /// **不填一个假的 boards 列表** —— 那会让接口「成功」但周期校验永远失败，
    /// 比报缺口更难查。两条修法记在
    /// `sm_service::discovery::ranking::RankingSourceCatalog` 的文档里。
    pub fn ranking_sources(&self) -> sm_service::discovery::ranking::RankingSourceCatalog {
        use sm_plugins::registration::EXTENSION_RANKING_SOURCE;
        let entries = self
            .providers
            .providers_with(EXTENSION_RANKING_SOURCE)
            .into_iter()
            .map(|provider| sm_service::discovery::ranking::RankingSourceDefinition {
                source_key: provider.provider_key.clone(),
                title: provider.display_name.clone(),
                boards: Vec::new(),
            })
            .collect();
        sm_service::discovery::ranking::RankingSourceCatalog::new(entries)
    }

    /// 从**全部**已加载插件的注册声明重建三张注册表。
    ///
    /// 重启后整体重建而不是增量合并：插件重启后声明可能变（少一个 provider、
    /// 换一份 cron），增量只会留下已经不存在的东西。
    pub fn rebuild(&mut self) {
        let builtin_task_keys: Vec<String> = sm_scheduler::builtin_jobs()
            .into_iter()
            .map(|spec| spec.task_key)
            .collect();
        self.providers = ProviderRegistry::new();
        self.jobs = JobRegistry::with_builtin(builtin_task_keys);
        self.extensions = ExtensionRegistry::new();
        // 先拷出声明再收：`collect` 要可变借用 `self`，而遍历也在借 `self`。
        let registrations: Vec<RegisterResponse> = self
            .loaded
            .iter()
            .map(|plugin| plugin.registration.clone())
            .collect();
        for registration in &registrations {
            self.collect(registration);
        }
    }
}

/// 看门狗：轮询进程退出 → 退避重启 → 重建注册表。
///
/// `stop` 置起后退出；退出前**不**杀插件 —— 插件进程由 [`Plugins`] 的析构带
/// 走（[`PluginProcess`] 的 `Drop` 会 kill），杀两次没必要。
pub async fn watchdog(
    plugins: Arc<tokio::sync::Mutex<Plugins>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut ticker = tokio::time::interval(WATCHDOG_INTERVAL);
    loop {
        ticker.tick().await;
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }

        // 一轮只处理一个崩溃：重启要退避 + 重建注册表，几个同时崩也逐个来。
        let exited = {
            let mut plugins = plugins.lock().await;
            first_exited(&mut plugins.loaded)
        };
        let Some(plugin_id) = exited else {
            continue;
        };

        // 退避时长与启动参数都要在**持有锁**时取，取完就放 —— `launch` 要花
        // 几百毫秒，期间不该锁着整张注册表。
        let (delay, spec) = {
            let plugins = plugins.lock().await;
            let attempt = plugins
                .loaded
                .iter()
                .find(|plugin| plugin.plugin_id == plugin_id)
                .map_or(0, |plugin| plugin.restarts);
            (
                restart_backoff(attempt, BACKOFF_BASE, BACKOFF_CAP),
                plugins.config.launch_spec(&plugin_id),
            )
        };
        tokio::time::sleep(delay).await;

        match launch(&spec).await {
            Ok(launched) => {
                tracing::info!(plugin_id, "插件已重启");
                let mut plugins = plugins.lock().await;
                if let Some(slot) = plugins
                    .loaded
                    .iter_mut()
                    .find(|plugin| plugin.plugin_id == plugin_id)
                {
                    // 旧进程句柄被替换掉即被杀（[`PluginProcess`] 的 `Drop`）。
                    slot.registration = launched.registration;
                    slot.process = launched.process;
                    slot.restarts += 1;
                }
                plugins.rebuild();
            }
            Err(err) => {
                tracing::warn!(
                    plugin_id,
                    code = err.code(),
                    "插件重启失败，下一轮继续尝试：{err:?}"
                );
                let mut plugins = plugins.lock().await;
                if let Some(slot) = plugins
                    .loaded
                    .iter_mut()
                    .find(|plugin| plugin.plugin_id == plugin_id)
                {
                    slot.restarts += 1;
                }
            }
        }
    }
}

/// 找出第一个已退出的插件。
fn first_exited(loaded: &mut [LoadedPlugin]) -> Option<String> {
    for plugin in loaded {
        match plugin.process.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!(
                    plugin_id = plugin.plugin_id.as_str(),
                    %status,
                    "插件进程退出，准备重启"
                );
                return Some(plugin.plugin_id.clone());
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(
                plugin_id = plugin.plugin_id.as_str(),
                error = %err,
                "等待插件进程失败"
            ),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn snapshot(plugins: Value) -> Value {
        let mut root = Map::new();
        root.insert("plugins".to_owned(), plugins);
        Value::Object(root)
    }

    #[test]
    fn the_config_is_read_from_the_readonly_plugins_section() {
        let config = PluginConfig::from_snapshot(&snapshot(serde_json::json!({
            "root_dir": "/srv/plugins",
            "enabled": ["local", "javbus"],
            "settings": {"javbus": {"timeout_seconds": 8}},
            "job_crons": {"local": {"local.cleanup": "0 4 * * *"}},
        })));
        assert_eq!(config.root_dir, Path::new("/srv/plugins"));
        // 顺序即优先级，必须原样保留。
        assert_eq!(config.enabled, vec!["local", "javbus"]);
        assert_eq!(
            config.settings_for("javbus"),
            Some(serde_json::json!({"timeout_seconds": 8}))
        );
        assert_eq!(config.settings_for("local"), None, "没配就是没有");
        assert_eq!(
            config.cron_override("local", "local.cleanup"),
            Some("0 4 * * *")
        );
        assert_eq!(config.cron_override("local", "other"), None);
        assert_eq!(config.cron_override("javbus", "local.cleanup"), None);
    }

    #[test]
    fn a_missing_section_falls_back_to_the_upstream_defaults() {
        // 上游 `Plugins.root_dir = "/data/plugins"`、`enabled = []`。
        let config = PluginConfig::from_snapshot(&Value::Object(Map::new()));
        assert_eq!(config.root_dir, Path::new("/data/plugins"));
        assert!(config.enabled.is_empty());
    }

    #[test]
    fn the_executable_and_data_dir_follow_the_directory_convention() {
        let config = PluginConfig::from_snapshot(&snapshot(serde_json::json!({
            "root_dir": "/srv/plugins",
        })));
        assert_eq!(
            config.program_for("local"),
            Path::new("/srv/plugins/local/local")
        );
        assert_eq!(
            config.data_dir_for("local"),
            Path::new("/srv/plugins/local/data")
        );
    }

    #[tokio::test]
    async fn the_catalog_holds_the_builtin_jobs_even_without_plugins() {
        // 手动触发靠目录认任务：缺一个内建任务，它就变成 404「未知任务」。
        let config = PluginConfig::from_snapshot(&Value::Object(Map::new()));
        let plugins = Plugins::load(config).await;
        let catalog = plugins.catalog();

        assert_eq!(catalog.entries().len(), 19, "19 个内建任务");
        let heat = catalog
            .get("movie_heat_update")
            .expect("内建任务要在目录里");
        assert_eq!(heat.log_name, "movie-heat-update");
        assert_eq!(heat.cli_name, "update-movie-heat");
        assert_eq!(heat.cron_expr.as_deref(), Some("15 0 * * *"));
        assert!(heat.plugin_id.is_none(), "内建任务没有来源插件");

        // `manual_only` 的任务也要在目录里 —— 它没有 cron，手动触发是它唯一的
        // 出路，而目录里没有它就会被判成「未知任务」。
        let manual = catalog
            .get("media_video_info_backfill")
            .expect("manual_only 任务也要在");
        assert_eq!(manual.cron_expr, None);
        assert!(manual.manual_trigger_allowed);
    }

    #[tokio::test]
    async fn a_plugin_that_cannot_start_is_skipped_not_fatal() {
        // 落点不在约定位置上（`root_dir/<id>/<id>`）—— 上游把坏插件记进
        // `PLUGIN_LOAD_ERRORS` 并隔离，这里是「记一条 warn 然后跳过」：
        // 一个坏插件不该让整个后端起不来。
        let config = PluginConfig {
            root_dir: PathBuf::from("/nonexistent-plugins"),
            enabled: vec!["broken".to_owned()],
            ..Default::default()
        };
        let plugins = Plugins::load(config).await;
        assert!(plugins.loaded.is_empty(), "起不来就不该被收进来");
        assert!(plugins.scheduler_specs().is_empty());
    }

    #[tokio::test]
    async fn loading_with_no_enabled_plugins_yields_no_specs() {
        // 默认部署没有插件：`scheduler_specs` 必须与「只有内建任务」同形。
        let config = PluginConfig::from_snapshot(&Value::Object(Map::new()));
        let plugins = Plugins::load(config).await;
        assert!(plugins.loaded.is_empty());
        assert!(plugins.scheduler_specs().is_empty());
    }
}
