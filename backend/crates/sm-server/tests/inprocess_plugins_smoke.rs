//! 进程内插件装配冒烟：`Plugins::load` 在本进程内把 10 个 vendored 插件拉起来。
//!
//! # 为什么不需要插件二进制、也不需要数据库
//!
//! 这条用例走的是**组合根的进程内那条路**（[`sm_server::inprocess_host`]）：插件在
//! 本进程里 serve 自己的控制面，配置作为参数传进去，不读 `SM_SMOKE_PLUGIN_ROOT`、
//! 不连库。于是它补的是 `plugin_launch_smoke.rs` 覆盖不到的那一格 —— 后者要外部
//! 产物，在没摆好二进制的机器上会**跳过**（「跳过与通过长得一样」是本仓反复修过
//! 的坑）。
//!
//! # 判据：**没有插件被静默跳过**
//!
//! `Plugins::load` 对起不来的插件只记一条 warn 然后继续（坏插件隔离，上游
//! `PLUGIN_LOAD_ERRORS` 是同一个意思）。所以「加载成功」不能只看函数返回 ——
//! 这条用例要求**每一个**进程内 id 都在四张产出里露面：
//!
//! | 产出 | 谁该在里面 |
//! |---|---|
//! | 任务目录（`catalog`）| 声明了后台任务的插件 |
//! | provider 表 | `sakuramedia_115_provider` / `plugin_ref_local` |
//! | 排行源表 | `sakuramedia_javdb_ranking` / `sakuramedia_more_rank_movies` |
//! | 元数据源表 | `sakuramedia_javbus_metadata` |
//!
//! 漏一个就红 —— 那正是「插件接上了但没生效」的表现。
//!
//! 另外两条把**执行侧**的接线也钉住：每个插件任务都要有 worker 处理器、
//! `job_target` 要指向活的控制面 —— 「任务到点入队、执行时被判未知任务键」
//! 的退化就发生在那一格。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use sm_server::inprocess_host::INPROCESS_PLUGIN_IDS;
use sm_server::plugins::{PluginConfig, Plugins};

/// 全部进程内插件都启用的配置。`root_dir` 指向临时目录：进程内拉起同样要建
/// `<root>/<id>/data`（`admit` 会把 `settings-schema.json` 写进去），指向一个
/// 不可写的地方会让插件被判成 `plugin_prepare_failed` 而跳过。
fn all_inprocess_config() -> PluginConfig {
    PluginConfig {
        root_dir: temp_root(),
        enabled: INPROCESS_PLUGIN_IDS.iter().map(|id| (*id).to_owned()).collect(),
        ..PluginConfig::default()
    }
}

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("sm-inprocess-smoke-{}", std::process::id()))
}

/// 四张产出里所有插件 id 的并集 —— 「谁真的被接上了」。
fn admitted_plugin_ids(plugins: &Plugins) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = plugins
        .catalog()
        .entries()
        .iter()
        .filter_map(|entry| entry.plugin_id.clone())
        .collect();

    for entry in plugins.provider_registry().lock().expect("provider 表").entries() {
        ids.insert(entry.plugin_id.clone());
    }
    let extensions = plugins.extension_registry();
    let extensions = extensions.lock().expect("扩展表");
    for entry in extensions.ranking_sources() {
        ids.insert(entry.plugin_id.clone());
    }
    for entry in extensions.metadata_sources() {
        ids.insert(entry.plugin_id.clone());
    }
    ids
}

/// ★ 每一个进程内 id 都要在产出里露面 —— 起不来会被静默跳过，这条挡的就是它。
#[tokio::test]
async fn every_inprocess_plugin_is_admitted() {
    let plugins = Plugins::load(all_inprocess_config()).await;
    let admitted = admitted_plugin_ids(&plugins);

    let missing: Vec<&str> = INPROCESS_PLUGIN_IDS
        .iter()
        .copied()
        .filter(|id| !admitted.contains(*id))
        .collect();
    assert!(
        missing.is_empty(),
        "这些进程内插件被静默跳过了（该看一次 warn 日志）：{missing:?}\n已接入：{admitted:?}"
    );
}

/// ★ provider 插件（115 / 参考插件）要进 provider 表，端点必须是**活的**回环端口。
///
/// 「声明收下了但端点打不出去」是这条链最隐蔽的退化：查得到 provider，一到
/// 建库 / 取空间用量就 `unavailable`。所以这里对着端点再 `Register` 一次 ——
/// 能通说明那个服务真的在本进程里 serve 着。
#[tokio::test]
async fn the_provider_endpoints_are_live_loopback_services() {
    let plugins = Plugins::load(all_inprocess_config()).await;
    let registry = plugins.provider_registry();
    let registry = registry.lock().expect("provider 表");

    for expected in ["sakuramedia_115_provider", "plugin_ref_local"] {
        let entry = registry
            .entries()
            .into_iter()
            .find(|entry| entry.plugin_id == expected)
            .unwrap_or_else(|| panic!("{expected} 该进 provider 表：{:?}", registry.entries()));
        assert!(
            entry.plugin_endpoint.starts_with("http://127.0.0.1:"),
            "{expected} 的端点该是回环地址：{}",
            entry.plugin_endpoint
        );

        // 对着端点再注册一次：证明它是本进程里一个真在 serve 的控制面。
        let endpoint = tonic::transport::Endpoint::from_str(&entry.plugin_endpoint)
            .expect("端点该解析得动");
        let mut client = sm_plugins::loader::connect(endpoint)
            .await
            .expect("进程内插件的控制面该连得上");
        let registration =
            sm_plugins::loader::register(&mut client, expected, expected)
                .await
                .expect("注册契约该校验通过");
        assert_eq!(registration.plugin_id, expected, "回显要和注入的一致");
    }
}

/// ★ 排行源与元数据源的定义要**整份**过来（不只是 key）—— 榜单周期字段是
/// 骨架期最常丢的那一格（`GET /ranking-sources/{key}/boards` 曾经的
/// 「榜单定义尚未接入」）。
#[tokio::test]
async fn the_ranking_and_metadata_definitions_arrive_whole() {
    let plugins = Plugins::load(all_inprocess_config()).await;

    // 排行源快照（读侧那份）：javdb 由 javdb-ranking 拥有，6 个榜单都要在。
    let catalog = plugins.ranking_sources();
    let javdb = catalog
        .definitions()
        .into_iter()
        .find(|source| source.source_key == "javdb")
        .expect("javdb 排行源该在");
    assert_eq!(javdb.owner_plugin_id, "sakuramedia_javdb_ranking");
    assert_eq!(javdb.boards.len(), 6, "6 个榜单都要过来");

    // more-movies 那个 crate 的排行源挂在 `..._more_rank_movies` 这个 id 上。
    let owners: BTreeSet<&str> = catalog
        .definitions()
        .iter()
        .map(|source| source.owner_plugin_id.as_str())
        .collect();
    assert!(
        owners.contains("sakuramedia_more_rank_movies"),
        "more-rank 的排行源该在：{owners:?}"
    );

    // 元数据源：javbus 声明的是 `catalog.metadata_source`（不是 media.provider），
    // 所以它只出现在扩展点注册表里。
    let extensions = plugins.extension_registry();
    let extensions = extensions.lock().expect("扩展表");
    let metadata: Vec<&str> = extensions
        .metadata_sources()
        .iter()
        .map(|entry| entry.plugin_id.as_str())
        .collect();
    assert!(
        metadata.contains(&"sakuramedia_javbus_metadata"),
        "javbus 元数据源该在：{metadata:?}"
    );
}

/// ★ 每个插件任务都要有 worker 处理器 —— 少了这一步，任务会到点入队、
/// 能被手动触发，却在 `handlers.build()` 一步查不到处理器，按「未知任务键」
/// 收口为 failed（见 `sm_scheduler::worker` 模块文档）。
#[tokio::test]
async fn every_plugin_job_has_a_worker_handler() {
    let plugins = Arc::new(tokio::sync::Mutex::new(
        Plugins::load(all_inprocess_config()).await,
    ));
    let entries = plugins.lock().await.plugin_job_entries();
    assert!(!entries.is_empty(), "进程内插件该声明至少一个任务");

    // 处理器注册表与组合根**同一条路**建（`plugin_jobs::plugin_handlers`）。
    let handlers = sm_server::plugin_jobs::plugin_handlers(Arc::clone(&plugins)).await;
    let missing: Vec<&str> = entries
        .iter()
        .map(|(task_key, _)| task_key.as_str())
        .filter(|task_key| !handlers.contains(task_key))
        .collect();
    assert!(missing.is_empty(), "这些插件任务缺处理器：{missing:?}");
}

/// ★ `job_target` 是执行时现取的「发给谁」—— 端点必须是活的控制面，且
/// 不是插件任务（内建 / 不存在）时必须返回 `None` 而不是瞎指一个。
#[tokio::test]
async fn job_targets_point_at_live_endpoints() {
    let plugins = Plugins::load(all_inprocess_config()).await;
    let entries = plugins.plugin_job_entries();
    assert!(!entries.is_empty(), "进程内插件该声明至少一个任务");

    for (task_key, plugin_id) in entries {
        let target = plugins
            .job_target(&task_key)
            .unwrap_or_else(|| panic!("{task_key} 该查得到目标"));
        assert_eq!(target.plugin_id, plugin_id, "归属插件该与注册表一致");
        assert!(
            target.endpoint.starts_with("http://127.0.0.1:"),
            "端点该是回环地址：{}",
            target.endpoint
        );
        assert!(
            target.data_dir.ends_with("data"),
            "数据目录该落在插件目录下：{}",
            target.data_dir.display()
        );
    }

    // 内建任务与不存在的键都不是插件任务 —— `Some` 会让 worker 把内建任务
    // 误发给插件。
    assert!(plugins.job_target("activity_record_cleanup").is_none());
    assert!(plugins.job_target("not_a_plugin_job").is_none());
}
