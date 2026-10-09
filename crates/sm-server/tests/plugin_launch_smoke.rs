//! **跨仓**冒烟：宿主真拉起插件仓编译出的二进制，并把它声明的任务跑到底。
//!
//! # 为什么它需要一份外部产物
//!
//! 插件住在自己的仓库里（`sakuramedia-judge-collecttion-movie` /
//! `sakuramedia-subtitlecat`），按「插件只依赖契约」的拆分原则，本仓不能把它们的
//! 源码引进来。所以这里走**产物**：`SM_SMOKE_PLUGIN_ROOT` 指向一个 `<root_dir>`，
//! 里面是宿主约定的 `<plugin_id>/<plugin_id>[.exe]`。
//!
//! ```text
//! # 1) 编译插件（在各自的插件仓里）
//! cargo build
//! # 2) 摆成宿主约定的路径（Windows 的产物带 .exe）
//! mkdir "$env:TEMP\sm-plugin-smoke\sakuramedia_judge_collecttion_movie"
//! copy ...\target\debug\sakuramedia_judge_collecttion_movie.exe "$env:TEMP\..."
//! # 3) 跑本测试
//! $env:SM_SMOKE_PLUGIN_ROOT="$env:TEMP\sm-plugin-smoke"
//! $env:SMDB_TEST_DATABASE_URL='postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
//! cargo test -p sm-server --test plugin_launch_smoke -- --nocapture
//! ```
//!
//! # 缺产物时**跳过**，且说明为什么
//!
//! 这是「可选的外部依赖探测」那一类（与 `sm_db::testing::create()` 的用途相同），
//! 不是「库连不上」那一类 —— 后者必须响亮地失败。**两种缺法都说出来**：环境变量没
//! 给、以及给了但里面没有那个插件的可执行文件。跳过时打一行原因（`--nocapture` 下
//! 看得见）—— 「跳过与通过长得一样」是本仓库反复修过的问题。
//!
//! # 这两条测试补的是哪一格
//!
//! 两侧各自的测试都只覆盖自己那半边：
//!
//! | 测试 | 覆盖 |
//! |---|---|
//! | `plugin_host_integration.rs`（本仓）| `ListMovies` / `PatchMovie` / `ImportSubtitle` 对真库的行为 |
//! | 插件仓自己的 `tests/job_run.rs` | 它的二进制对着**假宿主** + **假站点**跑完任务 |
//! | **本文件** | 宿主拉起**真**二进制 + **真**库 + ABI 两侧接上 |
//!
//! 契约一旦漂移（旧插件配新宿主），最先红的就该是它。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sm_db::repo::{MovieRepository, NewMovie, SubtitleRepository};
use sm_db::testing::TestDb;
use sm_plugin_api::json_struct::json_to_struct;
use sm_plugin_api::v1::plugin_control_client::PluginControlClient;
use sm_plugin_api::v1::RunJobRequest;
use sm_plugins::supervisor::{launch, LaunchSpec};
use sm_server::plugin_host::serve_for;
use sm_server::plugins::{PluginConfig, Plugins};
use sm_service::system::config::ConfigService;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 判定类插件：任务会**写库**，于是「跑完了」有可观察的结果。
const JUDGE: &str = "sakuramedia_judge_collecttion_movie";
/// 字幕插件：任务会**抓站点 + 写字幕文件**（两手都要假服务）。
const SUBTITLECAT: &str = "sakuramedia_subtitlecat";
/// 字幕插件的「手动抓一部」任务。
const SUBTITLECAT_FETCH: &str = "sakuramedia_subtitlecat_fetch";

/// 一个进程级的能力出口配置：字幕落盘的位置挂在 `media.import_image_root_path` 下面
/// （`sakuramedia_subtitlecat` 会真的往那里写文件）。
fn host_config() -> &'static ConfigService {
    static CONFIG: std::sync::OnceLock<ConfigService> = std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        let root = image_root();
        std::fs::create_dir_all(&root).expect("建临时图片根");
        let path = root.join("config.toml");
        // TOML 的**字面量字符串**（单引号）：Windows 路径里的反斜杠在基本字符串里
        // 是转义符，`\t` / `\U` 之类会被吃掉。
        std::fs::write(
            &path,
            format!(
                "[media]\nimport_image_root_path = '{}'\n",
                root.to_string_lossy()
            ),
        )
        .expect("写临时配置");
        ConfigService::new(path)
    })
}

fn image_root() -> PathBuf {
    std::env::temp_dir().join(format!("sm-smoke-images-{}", std::process::id()))
}

/// `<root_dir>`；环境变量没给就返回 `None`（调用方跳过并说明原因）。
fn plugins_root() -> Option<PathBuf> {
    let Ok(root) = std::env::var("SM_SMOKE_PLUGIN_ROOT") else {
        eprintln!("SKIP: 未设置 SM_SMOKE_PLUGIN_ROOT —— 先编译插件仓，见本文件模块文档");
        return None;
    };
    Some(PathBuf::from(root))
}

/// 该插件的可执行文件。
///
/// 用宿主自己的 `entry_point_of`（组合根找可执行文件用的就是它）—— 它按平台探测
/// `.exe`，所以这里不必写死扩展名，也顺带证明「宿主按约定能找着它」。
fn program_of(root: &Path, plugin_id: &str) -> Option<PathBuf> {
    let dir = root.join(plugin_id);
    match sm_plugins::installer::entry_point_of(&dir, plugin_id) {
        Some(program) => Some(program),
        None => {
            eprintln!(
                "SKIP: {dir:?} 里没有 {plugin_id} 的可执行文件 —— 先编译插件仓，见本文件模块文档"
            );
            None
        }
    }
}

/// 拉起插件的规格 —— 与组合根 `PluginConfig::launch_spec` 同形（环境变量注入、
/// 数据目录、配置落点都由 `supervisor::launch` 负责）。
fn launch_spec(
    plugin_id: &str,
    program: &Path,
    root: &Path,
    host_endpoint: String,
    settings: Option<serde_json::Value>,
) -> LaunchSpec {
    LaunchSpec {
        plugin_id: plugin_id.to_owned(),
        manifest_id: plugin_id.to_owned(),
        program: program.display().to_string(),
        args: Vec::new(),
        data_dir: root.join(plugin_id).join("data"),
        settings,
        settings_path: None,
        host_endpoint: Some(host_endpoint),
        ready_timeout: Duration::from_secs(10),
    }
}

// ══════════════════════════════════════════════════════════ 判定插件

/// ★ **组合根那条路**：`Plugins::load` 拉起 + 注册，任务进任务目录。
///
/// 任务目录里有它就说明注册成功了 —— 起不来的插件会被记 warn 并跳过
/// （`PluginConfig` 的坏插件隔离），那正是本用例要挡住的失败。
#[tokio::test]
async fn the_composition_root_launches_the_judge_and_collects_its_job() {
    let Some(root) = plugins_root() else {
        return;
    };
    let Some(_program) = program_of(&root, JUDGE) else {
        return;
    };
    let db = TestDb::require().await;
    let endpoint = serve_for(db.pool(), host_config(), JUDGE)
        .await
        .expect("起能力出口");

    let plugins = Plugins::load(PluginConfig {
        root_dir: root,
        enabled: vec![JUDGE.to_owned()],
        host_endpoints: HashMap::from([(JUDGE.to_owned(), endpoint)]),
        ..PluginConfig::default()
    })
    .await;

    assert!(
        plugins.catalog().get(JUDGE).is_some(),
        "插件该被拉起并注册（它声明的任务要进目录，手动触发靠这个）"
    );
}

/// ★ **整条链路**：宿主拉起真二进制 → `RunJob` → 插件回调宿主 → 库里真的变了。
///
/// 走 `supervisor::launch` 而不是 `Plugins::load`：后者把进程句柄收进私有字段，
/// 集成测试拿不到控制面端点。两条路拉起的都是同一个二进制、同一套环境变量注入。
#[tokio::test]
async fn the_judge_job_runs_end_to_end_and_writes_the_database() {
    let Some(root) = plugins_root() else {
        return;
    };
    let Some(program) = program_of(&root, JUDGE) else {
        return;
    };
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    // 时长 600 分钟 → 该被判定成合集（插件默认阈值 300）。
    let inserted = repo
        .insert(&NewMovie {
            movie_number: "SMOKE-001".to_owned(),
            title: "冒烟".to_owned(),
            duration_minutes: 600,
            ..NewMovie::default()
        })
        .await
        .expect("insert");
    assert!(!inserted.is_collection, "前提：一开始不是合集");

    let endpoint = serve_for(db.pool(), host_config(), JUDGE)
        .await
        .expect("起能力出口");
    let launched = launch(&launch_spec(JUDGE, &program, &root, endpoint, None))
        .await
        .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, JUDGE);
    assert_eq!(launched.registration.jobs.len(), 1);
    assert!(
        launched.registration.extensions.is_empty(),
        "这个插件没有扩展点（上游如此）"
    );

    let data_dir = root.join(JUDGE).join("data");
    let result = run_job_and_collect(&launched, JUDGE, None, &data_dir).await;
    assert_eq!(result["scanned"], 1.0);
    assert_eq!(result["updated"], 1.0);

    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在");
    assert!(after.is_collection, "插件该把这条标成合集");
    assert_eq!(after.mutation_revision, 1, "写一次 → 版本推进一格");
    assert_eq!(
        after.field_owners["is_collection"],
        serde_json::json!(format!("plugin:{JUDGE}")),
        "owner 该是这个插件（身份由宿主下发的端点决定）"
    );
}

// ══════════════════════════════════════════════════════════ 字幕插件

/// ★ **组合根那条路**（字幕插件）：两个任务都进目录。
#[tokio::test]
async fn the_composition_root_launches_the_subtitlecat_and_collects_its_jobs() {
    let Some(root) = plugins_root() else {
        return;
    };
    let Some(_program) = program_of(&root, SUBTITLECAT) else {
        return;
    };
    let db = TestDb::require().await;
    let endpoint = serve_for(db.pool(), host_config(), SUBTITLECAT)
        .await
        .expect("起能力出口");

    let plugins = Plugins::load(PluginConfig {
        root_dir: root,
        enabled: vec![SUBTITLECAT.to_owned()],
        host_endpoints: HashMap::from([(SUBTITLECAT.to_owned(), endpoint)]),
        ..PluginConfig::default()
    })
    .await;

    let catalog = plugins.catalog();
    assert!(catalog.get(SUBTITLECAT_FETCH).is_some(), "手动任务要进目录");
    assert!(
        catalog
            .get("sakuramedia_subtitlecat_fetch_subscribed")
            .is_some(),
        "定时任务要进目录（调度表按它排 cron）"
    );
}

/// ★ **整条链路**（字幕插件）：宿主拉起真二进制 → 插件去**假站点**抓 → 回调宿主
/// 的 `ImportSubtitle` → 库里多一行字幕、文件落在图片根下面。
///
/// 这条同时是 `ImportSubtitle` 接线后的端到端判据：抓取与落盘分别在两个进程里，
/// 中间只隔着 ABI。
#[tokio::test]
async fn the_subtitlecat_manual_job_fetches_and_imports_end_to_end() {
    let Some(root) = plugins_root() else {
        return;
    };
    let Some(program) = program_of(&root, SUBTITLECAT) else {
        return;
    };
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo
        .insert(&NewMovie {
            movie_number: "SSNI-888".to_owned(),
            title: "字幕冒烟".to_owned(),
            ..NewMovie::default()
        })
        .await
        .expect("insert");

    // 假站点：搜索页 → 详情页 → 下载（与插件仓自己的用例同一套 HTML）
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.php"))
        .and(query_param("search", "SSNI-888"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH_HTML))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/subtitles/SSNI-888-001"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DETAIL_HTML))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/download/zh.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SRT))
        .mount(&site)
        .await;

    let endpoint = serve_for(db.pool(), host_config(), SUBTITLECAT)
        .await
        .expect("起能力出口");
    // 站点基址通过**宿主写好的配置文件**交给插件（插件只读那个文件）。
    let settings = Some(serde_json::json!({ "base_url": site.uri() }));
    let launched = launch(&launch_spec(
        SUBTITLECAT,
        &program,
        &root,
        endpoint,
        settings,
    ))
    .await
    .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, SUBTITLECAT);
    assert_eq!(launched.registration.jobs.len(), 2);

    let data_dir = root.join(SUBTITLECAT).join("data");
    let params = Some(serde_json::json!({ "movie_number": "SSNI-888" }));
    let result = run_job_and_collect(&launched, SUBTITLECAT_FETCH, params, &data_dir).await;

    assert_eq!(result["source_matches"], 1.0, "假站点只有一份中文字幕");
    assert_eq!(result["imported"], 1.0, "宿主该收下它");
    assert_eq!(result["failed"], 0.0);

    // 库里多一行，文件真落在图片根下面
    let rows = SubtitleRepository::new(db.pool().clone())
        .list_by_movie(inserted.id)
        .await
        .expect("列字幕");
    assert_eq!(rows.len(), 1, "宿主该登记一行字幕");
    let file = PathBuf::from(&rows[0].file_path);
    assert!(file.is_file(), "{file:?} 该真的落盘");
    assert!(file.starts_with(image_root()), "{file:?}");
    assert_eq!(std::fs::read(&file).expect("读"), SRT.as_bytes());
}

// ══════════════════════════════════════════════════════════ 夹具

const SEARCH_HTML: &str = r#"
<div class="other"><a href="/subtitles/WRONG-999">范围外</a></div>
<div class="subtitles">
  <table><tbody>
    <tr><td><a href="/subtitles/SSNI-888-001">match</a></td></tr>
  </tbody></table>
</div>
"#;

const DETAIL_HTML: &str = r#"<a id="download_zh-CN" href="/download/zh.srt">中文</a>"#;

const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\n你好\n";

/// 跑一次任务并把终态结果读成 JSON。
///
/// 这里**不用** `sm_plugins::runner::run_job`：那个是宿主生产路径的消费方式，
/// 但它只把事件收敛成结局（`Completed { has_result, progress_events }`），**不带
/// 载荷**。本文件要断言结果里的计数（那是 ABI 两侧对「结果形状」的约定），所以直接
/// 读流。`runner` 本身有它自己的单测。
async fn run_job_and_collect(
    launched: &sm_plugins::supervisor::LaunchedPlugin,
    task_key: &str,
    params: Option<serde_json::Value>,
    data_dir: &Path,
) -> serde_json::Value {
    let mut client = PluginControlClient::connect(launched.process.endpoint())
        .await
        .expect("连插件");
    let mut stream = client
        .run_job(RunJobRequest {
            run_id: "smoke".to_owned(),
            task_key: task_key.to_owned(),
            params: params.and_then(|params| json_to_struct(&params)),
            data_dir: data_dir.display().to_string(),
        })
        .await
        .expect("发起任务")
        .into_inner();

    let mut result = None;
    while let Some(event) = stream.message().await.expect("读事件") {
        match event.event {
            Some(sm_plugin_api::v1::job_event::Event::Progress(progress)) => {
                eprintln!(
                    "进度：{}（{}/{}）",
                    progress.text, progress.current, progress.total
                );
            }
            Some(sm_plugin_api::v1::job_event::Event::Result(payload)) => {
                result = Some(sm_plugin_api::json_struct::struct_to_json(Some(&payload)));
            }
            None => panic!("收到没有变体的 JobEvent，说明两边契约不一致"),
        }
    }

    let result = result.expect("任务该以终态帧收尾");
    eprintln!("结果：{result}");
    // 数字经 Struct 回来是 f64（`json_struct` 的既定行为），所以断言侧按 f64 比。
    result
}
