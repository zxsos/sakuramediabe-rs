//! `PluginHostService` 的集成测试：真库 + 真实服务实例。
//!
//! # 与 `sm-db/tests/gateway_integration.rs` 的分工
//!
//! 那边钉主权网关的 SQL（占位符编号、jsonb 重建、CAS），这边钉**这一层**的两件事：
//!
//! 1. `proto` 形态 → `FieldPatch` 的**翻译**（哪个字段收哪种值、未知字段怎么办）；
//! 2. **身份**：写进去的 owner 是「端点属于谁」，而不是请求里自报的东西；
//! 3. `ListMovies` 的游标语义 —— 只有真库能验（`WHERE id > $1`）。
//!
//! 直接调 trait 方法而不是起 gRPC：语义都在服务实例里，端点分配那条路在
//! `sm-server/src/lib.rs` 的装配步骤 4a（`server_smoke.rs` 覆盖进程级冒烟）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use prost_types::value::Kind;
use prost_types::Value as PbValue;
use sm_db::repo::{MovieRepository, NewMovie, SubtitleRepository};
use sm_db::testing::TestDb;
use sm_plugin_api::v1::plugin_host_server::PluginHost;
use sm_plugin_api::v1::{
    ImportSubtitleRequest, ListMoviesRequest, PatchMovieRequest, PatchMovieResponse,
};
use sm_server::plugin_host::PluginHostService;
use sm_service::system::config::ConfigService;
use tonic::Request;

/// 写这个库的插件（判决类插件的 id，与 `sakuramedia-judge-collecttion-movie` 一致）。
const PLUGIN: &str = "sakuramedia_judge_collecttion_movie";

/// 测试用的图片根：**每个进程一个**，指向一个临时目录。
///
/// 每个用例都要建能力出口实例，而构造器要一个 `ConfigService`（字幕落盘的位置挂在
/// `media.import_image_root_path` 下面）。做「每用例一个临时目录」要把那份守卫在十几
/// 个用例里各传一遍，而它们都不写字幕文件 —— 一个进程级的目录足够。真正会落盘的字幕
/// 用例用**不同的番号**，彼此不会互相看见（`movie_subtitle_hashes` 只列本用例 schema
/// 里那部影片的行）。
fn test_image_root() -> PathBuf {
    std::env::temp_dir().join(format!("sm-plugin-host-images-{}", std::process::id()))
}

fn test_config() -> &'static ConfigService {
    static CONFIG: OnceLock<ConfigService> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let root = test_image_root();
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

/// 建一个绑定到该插件的能力出口实例。
fn host_service(db: &TestDb, plugin_id: &str) -> PluginHostService {
    PluginHostService::new(db.pool(), test_config(), plugin_id)
}

fn movie(number: &str) -> NewMovie {
    NewMovie {
        movie_number: number.to_owned(),
        title: format!("{number} 标题"),
        ..NewMovie::default()
    }
}

fn text_value(text: &str) -> PbValue {
    PbValue {
        kind: Some(Kind::StringValue(text.to_owned())),
    }
}

fn bool_value(flag: bool) -> PbValue {
    PbValue {
        kind: Some(Kind::BoolValue(flag)),
    }
}

/// 一次 `is_collection` 回写（判定类插件唯一会发的那个补丁）。
fn collection_request(movie_id: i32, revision: i64, flag: bool) -> PatchMovieRequest {
    PatchMovieRequest {
        movie_id: i64::from(movie_id),
        fields: HashMap::from([("is_collection".to_owned(), bool_value(flag))]),
        expected_revision: revision,
    }
}

/// 取一页（把 `Response` 拆开，测试里只关心载荷）。
async fn page_of(
    host: &PluginHostService,
    after_id: i64,
    limit: i32,
) -> sm_plugin_api::v1::ListMoviesResponse {
    host.list_movies(Request::new(ListMoviesRequest {
        after_id,
        limit,
        filters: None,
    }))
    .await
    .expect("list_movies")
    .into_inner()
}

/// ★ 游标分页：`next_cursor` 只在**确实还有**下一页时给出。
///
/// 「取满一页」与「到底了」是两种不同的情形，而它们的响应形状只差一个
/// `next_cursor` —— 判错的后果分别是「白跑一趟空查询」与**漏数据**。
#[tokio::test]
async fn list_movies_pages_by_cursor() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    for number in ["PLS-001", "PLS-002", "PLS-003"] {
        repo.insert(&movie(number)).await.expect("insert");
    }
    let host = host_service(&db, PLUGIN);

    let first = page_of(&host, 0, 2).await;
    assert_eq!(first.movies.len(), 2);
    assert!(
        first.movies[0].movie_id < first.movies[1].movie_id,
        "id 升序 —— 游标分页的前提"
    );
    let cursor = first.next_cursor.expect("三选二，后面还有一条");
    assert_eq!(
        cursor, first.movies[1].movie_id,
        "游标是这一页最后一条的 id"
    );

    let second = page_of(&host, cursor, 2).await;
    assert_eq!(second.movies.len(), 1);
    assert!(
        second.next_cursor.is_none(),
        "最后一条（恰好取满）也必须判成到底 —— 少取一条它就会漏数据"
    );

    // `limit` 为 0 时收窄到 1（proto 的上限是 1000，下界由这里定），不是返回空页。
    let narrowed = page_of(&host, 0, 0).await;
    assert_eq!(narrowed.movies.len(), 1);
}

/// 快照带上**可写字段**：`is_collection` / `is_blacklisted` 都得有现值。
///
/// 判定类插件靠 `is_collection` 决定要不要写（已经是 true 就不重复写），
/// 缺了它就只能盲写 —— 那会平白推进 `mutation_revision`。
#[tokio::test]
async fn list_movies_carries_the_writable_fields() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    repo.insert(&movie("SNAP-001")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let page = page_of(&host, 0, 10).await;
    let snapshot = &page.movies[0];
    for key in [
        "title",
        "summary",
        "is_collection",
        "is_blacklisted",
        "movie_number",
        "duration_minutes",
    ] {
        assert!(
            snapshot.values.contains_key(key),
            "快照缺 {key}：可写就得可读"
        );
    }
    assert_eq!(
        snapshot.values["is_collection"].kind,
        Some(Kind::BoolValue(false))
    );
    assert_eq!(
        snapshot.values["is_blacklisted"].kind,
        Some(Kind::BoolValue(false))
    );
    // `is_subscribed` **不在**可写白名单里，但遍历类插件（subtitlecat 的定时任务）
    // 靠它筛已订阅影片 —— 缺了它插件只会看到「一部都没订阅」而**静默什么都不做**。
    assert_eq!(
        snapshot.values["is_subscribed"].kind,
        Some(Kind::BoolValue(false)),
        "快照必须带上订阅位"
    );
    // `maker_name` / `director_name` 是 NULL：缺失的可选字段**不进**快照
    // （`put_opt` 的理由），插件据此知道「这个值是空的」。
    assert!(!snapshot.values.contains_key("maker_name"));
    assert_eq!(snapshot.revision, 0);
}

/// ★ 写成功：值落库、owner 是**这个端点属于的插件**、revision 前进一格。
#[tokio::test]
async fn patch_movie_takes_ownership_for_the_endpoints_plugin() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("JDG-001")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let updated = host
        .patch_movie(Request::new(collection_request(
            inserted.id,
            inserted.mutation_revision,
            true,
        )))
        .await
        .expect("patch_movie")
        .into_inner()
        .updated;

    assert!(updated, "revision 匹配、字段无主 → 必须命中");
    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在");
    assert!(after.is_collection);
    assert_eq!(after.mutation_revision, 1);
    assert_eq!(
        after.field_owners["is_collection"],
        serde_json::json!(format!("plugin:{PLUGIN}")),
        "owner 来自端点身份，不是请求内容"
    );
}

/// ★ 另一个插件的端点写同一行：**拒**，且整次零修改。
///
/// 这是「身份」那一节的判据 —— 同一个库、同一行、只换服务实例。
#[tokio::test]
async fn another_plugins_endpoint_cannot_overwrite_the_field() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("JDG-002")).await.expect("insert");
    let owner = host_service(&db, PLUGIN);
    let intruder = host_service(&db, "sakuramedia_other");

    assert!(
        owner
            .patch_movie(Request::new(collection_request(
                inserted.id,
                inserted.mutation_revision,
                true,
            )))
            .await
            .expect("第一个插件写")
            .into_inner()
            .updated
    );

    let revision_after_owner = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在")
        .mutation_revision;

    let intruder_updated = intruder
        .patch_movie(Request::new(collection_request(
            inserted.id,
            revision_after_owner,
            false,
        )))
        .await
        .expect("第二个插件写（不是错误，是没命中）")
        .into_inner()
        .updated;

    assert!(!intruder_updated, "字段已被 plugin:{PLUGIN} 接管");
    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在");
    assert!(after.is_collection, "值不能被改回去");
    assert_eq!(
        after.mutation_revision, revision_after_owner,
        "零修改：revision 不动"
    );
}

/// 过期 revision → `updated = false`，零修改（乐观并发的契约）。
#[tokio::test]
async fn a_stale_revision_changes_nothing() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("JDG-003")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let updated = host
        .patch_movie(Request::new(collection_request(inserted.id, 99, true)))
        .await
        .expect("不是错误，是没命中")
        .into_inner()
        .updated;

    assert!(!updated);
    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在");
    assert!(!after.is_collection);
    assert_eq!(after.mutation_revision, 0);
    assert!(
        after.field_owners.get("is_collection").is_none(),
        "没命中就不该留下 owner"
    );
}

/// 白名单外的字段、类型不对、空补丁 → `InvalidArgument`（都**不写库**）。
#[tokio::test]
async fn bad_patches_are_invalid_arguments() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("JDG-004")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let cases: Vec<(&str, HashMap<String, PbValue>)> = vec![
        (
            "score",
            HashMap::from([("score".to_owned(), bool_value(true))]),
        ),
        (
            "is_collection",
            HashMap::from([("is_collection".to_owned(), text_value("true"))]),
        ),
        ("(空)", HashMap::new()),
    ];

    for (name, fields) in cases {
        let status = host
            .patch_movie(Request::new(PatchMovieRequest {
                movie_id: i64::from(inserted.id),
                fields,
                expected_revision: 0,
            }))
            .await
            .err()
            .unwrap_or_else(|| panic!("{name} 该被拒"));
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name}");
    }

    let after = repo
        .find_by_id(inserted.id)
        .await
        .expect("find")
        .expect("还在");
    assert_eq!(after.mutation_revision, 0, "被拒的补丁一条都不该落库");
}

/// `filters` 还没映射 → **显式拒**，不是「当成没传筛选」把全库给出去。
#[tokio::test]
async fn unlisted_filters_are_refused() {
    let db = TestDb::require().await;
    let host = host_service(&db, PLUGIN);

    // `let-else` 而不是 `.err().expect()`：后者会撞 `clippy::err_expect`
    // （本仓 `-D warnings` 下是错误），而这里的意图本来就是「必须是 Err」。
    let Err(status) = host
        .list_movies(Request::new(ListMoviesRequest {
            after_id: 0,
            limit: 10,
            filters: Some(prost_types::Struct::default()),
        }))
        .await
    else {
        panic!("带 filters 的请求该被拒");
    };
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(
        status.message().contains("filters"),
        "{:?}",
        status.message()
    );
}

/// `updated = false` 是**正常响应**而不是错误 —— 插件据此重新读快照。
#[tokio::test]
async fn a_missed_patch_is_not_an_error() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("JDG-005")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let response: PatchMovieResponse = host
        .patch_movie(Request::new(collection_request(inserted.id, 7, true)))
        .await
        .expect("不该是 Err")
        .into_inner();
    assert!(!response.updated);
}

// ------------------------------------------------------------------ 字幕导入

/// 一份最小的 SRT。服务**不嗅探内容**（只看扩展名），指纹按内容算 ——
/// 所以四个用例各自用不同字节的 SRT，否则会互相判重复。
fn srt(body: &str) -> Vec<u8> {
    format!("1\n00:00:01,000 --> 00:00:02,000\n{body}\n").into_bytes()
}

/// ★ 导入成功：状态 `imported`、给出 id、文件真落在图片根下面、库里有登记。
#[tokio::test]
async fn import_subtitle_writes_the_file_and_registers_it() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("SUB-001")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let response = host
        .import_subtitle(Request::new(ImportSubtitleRequest {
            movie_number: "SUB-001".to_owned(),
            content: srt("第一份"),
            file_name: "SUB-001-1.srt".to_owned(),
            language: Some("zh-CN".to_owned()),
        }))
        .await
        .expect("导入")
        .into_inner();

    assert_eq!(response.status, "imported");
    assert!(response.subtitle_id > 0, "imported 必须给出 id");

    let rows = SubtitleRepository::new(db.pool().clone())
        .list_by_movie(inserted.id)
        .await
        .expect("列字幕");
    assert_eq!(rows.len(), 1);
    let path = PathBuf::from(&rows[0].file_path);
    assert!(path.is_file(), "{path:?} 该真的落盘");
    assert_eq!(std::fs::read(&path).expect("读"), srt("第一份"));
    // 落点形状：`<图片根>/movies/<shard>/<番号>/subtitles/<番号>-1.srt`
    assert!(path.starts_with(test_image_root()), "{path:?}");
    assert!(
        path.components()
            .any(|part| part.as_os_str() == "subtitles"),
        "{path:?}"
    );
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("SUB-001-1.srt"),
        "文件名由宿主分配（`<番号>-<N>.srt`），不沿用插件给的名字"
    );
}

/// 影片不在库里 → `movie_not_found`（**不是** gRPC 错误），且什么都没写。
#[tokio::test]
async fn import_subtitle_reports_a_missing_movie() {
    let db = TestDb::require().await;
    let host = host_service(&db, PLUGIN);

    let response = host
        .import_subtitle(Request::new(ImportSubtitleRequest {
            movie_number: "NOPE-001".to_owned(),
            content: srt("没有这部片"),
            file_name: "NOPE-001-1.srt".to_owned(),
            language: None,
        }))
        .await
        .expect("不是错误，是结果")
        .into_inner();

    assert_eq!(response.status, "movie_not_found");
    assert_eq!(response.subtitle_id, 0);
}

/// 扩展名不在白名单（`.srt` / `.ass` / `.ssa` / `.vtt`）→ `invalid_format`。
///
/// `".srt"` 这条是**刻意的边界**：`Path(".srt").extension()` 是 `None`（点开头算
/// 文件名而不是后缀），上游 `Path(filename).suffix` 同样如此 —— 两边都判不合法。
#[tokio::test]
async fn import_subtitle_reports_an_unsupported_extension() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    repo.insert(&movie("SUB-002")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    for file_name in ["SUB-002.txt", "SUB-002", ".srt", "SUB-002.srt.bak"] {
        let response = host
            .import_subtitle(Request::new(ImportSubtitleRequest {
                movie_number: "SUB-002".to_owned(),
                content: srt("扩展名不对"),
                file_name: file_name.to_owned(),
                language: None,
            }))
            .await
            .expect("不是错误，是结果")
            .into_inner();
        assert_eq!(response.status, "invalid_format", "{file_name}");
    }
}

/// ★ 同一份内容再来一次 → `duplicate`（去重靠**内容 sha256**，不是文件名）。
#[tokio::test]
async fn import_subtitle_reports_a_duplicate_by_content() {
    let db = TestDb::require().await;
    let repo = MovieRepository::new(db.pool().clone());
    let inserted = repo.insert(&movie("SUB-003")).await.expect("insert");
    let host = host_service(&db, PLUGIN);

    let first = host
        .import_subtitle(Request::new(ImportSubtitleRequest {
            movie_number: "SUB-003".to_owned(),
            content: srt("重复用"),
            file_name: "SUB-003-1.srt".to_owned(),
            language: Some("zh-CN".to_owned()),
        }))
        .await
        .expect("第一份")
        .into_inner();
    assert_eq!(first.status, "imported");

    // **换个文件名**、内容一模一样 —— 仍然算重复：判据是内容指纹
    let second = host
        .import_subtitle(Request::new(ImportSubtitleRequest {
            movie_number: "SUB-003".to_owned(),
            content: srt("重复用"),
            file_name: "别处的名字.srt".to_owned(),
            language: Some("zh-CN".to_owned()),
        }))
        .await
        .expect("第二份")
        .into_inner();
    assert_eq!(second.status, "duplicate");
    assert_eq!(second.subtitle_id, 0);

    let rows = SubtitleRepository::new(db.pool().clone())
        .list_by_movie(inserted.id)
        .await
        .expect("列字幕");
    assert_eq!(rows.len(), 1, "重复不该再登记一行");
}
