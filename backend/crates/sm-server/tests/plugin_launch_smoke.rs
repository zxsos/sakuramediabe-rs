//! 冒烟：宿主真拉起本仓的插件（**进程内**），并把它声明的任务跑到底。
//!
//! # 为什么不再走「插件产物」
//!
//! 本文件此前测的是跨仓产物：`SM_SMOKE_PLUGIN_ROOT` 指向
//! `<plugin_id>/<plugin_id>[.exe]`，由 `supervisor::launch` 起子进程。9 个 vendored
//! 插件现在都住在本仓（`crates/plugin-*`），且宿主对它们走**进程内宿主**
//! （[`sm_server::inprocess_host`]）—— 它们是纯 lib（只有 `serve()` 入口、没有
//! bin），产物那条前置**永远不成立**，于是整套用例永远 SKIP（「跳过与通过长得
//! 一样」是本仓反复修过的坑）。
//!
//! 现在这些用例默认**真的跑**：走 [`sm_server::inprocess_host::launch`] 起的是同一
//! 个 `serve`、同一套等就绪判据，对着回环端点发同一个 `RunJob`。子进程那条路没有
//! 丢 —— `plugin-ref-local/tests/lifecycle.rs` 拿 `CARGO_BIN_EXE_…` 的真二进制
//! 覆盖它（可执行文件只有那个 crate 有，所以用例只能落在那里）。
//!
//! # 需要什么
//!
//! - **PostgreSQL**：`SMDB_TEST_DATABASE_URL`。缺了**响亮地失败**，不是跳过 ——
//!   理由见 `TestDb::require` 的文档。
//! - **假站点**：wiremock 就地起，不碰外网。
//!
//! ```text
//! $env:SMDB_TEST_DATABASE_URL='postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
//! cargo test -p sm-server --test plugin_launch_smoke -- --nocapture
//! ```
//!
//! # 这几条测试补的是哪一格
//!
//! | 测试 | 覆盖 |
//! |---|---|
//! | `inprocess_plugins_smoke.rs`（本仓）| 10 个进程内 id 都接上了（目录 / provider / 扩展点）、任务有处理器、端点是活的 |
//! | `plugin_host_integration.rs`（本仓）| 影片/演员/字幕那几组 rpc 对真库的行为 |
//! | 各插件 crate 自己的 `tests/` | 它的逻辑对着**假宿主** + **假站点**跑 |
//! | **本文件** | 组合根拉起**真插件服务** + **真**库 + ABI 两侧接上，任务**真的改了库** |
//!
//! 契约一旦漂移（旧插件配新宿主），最先红的就该是它。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sm_db::repo::discovery::RankingItemRepository;
use sm_db::repo::{ActorRepository, MovieRepository, NewActor, NewMovie, SubtitleRepository};
use sm_db::testing::TestDb;
use sm_plugin_api::json_struct::json_to_struct;
use sm_plugin_api::v1::plugin_control_client::PluginControlClient;
use sm_plugin_api::v1::RunJobRequest;
use sm_plugins::extensions::{collect_extensions, ExtensionRegistry};
use sm_plugins::runner::{run_job_with_progress, JobOutcome};
use sm_plugins::supervisor::LaunchSpec;
use sm_server::inprocess_host;
use sm_server::metadata_import::MetadataImportSlot;
use sm_server::plugin_host::{serve_for, serve_for_with};
use sm_server::plugins::{self, PluginConfig, Plugins};
use sm_server::ranking_gateway::{RankingPluginGateway, RankingSyncSlot};
use sm_service::discovery::ranking::RankingSyncService;
use sm_service::system::config::ConfigService;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 判定类插件：任务会**写库**，于是「跑完了」有可观察的结果。
const JUDGE: &str = "sakuramedia_judge_collecttion_movie";
/// 字幕插件：任务会**抓站点 + 写字幕文件**（两手都要假服务）。
const SUBTITLECAT: &str = "sakuramedia_subtitlecat";
/// 字幕插件的「手动抓一部」任务。
const SUBTITLECAT_FETCH: &str = "sakuramedia_subtitlecat_fetch";
/// 资料补全插件：任务会**抓两个来源 + 写演员资料**（`task_key` 与 `plugin_id` 同名）。
const ACTOR_METADATA: &str = "sakuramedia_actor_metadata";
/// 文案抓取翻译插件：任务会**反向调宿主的影片 rpc + 抓 DMM + 写回标题/简介**。
const SCRAPE_TRANSLATE: &str = "sakuramedia_movie_scrape_translate";
/// 它的「按番号」手动任务。
const SCRAPE_TRANSLATE_SYNC: &str = "sakuramedia_movie_scrape_translate_sync";

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

/// 组合根的插件根目录：`<root>/<plugin_id>/data` 从这里长出来。
///
/// 进程内拉起**不看可执行文件**（那条约定只对子进程形态有意义），所以这里只要一个
/// 可写目录、不需要摆任何产物。仍然按进程取独立目录：跑过的用例会往里落
/// `settings-schema.json`（`Plugins::admit` 写的）。
fn plugins_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("sm-smoke-plugins-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("建临时插件根");
    root
}

/// 拉起插件的规格 —— 与组合根 `PluginConfig::launch_spec` 同形（数据目录与配置
/// 都交给宿主决定，见 [`sm_server::inprocess_host::launch`]）。
///
/// # `program` 是空串，**故意的**
///
/// 进程内宿主不看它（可执行文件那条约定只对子进程形态有意义）。留空串而不是随手
/// 填一个存在的路径：哪天宿主误把进程内插件丢给子进程那条路，报出来的会是
/// 「找不到可执行文件」，一眼看得出是接线错了 —— 而不是静默跑起来个别的东西。
fn launch_spec(
    plugin_id: &str,
    host_endpoint: String,
    settings: Option<serde_json::Value>,
) -> LaunchSpec {
    LaunchSpec {
        plugin_id: plugin_id.to_owned(),
        manifest_id: plugin_id.to_owned(),
        program: String::new(),
        args: Vec::new(),
        // 每次拉起前清空工作目录：本文件里的用例要能**反复跑**，理由见
        // [`data_dir_of`]。
        data_dir: {
            let dir = data_dir_of(plugin_id);
            let _ = std::fs::remove_dir_all(&dir);
            dir
        },
        settings,
        settings_path: None,
        host_endpoint: Some(host_endpoint),
        ready_timeout: Duration::from_secs(10),
    }
}

/// 冒烟用的插件**工作目录**（任务请求里的 `data_dir`，插件按它定位状态文件）。
///
/// 刻意**不**落在 `<root>/<plugin_id>/data`：那是插件的真实安装目录，跑一次就会
/// 留下状态文件（演员插件的 `actor_metadata.json`、字幕插件的 `fetch_state.json`）。
/// 而 `TestDb` 每次给的是**干净**的库、自增 id 又回到 1 —— 第二次跑同一个用例时，
/// 插件读到上一轮的「演员 1 已完成」就整轮跳过（`scanned=1, skipped=1`），用例退化
/// 成「只能跑一次」。产物式冒烟本来就是要反复跑的（改完宿主跑一遍、改完插件再跑
/// 一遍），所以每次给一个全新的临时目录。
fn data_dir_of(plugin_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sm-smoke-data-{}-{plugin_id}", std::process::id()))
}

// ══════════════════════════════════════════════════════════ 判定插件

/// ★ **组合根那条路**：`Plugins::load` 拉起 + 注册，任务进任务目录。
///
/// 任务目录里有它就说明注册成功了 —— 起不来的插件会被记 warn 并跳过
/// （`PluginConfig` 的坏插件隔离），那正是本用例要挡住的失败。
#[tokio::test]
async fn the_composition_root_launches_the_judge_and_collects_its_job() {
    let db = TestDb::require().await;
    // 排行榜同步这一轮不测（这些用例只看拉起与注册），槽留空 —— 空了那两个
    // rpc 回 `Unavailable`，不会假成功。
    let endpoint = serve_for(
        db.pool(),
        host_config(),
        JUDGE,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");

    let plugins = Plugins::load(PluginConfig {
        root_dir: plugins_root(),
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

/// ★ **整条链路**：宿主拉起真插件服务 → `RunJob` → 插件回调宿主 → 库里真的变了。
///
/// 走 `inprocess_host::launch` 而不是 `Plugins::load`：后者把插件句柄收进私有字段，
/// 集成测试拿不到控制面端点。两条路拉起的是**同一个** `serve`、同一套等就绪判据。
#[tokio::test]
async fn the_judge_job_runs_end_to_end_and_writes_the_database() {
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

    // 排行榜同步这一轮不测（这些用例只看拉起与注册），槽留空 —— 空了那两个
    // rpc 回 `Unavailable`，不会假成功。
    let endpoint = serve_for(
        db.pool(),
        host_config(),
        JUDGE,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");
    let launched = inprocess_host::launch(&launch_spec(JUDGE, endpoint, None))
        .await
        .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, JUDGE);
    assert_eq!(launched.registration.jobs.len(), 1);
    assert!(
        launched.registration.extensions.is_empty(),
        "这个插件没有扩展点（上游如此）"
    );

    let data_dir = data_dir_of(JUDGE);
    let result = run_job_and_collect(launched.plugin.endpoint(), JUDGE, None, &data_dir).await;
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
    let db = TestDb::require().await;
    let endpoint = serve_for(
        db.pool(),
        host_config(),
        SUBTITLECAT,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");

    let plugins = Plugins::load(PluginConfig {
        root_dir: plugins_root(),
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

/// ★ **整条链路**（字幕插件）：宿主拉起真插件服务 → 插件去**假站点**抓 → 回调宿主
/// 的 `ImportSubtitle` → 库里多一行字幕、文件落在图片根下面。
///
/// 这条同时是 `ImportSubtitle` 接线后的端到端判据：抓取与落盘分别在插件与宿主
/// 两侧，中间只隔着 ABI。
#[tokio::test]
async fn the_subtitlecat_manual_job_fetches_and_imports_end_to_end() {
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

    let endpoint = serve_for(
        db.pool(),
        host_config(),
        SUBTITLECAT,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");
    // 站点基址通过**宿主写好的配置**交给插件（插件只读那个文件）。
    let settings = Some(serde_json::json!({ "base_url": site.uri() }));
    let launched = inprocess_host::launch(&launch_spec(SUBTITLECAT, endpoint, settings))
        .await
        .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, SUBTITLECAT);
    assert_eq!(launched.registration.jobs.len(), 2);

    let data_dir = data_dir_of(SUBTITLECAT);
    let params = Some(serde_json::json!({ "movie_number": "SSNI-888" }));
    let result = run_job_and_collect(
        launched.plugin.endpoint(),
        SUBTITLECAT_FETCH,
        params,
        &data_dir,
    )
    .await;

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

// ══════════════════════════════════════════════════ JavDB 排行榜插件

/// JavDB 排行榜插件：**声明 + 转发**，取数（登录、签名、解析）都在宿主那边。
const RANKING: &str = "sakuramedia_javdb_ranking";
/// 它的手动单榜任务。
const RANKING_SYNC_BOARD: &str = "sakuramedia_javdb_ranking_sync_board";

/// 榜单响应的形状（`success` 包络 + `data.movies`）。
fn rank_body(numbers: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "success": 1,
        "data": {
            "movies": numbers
                .iter()
                .map(|number| serde_json::json!({ "number": number }))
                .collect::<Vec<_>>(),
        },
    })
}

/// ★ **组合根那条路**：真插件拉起来后，榜单定义要**整份**（不只是 key）进目录。
///
/// 这条挡的是骨架期那个「接口成功但没数据」的退化：`GET /ranking-sources/{key}/boards`
/// 曾经永远回「榜单定义尚未接入」，因为收集器读的是 provider 注册表（那里没有
/// boards）并硬写空数组。现在这条链是：插件注册载荷 → `ExtensionRegistry` →
/// `Plugins::ranking_sources()`。
#[tokio::test]
async fn the_composition_root_collects_the_javdb_board_definitions() {
    let db = TestDb::require().await;
    let endpoint = serve_for(
        db.pool(),
        host_config(),
        RANKING,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");

    let plugins = Plugins::load(PluginConfig {
        root_dir: plugins_root(),
        enabled: vec![RANKING.to_owned()],
        host_endpoints: HashMap::from([(RANKING.to_owned(), endpoint)]),
        ..PluginConfig::default()
    })
    .await;

    assert!(
        plugins.catalog().get(RANKING_SYNC_BOARD).is_some(),
        "手动单榜任务要进任务目录"
    );

    let catalog = plugins.ranking_sources();
    let definitions = catalog.definitions();
    assert_eq!(definitions.len(), 1, "这个插件声明一个排行源");
    let source = &definitions[0];
    assert_eq!(source.source_key, "javdb");
    assert_eq!(
        source.owner_plugin_id, RANKING,
        "归属是写侧授权用的（插件只能同步自己的源）"
    );
    assert_eq!(source.boards.len(), 6, "6 个榜单都要过来");

    let playback = source
        .boards
        .iter()
        .find(|board| board.board_key == "playback_all")
        .expect("热播榜");
    assert_eq!(
        playback.supported_periods,
        ["daily", "weekly", "monthly"],
        "★ 周期字段要一起过来 —— 骨架期这里永远是空数组"
    );
    assert_eq!(playback.default_period, "daily");
    assert!(!playback.dynamic_periods);

    let top250 = source
        .boards
        .iter()
        .find(|board| board.board_key == "top250")
        .expect("TOP250");
    assert!(
        top250.supported_periods.is_empty() && top250.dynamic_periods,
        "TOP250 的周期随年份滚动：载荷里是空的，但要标出「周期要问插件」"
    );
    assert_eq!(top250.default_period, "all", "代表值仍要给");
}

/// ★ **整条链路**：宿主拉起真插件 → 插件 `RunJob` → 宿主写侧 → 回调插件的
/// `FetchRanking` → 插件回调宿主的 `GetJavdbRankNumbers` → **假 JavDB** → 落库。
///
/// 这条链上有**四个**跨模块的跳跃，每一跳都能单独写错而其它测试全绿：
///
/// | 跳 | 写错的症状 |
/// |---|---|
/// | 插件把 `board_key` 翻成查询臂 | 打到错的端点，拿到别的榜 |
/// | 宿主 rpc 没注入账号 | TOP250 静默返回空榜 |
/// | 宿主的取数网关拿**快照**端点 | 插件重启后全部失败，而注册表看着正常 |
/// | 宿主先删后插没在同一事务 | 中途失败留下空 scope |
///
/// 打桩点在**宿主这一侧**（`serve_for_with` 的 `javdb_base`）：JavDB 客户端只在
/// 宿主那里实例化。
#[tokio::test]
async fn the_javdb_ranking_job_syncs_a_board_end_to_end() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    for number in ["SMOKE-J1", "SMOKE-J2"] {
        movies
            .insert(&NewMovie {
                movie_number: number.to_owned(),
                title: "冒烟".to_owned(),
                ..NewMovie::default()
            })
            .await
            .expect("insert");
    }

    // 假 JavDB：播放榜回两条，**顺序即排名**（第 2 部在前）。
    let javdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rankings/playback"))
        .and(query_param("filter_by", "all"))
        .and(query_param("period", "daily"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(rank_body(&["SMOKE-J2", "SMOKE-J1"])),
        )
        .mount(&javdb)
        .await;

    // 4a：先起能力出口（端点要注入给插件）。槽此刻还是空的 —— 排行源目录要等
    // 插件注册完才有，与组合根的时序一致。
    let slot = RankingSyncSlot::new();
    let endpoint = serve_for_with(
        db.pool(),
        host_config(),
        RANKING,
        slot.clone(),
        Some(&javdb.uri()),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");

    let launched = inprocess_host::launch(&launch_spec(RANKING, endpoint, None))
        .await
        .expect("宿主拉起插件");
    assert_eq!(launched.registration.plugin_id, RANKING);
    assert_eq!(launched.registration.jobs.len(), 2, "定时 + 手动各一个");

    // 用宿主**同一条**收集函数与转换建目录：这条用例要的是**注册载荷原文 + 控制面
    // 端点**（填排行槽必须发生在跑任务之前），而 `Plugins::load` 只暴露转换后的快照
    // 与 `job_target`，拿不到载荷原文 —— 所以这里自己走一遍拉起与收集。
    let mut registry = ExtensionRegistry::new();
    let problems = collect_extensions(
        &mut registry,
        &launched.registration,
        launched.plugin.endpoint(),
    );
    assert!(problems.is_empty(), "注册载荷该被全部收下：{problems:?}");
    let catalog = plugins::ranking_catalog(&registry);
    assert_eq!(
        catalog.definitions()[0].boards.len(),
        6,
        "前提：榜单定义收到了"
    );

    // 填槽：取数网关拿的是**活的**注册表句柄（端点会随插件重启变）。
    let registry = Arc::new(Mutex::new(registry));
    slot.fill(Arc::new(
        RankingSyncService::new(db.pool().clone(), catalog)
            .with_gateway(Arc::new(RankingPluginGateway::new(registry))),
    ));

    let data_dir = data_dir_of(RANKING);
    let result = run_job_and_collect(
        launched.plugin.endpoint(),
        RANKING_SYNC_BOARD,
        Some(serde_json::json!({ "board_key": "playback_all", "period": "daily" })),
        &data_dir,
    )
    .await;

    assert_eq!(
        result["fetched_numbers"], 2.0,
        "番号是插件经宿主问 JavDB 拿的"
    );
    assert_eq!(result["local_hit_movies"], 2.0, "两条番号都在库里");
    assert_eq!(result["stored_items"], 2.0);
    assert_eq!(result["board_key"], "playback_all");
    assert_eq!(result["period"], "daily");

    let items = RankingItemRepository::new(db.pool().clone())
        .list_by_board("javdb", "playback_all", "daily")
        .await
        .expect("读回榜单条目");
    assert_eq!(items.len(), 2, "整榜替换后应当只有这两条");
    assert_eq!(
        (items[0].rank, items[0].movie_number.as_str()),
        (1, "SMOKE-J2"),
        "名次照插件给的顺序：JavDB 说第 2 部是第 1 名"
    );
    assert_eq!(
        (items[1].rank, items[1].movie_number.as_str()),
        (2, "SMOKE-J1")
    );
}

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
/// 走 `sm_plugins::runner::run_job_with_progress` —— 宿主**生产路径**的同一条调用
/// （进度回调与事件收敛只有这一份实现）。本文件要断言结果里的计数，而结局对象现在
/// **带着载荷**（`JobOutcome::Completed { result, .. }` 曾经只有 `has_result: bool`），
/// 所以不必再自己读流。
///
/// `deadline` 给了上限：卡住的任务要在冒烟里**响亮地失败**（`Cancelled`），而不是把
/// 整个测试挂死。
async fn run_job_and_collect(
    endpoint: &str,
    task_key: &str,
    params: Option<serde_json::Value>,
    data_dir: &Path,
) -> serde_json::Value {
    let mut client = PluginControlClient::connect(endpoint.to_owned())
        .await
        .expect("连插件");
    let outcome = run_job_with_progress(
        &mut client,
        RunJobRequest {
            run_id: "smoke".to_owned(),
            task_key: task_key.to_owned(),
            params: params.and_then(|params| json_to_struct(&params)),
            data_dir: data_dir.display().to_string(),
        },
        Some(Duration::from_secs(120)),
        |event| {
            if let Some(sm_plugin_api::v1::job_event::Event::Progress(progress)) = event.event {
                eprintln!(
                    "进度：{}（{}/{}）",
                    progress.text, progress.current, progress.total
                );
            }
            std::future::ready(())
        },
    )
    .await
    .expect("读事件");

    match outcome {
        JobOutcome::Completed { result, .. } => {
            eprintln!("结果：{result}");
            // 数字经 Struct 回来是 f64（`json_struct` 的既定行为），所以断言侧按 f64 比。
            result
        }
        other => panic!("任务该以终态结果收尾，实际是 {other:?}"),
    }
}

// ══════════════════════════════════════════════════════════ 女优资料补全插件

/// ★ **整条链路**（资料补全插件）：宿主拉起真插件服务 → 插件查**假 JavDB** → 回调
/// 宿主的 `ListActors` / `PatchActor` → 库里那位演员的资料**真的被补上**。
///
/// 这条同时钉住几件在两侧各自测试里看不到的事：`ListActors` 分页可用、`PatchActor` 的
/// 端点身份认得上（写进去的 owner 是 `plugin:sakuramedia_actor_metadata`）、版本推进。
#[tokio::test]
async fn the_actor_metadata_job_fills_the_profile_end_to_end() {
    let db = TestDb::require().await;
    // 一位「什么都缺」的演员：只有身份与名字，九个资料字段全空。
    let actors = ActorRepository::new(db.pool().clone());
    let inserted = actors
        .insert(&NewActor {
            javdb_id: "smoke-actor".to_owned(),
            name: "冒烟女优".to_owned(),
        })
        .await
        .expect("insert actor");

    // 假 JavDB：一次把九个字段给全（于是插件不会再查 MinnanoAV）。
    // `gender: 0` 是 JavDB 的枚举（0=女），插件要把它映射成本地的 1。
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/actors/smoke-actor"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                serde_json::json!({
                    "success": 1,
                    "data": { "actor": {
                        "id": "smoke-actor",
                        "name": "冒烟女优",
                        "birthday": "1996-03-14",
                        "height": 160,
                        "bust": 88,
                        "waist": 58,
                        "hips": 86,
                        "cup": "D",
                        "birthplace": "東京都",
                        "blood_type": "A",
                        "gender": 0
                    }}
                })
                .to_string(),
            ),
        )
        .mount(&site)
        .await;

    let endpoint = serve_for(
        db.pool(),
        host_config(),
        ACTOR_METADATA,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");
    // 两个站点基址都指向 wiremock（插件仓多出来的那两格配置）。
    let settings = Some(serde_json::json!({
        "javdb_base_url": site.uri(),
        "minnanoav_base_url": site.uri(),
        "request_interval_seconds": 0.2,
    }));
    let launched = inprocess_host::launch(&launch_spec(ACTOR_METADATA, endpoint, settings))
        .await
        .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, ACTOR_METADATA);
    assert_eq!(launched.registration.jobs.len(), 1, "上游只有一个后台任务");

    let data_dir = data_dir_of(ACTOR_METADATA);
    let result = run_job_and_collect(
        launched.plugin.endpoint(),
        ACTOR_METADATA,
        None,
        &data_dir,
    )
    .await;

    assert_eq!(result["scanned"], 1.0);
    assert_eq!(result["attempted"], 1.0);
    assert_eq!(result["updated"], 1.0, "{result}");
    assert_eq!(result["completed"], 1.0, "{result}");
    assert_eq!(result["errors"], 0.0, "{result}");

    // 库里的资料真的被补上了
    let after = actors
        .find_by_id(inserted.id)
        .await
        .expect("查演员")
        .expect("还在");
    assert_eq!(after.height_cm, Some(160));
    assert_eq!(after.cup.as_deref(), Some("D"));
    assert_eq!(
        after.birthday.map(|date| date.to_string()).as_deref(),
        Some("1996-03-14")
    );
    assert_eq!(after.gender, 1, "JavDB 的 0（女）映射成本地的 1");
    assert_eq!(
        after.field_owners["height_cm"],
        serde_json::json!(format!("plugin:{ACTOR_METADATA}")),
        "owner 该是这个插件（身份由宿主下发的端点决定）"
    );
    assert!(after.mutation_revision > 0, "写要推进版本");
}

// ══════════════════════════════════════════════════════════ 文案抓取翻译插件

/// 假 DMM 的搜索页：标题带上番号，正文里一条详情页链接（与 `dmm.rs` 的用例同源）。
const DMM_SEARCH_HTML: &str = r#"<html><head><title>検索結果 ABC-123</title></head><body>
    <a href="/detail/=/cid=abc123/">商品</a></body></html>"#;
/// 假 DMM 的详情页：日文标题 + 简介。
const DMM_DETAIL_HTML: &str = r#"<html><head><title>商品页</title></head><body>
    <h1 id="title">日文タイトル</h1>
    <div class="mg-b20 lh4">これは説明文です</div></body></html>"#;

/// ★ **整条链路**（文案插件）：宿主拉起真插件服务 → 插件回调宿主的
/// `FindMoviesByNumbers` 找到影片 → 抓**假 DMM** → 回调 `PatchMovie` 把标题与
/// 简介写回 → 库里的值真的变了。
///
/// 这条同时钉住 [`sm_server::plugin_host`] 上那四个 rpc 的接线：列表 / 查询 /
/// 单查（写回前的复核）/ 写回；以及 `field_owners` 这条字段级归属——插件据此
/// 判断「这个字段归不归我写」，写进去的 owner 是
/// `plugin:sakuramedia_movie_scrape_translate`。
#[tokio::test]
async fn the_scrape_translate_job_writes_back_the_dmm_copy() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    // 一部「还没有文案」的影片：标题与简介都是空的。
    let inserted = repo
        .insert(&NewMovie {
            movie_number: "ABC-123".to_owned(),
            title: String::new(),
            ..NewMovie::default()
        })
        .await
        .expect("insert");

    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/=/searchstr=ABC-123/limit=30/sort=date/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DMM_SEARCH_HTML))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/detail/=/cid=abc123/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DMM_DETAIL_HTML))
        .mount(&site)
        .await;

    let endpoint = serve_for(
        db.pool(),
        host_config(),
        SCRAPE_TRANSLATE,
        RankingSyncSlot::new(),
        MetadataImportSlot::new(),
    )
    .await
    .expect("起能力出口");
    // DMM 基址通过**宿主写好的配置**交给插件（插件只读那个文件）。
    let settings = Some(serde_json::json!({
        "dmm_base_url": site.uri(),
        "request_interval_seconds": 0.0,
    }));
    let launched = inprocess_host::launch(&launch_spec(SCRAPE_TRANSLATE, endpoint, settings))
        .await
        .expect("宿主拉起插件");

    assert_eq!(launched.registration.plugin_id, SCRAPE_TRANSLATE);
    assert_eq!(launched.registration.jobs.len(), 3, "上游声明三个任务");

    let data_dir = data_dir_of(SCRAPE_TRANSLATE);
    let params = Some(serde_json::json!({ "movie_number": "ABC-123" }));
    let result = run_job_and_collect(
        launched.plugin.endpoint(),
        SCRAPE_TRANSLATE_SYNC,
        params,
        &data_dir,
    )
    .await;

    assert_eq!(result["movies"], 1.0, "只有那一部：{result}");
    assert_eq!(result["fetched"], 1.0, "DMM 抓到了：{result}");
    assert_eq!(result["fetch_failed"], 0.0, "{result}");
    assert_eq!(
        result["writeback_applied"], 2.0,
        "标题与简介各写一次：{result}"
    );
    assert_eq!(result["writeback_failed"], 0.0, "{result}");
    assert_eq!(result["failed_movies"], 0.0, "{result}");

    // 库里的文案真的被写上了
    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("查影片")
        .expect("还在");
    assert_eq!(after.title, "日文タイトル");
    assert_eq!(after.summary, "これは説明文です");
    assert_eq!(
        after.field_owners["title"],
        serde_json::json!(format!("plugin:{SCRAPE_TRANSLATE}")),
        "owner 该是这个插件（身份由宿主下发的端点决定）"
    );
    assert!(after.mutation_revision >= 2, "两次写要推进两格版本");
}
