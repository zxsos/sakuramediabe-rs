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
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use prost_types::value::Kind;
use prost_types::Value as PbValue;
use sm_db::repo::discovery::{NewRankingItem, RankingItemRepository};
use sm_db::repo::gateway::{ActorOwnershipGateway, FieldPatch, HOST_JAVDB_OWNER};
use sm_db::repo::{
    ActorRepository, MovieActorRepository, MovieRepository, NewActor, NewMovie, SubtitleRepository,
};
use sm_db::testing::TestDb;
use sm_plugin_api::v1::get_javdb_rank_numbers_request::Query;
use sm_plugin_api::v1::plugin_host_server::PluginHost;
use sm_plugin_api::v1::{
    GetActorRequest, GetJavdbRankNumbersRequest, GetMovieRequest, ImportSubtitleRequest,
    JavdbPlaybackRankQuery, JavdbTopQuery, ListActorsRequest, ListMoviesRequest, PatchActorRequest,
    PatchMovieRequest, PatchMovieResponse, SyncRankingBoardRequest, SyncRankingSourcesRequest,
};
use sm_server::plugin_host::PluginHostService;
use sm_server::ranking_gateway::RankingSyncSlot;
use sm_service::discovery::ranking::{
    RankingBoardDefinition, RankingCallError, RankingGateway, RankingSourceCatalog,
    RankingSourceDefinition, RankingSyncService,
};
use sm_service::system::config::ConfigService;
use tonic::Request;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
///
/// 排行同步的槽**留空** —— 排行榜那组用例自己填（见 `ranking_host`）。
fn host_service(db: &TestDb, plugin_id: &str) -> PluginHostService {
    host_service_with(db, plugin_id, RankingSyncSlot::new())
}

/// 带指定排行同步槽的能力出口（排行榜那组用例用）。
fn host_service_with(db: &TestDb, plugin_id: &str, rankings: RankingSyncSlot) -> PluginHostService {
    PluginHostService::new(db.pool(), test_config(), plugin_id, rankings)
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

// ------------------------------------------------------------------ 演员

/// 建一个演员。`NewActor` 只有 `javdb_id` + `name`（其余都是可空资料列），所以资料
/// 字段要靠网关写 —— 与生产里「JavDB 补录」是同一条路。
async fn seed_actor(db: &TestDb, javdb_id: &str, name: &str) -> sm_db::Actor {
    ActorRepository::new(db.pool().clone())
        .insert(&NewActor {
            javdb_id: javdb_id.to_owned(),
            name: name.to_owned(),
        })
        .await
        .expect("插入演员")
}

/// 用**宿主来源**写一批演员字段（owner = `host:javdb`，与 JavDB 补录同一条路）。
async fn seed_host_fields(db: &TestDb, actor_id: i32, patch: &FieldPatch) {
    let updated = ActorOwnershipGateway::new(db.pool().clone())
        .update_host_source(actor_id, patch, HOST_JAVDB_OWNER)
        .await
        .expect("宿主来源写入");
    assert!(updated, "宿主来源写入该命中");
}

fn number_value(number: f64) -> PbValue {
    PbValue {
        kind: Some(Kind::NumberValue(number)),
    }
}

fn actor_request(actor_id: i32, revision: i64, fields: &[(&str, PbValue)]) -> PatchActorRequest {
    PatchActorRequest {
        actor_id: i64::from(actor_id),
        fields: fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
        expected_revision: revision,
    }
}

async fn patch_actor(host: &PluginHostService, request: PatchActorRequest) -> bool {
    host.patch_actor(Request::new(request))
        .await
        .expect("patch_actor")
        .into_inner()
        .updated
}

async fn get_actor(host: &PluginHostService, actor_id: i32) -> sm_plugin_api::v1::ActorSnapshot {
    host.get_actor(Request::new(GetActorRequest {
        actor_id: i64::from(actor_id),
    }))
    .await
    .expect("get_actor")
    .into_inner()
    .actor
    .expect("有 actor")
}

/// ★ 游标分页，且**墓碑不进名单**（合并掉的演员列不出来）。
#[tokio::test]
async fn list_actors_pages_by_cursor_and_hides_merged_rows() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let first = seed_actor(&db, "javdb-1", "一号").await;
    let second = seed_actor(&db, "javdb-2", "二号").await;
    let third = seed_actor(&db, "javdb-3", "三号").await;
    let merged = seed_actor(&db, "javdb-4", "四号").await;
    assert_eq!(
        repo.mark_merged(&[merged.id], second.id)
            .await
            .expect("合并"),
        1
    );

    let host = host_service(&db, PLUGIN);
    let page = host
        .list_actors(Request::new(ListActorsRequest {
            after_id: 0,
            limit: 2,
            filters: None,
        }))
        .await
        .expect("第一页")
        .into_inner();
    let ids: Vec<i64> = page.actors.iter().map(|actor| actor.actor_id).collect();
    assert_eq!(
        ids,
        vec![i64::from(first.id), i64::from(second.id)],
        "id 升序，墓碑被跳过"
    );
    let cursor = page.next_cursor.expect("还有第三位");

    let rest = host
        .list_actors(Request::new(ListActorsRequest {
            after_id: cursor,
            limit: 2,
            filters: None,
        }))
        .await
        .expect("第二页")
        .into_inner();
    assert_eq!(rest.actors.len(), 1);
    assert_eq!(rest.actors[0].actor_id, i64::from(third.id));
    assert!(rest.next_cursor.is_none(), "恰好取满也要判成到底");

    // 超上限收窄（proto 对演员这组没写注释，照影片侧的上界）
    let narrowed = host
        .list_actors(Request::new(ListActorsRequest {
            after_id: 0,
            limit: 5000,
            filters: None,
        }))
        .await
        .expect("收窄")
        .into_inner();
    assert_eq!(narrowed.actors.len(), 3, "三部可见（墓碑不算）");

    // `filters` 未映射 → 显式拒，不假装筛过
    let Err(status) = host
        .list_actors(Request::new(ListActorsRequest {
            after_id: 0,
            limit: 10,
            filters: Some(prost_types::Struct::default()),
        }))
        .await
    else {
        panic!("带 filters 的请求该被拒");
    };
    assert_eq!(status.code(), tonic::Code::Unimplemented);
}

/// ★ 快照带上身份、订阅位与**九个可写资料字段**（可写就得可读）。
#[tokio::test]
async fn an_actor_snapshot_carries_identity_and_profile_fields() {
    let db = TestDb::require().await;
    let actor = seed_actor(&db, "javdb-10", "十号").await;
    let mut host_fields = FieldPatch::new();
    host_fields.text("cup", Some("D"));
    host_fields.int("height_cm", Some(160));
    host_fields.date("birthday", chrono::NaiveDate::from_ymd_opt(1996, 3, 14));
    seed_host_fields(&db, actor.id, &host_fields).await;

    let host = host_service(&db, PLUGIN);
    let snapshot = get_actor(&host, actor.id).await;
    assert_eq!(snapshot.actor_id, i64::from(actor.id));
    assert_eq!(snapshot.revision, 1, "宿主来源写一次 → 版本推进一格");
    assert_eq!(snapshot.owners, vec![HOST_JAVDB_OWNER.to_owned()]);
    // 恒在的六项：身份 + 订阅位
    for key in [
        "name",
        "alias_name",
        "javdb_id",
        "javdb_type",
        "is_subscribed",
        "gender",
    ] {
        assert!(snapshot.values.contains_key(key), "快照缺 {key}");
    }
    // 九个可写资料字段「可写就得可读」：**填过的**必须在快照里……
    for key in ["birthday", "height_cm", "cup"] {
        assert!(snapshot.values.contains_key(key), "填过的字段该在：{key}");
    }
    // ……**没填的不进快照**（缺失 = 空，与影片侧同一条规则）。
    for key in ["bust_cm", "waist_cm", "hips_cm", "birthplace", "blood_type"] {
        assert!(
            !snapshot.values.contains_key(key),
            "没填的字段不该进快照：{key}"
        );
    }
    assert_eq!(
        snapshot.values["cup"].kind,
        Some(Kind::StringValue("D".to_owned()))
    );
    assert_eq!(
        snapshot.values["height_cm"].kind,
        Some(Kind::NumberValue(160.0))
    );
    assert_eq!(
        snapshot.values["birthday"].kind,
        Some(Kind::StringValue("1996-03-14".to_owned()))
    );
    assert_eq!(
        snapshot.values["is_subscribed"].kind,
        Some(Kind::BoolValue(false))
    );
}

/// ★ 插件写资料字段：值落库、owner 是本插件、版本推进。
#[tokio::test]
async fn patch_actor_writes_fields_and_takes_ownership() {
    let db = TestDb::require().await;
    let actor = seed_actor(&db, "javdb-11", "十一号").await;
    let host = host_service(&db, PLUGIN);

    let updated = patch_actor(
        &host,
        actor_request(
            actor.id,
            0,
            &[
                ("height_cm", number_value(158.0)),
                ("cup", text_value("C")),
                ("birthday", text_value("1996-03-14")),
            ],
        ),
    )
    .await;
    assert!(updated, "字段无主 + 版本匹配 → 该命中");

    let snapshot = get_actor(&host, actor.id).await;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.owners, vec![format!("plugin:{PLUGIN}")]);
    assert_eq!(
        snapshot.values["height_cm"].kind,
        Some(Kind::NumberValue(158.0))
    );
    // 生日写进去再读回来是**规范形态**（网关按严格 `YYYY-MM-DD` 解析后落库）
    assert_eq!(
        snapshot.values["birthday"].kind,
        Some(Kind::StringValue("1996-03-14".to_owned()))
    );
}

/// ★ 被 `host:javdb` 占着的字段插件改不动，而且**整次零修改**。
///
/// 网关是**全字段**判定（任一字段条件不满足则一条都不写），不是逐字段部分成功 ——
/// 上游 Python 那份也如此。这条测试把这个后果钉住：同一个 patch 里带上一个没被占的
/// 字段，那个字段也不会落库。
#[tokio::test]
async fn a_host_owned_actor_field_blocks_the_whole_patch() {
    let db = TestDb::require().await;
    let actor = seed_actor(&db, "javdb-12", "十二号").await;
    let mut host_fields = FieldPatch::new();
    host_fields.text("birthplace", Some("东京"));
    seed_host_fields(&db, actor.id, &host_fields).await;

    let host = host_service(&db, PLUGIN);
    assert!(
        !patch_actor(
            &host,
            actor_request(actor.id, 1, &[("birthplace", text_value("大阪"))])
        )
        .await,
        "宿主来源占着的字段该拒"
    );
    assert!(
        !patch_actor(
            &host,
            actor_request(
                actor.id,
                1,
                &[
                    ("birthplace", text_value("大阪")),
                    ("height_cm", number_value(160.0)),
                ],
            )
        )
        .await,
        "全字段判定：连带那个没被占的也不写"
    );

    let snapshot = get_actor(&host, actor.id).await;
    assert_eq!(snapshot.revision, 1, "零修改：版本不动");
    assert_eq!(
        snapshot.values["birthplace"].kind,
        Some(Kind::StringValue("东京".to_owned()))
    );
    assert!(
        !snapshot.values.contains_key("height_cm"),
        "整次不写，那个字段也没落"
    );
}

/// 白名单外 / 类型不对 → `InvalidArgument`，且**不写库**。
#[tokio::test]
async fn bad_actor_patches_are_invalid_arguments() {
    let db = TestDb::require().await;
    let actor = seed_actor(&db, "javdb-13", "十三号").await;
    let host = host_service(&db, PLUGIN);

    for (name, value) in [
        // 身份与订阅：不在白名单里
        ("name", text_value("改名")),
        ("javdb_id", text_value("别的-id")),
        ("is_subscribed", bool_value(true)),
        // 白名单外 + 类型不对 + 松动日期
        ("身高", number_value(160.0)),
        ("height_cm", number_value(160.5)),
        ("birthday", text_value("2020-1-1")),
    ] {
        let Err(status) = host
            .patch_actor(Request::new(actor_request(actor.id, 0, &[(name, value)])))
            .await
        else {
            panic!("{name} 该被拒");
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name}");
        assert!(status.message().contains(name), "{}", status.message());
    }

    let snapshot = get_actor(&host, actor.id).await;
    assert_eq!(snapshot.revision, 0, "被拒的 patch 一条都不该落库");
}

/// ★ 墓碑解析：合并掉的演员，**读与写都落到留存记录上**。
#[tokio::test]
async fn merged_actors_resolve_to_the_surviving_row() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let survivor = seed_actor(&db, "javdb-20", "留存").await;
    let gone = seed_actor(&db, "javdb-21", "被并").await;
    assert_eq!(
        repo.mark_merged(&[gone.id], survivor.id)
            .await
            .expect("合并"),
        1
    );

    let host = host_service(&db, PLUGIN);
    let snapshot = get_actor(&host, gone.id).await;
    assert_eq!(
        snapshot.actor_id,
        i64::from(survivor.id),
        "读要解析到留存记录"
    );

    assert!(
        patch_actor(
            &host,
            actor_request(
                gone.id,
                snapshot.revision,
                &[("height_cm", number_value(160.0))],
            )
        )
        .await
    );
    let after = get_actor(&host, survivor.id).await;
    assert_eq!(
        after.values["height_cm"].kind,
        Some(Kind::NumberValue(160.0)),
        "写也落在留存记录上"
    );
}

/// ★ 影片快照带上演员：按 `actor_id` 升序，且**是完整快照**（上游也这么给）。
#[tokio::test]
async fn a_movie_snapshot_carries_its_actors_in_id_order() {
    let db = TestDb::require().await;
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&movie("ACT-001"))
        .await
        .expect("insert");
    let first = seed_actor(&db, "javdb-30", "甲").await;
    let second = seed_actor(&db, "javdb-31", "乙").await;
    let links = MovieActorRepository::new(db.pool().clone());
    // 故意**反序**关联：顺序该由查询定，不由插入顺序定
    links.link(movie.id, second.id).await.expect("关联乙");
    links.link(movie.id, first.id).await.expect("关联甲");

    let host = host_service(&db, PLUGIN);
    let snapshot = host
        .get_movie(Request::new(GetMovieRequest {
            movie_id: i64::from(movie.id),
        }))
        .await
        .expect("取影片")
        .into_inner()
        .movie
        .expect("有影片");
    let ids: Vec<i64> = snapshot.actors.iter().map(|actor| actor.actor_id).collect();
    assert_eq!(
        ids,
        vec![i64::from(first.id), i64::from(second.id)],
        "按 actor_id 升序"
    );
    assert_eq!(
        snapshot.actors[0].values["name"].kind,
        Some(Kind::StringValue("甲".to_owned())),
        "演员项是完整快照，不是只有 id"
    );

    // 列表那条路也要带：`actor_metadata` 靠它算「关联的非合集影片数」
    let page = host
        .list_movies(Request::new(ListMoviesRequest {
            after_id: 0,
            limit: 10,
            filters: None,
        }))
        .await
        .expect("列影片")
        .into_inner();
    let listed = page
        .movies
        .iter()
        .find(|listed| listed.movie_id == i64::from(movie.id))
        .expect("在列表里");
    assert_eq!(listed.actors.len(), 2);
}

// ══════════════════════════════════════════════════════════════════
// JavDB 榜单（`GetJavdbRankNumbers`）
// ══════════════════════════════════════════════════════════════════
//
// 这一组**不碰库**（榜单是只读出网的），但服务实例构造要一个 `Db`，所以沿用
// 本文件的 `TestDb::require()`。
//
// ★ 每个用例都把 JavDB 指向一个**空 mock 服务**（`with_javdb_base`）。这不是
// 只为了打桩：没打桩而校验写漏了一处时，测试会去**真连 JavDB** —— 那是「测试
// 联网」，比断言失败糟糕得多。空 mock 下任何真请求都会得到 404 → `Unavailable`，
// 于是「拿到了别的码」本身就证明校验发生在发请求之前。

/// 榜单响应的公共形状（`success` 包络 + `data.movies`）。
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

/// 建一个指向假 JavDB 的出口实例。
fn host_service_against(db: &TestDb, base: &str) -> PluginHostService {
    host_service(db, PLUGIN).with_javdb_base(base)
}

fn playback_request(filter_by: &str, period: &str) -> GetJavdbRankNumbersRequest {
    GetJavdbRankNumbersRequest {
        username: None,
        password: None,
        query: Some(Query::Playback(JavdbPlaybackRankQuery {
            filter_by: filter_by.to_owned(),
            period: period.to_owned(),
        })),
    }
}

/// ★ 一次播放榜查询走通：oneof 分发到对的端点，番号**原序**返回（顺序即排名）。
#[tokio::test]
async fn a_playback_rank_query_is_dispatched_to_the_playback_endpoint() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rankings/playback"))
        .and(query_param("filter_by", "high_score"))
        .and(query_param("period", "weekly"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rank_body(&["A-1", "A-2"])))
        .mount(&server)
        .await;

    let response = host_service_against(&db, &server.uri())
        .get_javdb_rank_numbers(Request::new(playback_request("high_score", "weekly")))
        .await
        .expect("取榜")
        .into_inner();
    assert_eq!(response.movie_numbers, vec!["A-1", "A-2"]);
}

/// ★ **空榜单是成功**，不是错误。
///
/// 这是这一组里最容易写反的一条：把空榜判成失败会让「历史上这个榜就没有数据」
/// 变成一个每天重试、永远好不了的错误。TOP250 的历史年份、任何当日无数据的榜
/// 都会返回空。
#[tokio::test]
async fn an_empty_board_is_a_success_not_an_error() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rankings/playback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rank_body(&[])))
        .mount(&server)
        .await;

    let response = host_service_against(&db, &server.uri())
        .get_javdb_rank_numbers(Request::new(playback_request("all", "daily")))
        .await
        .expect("空榜单是成功");
    assert!(response.into_inner().movie_numbers.is_empty());
}

// ══════════════════════════════════════════ 排行榜**写侧**（`SyncRanking*`）
//
// 这一组钉的是「宿主拿到番号之后做的事」：名次怎么定、库里没有的番号怎么办、
// 重抓时旧名次会不会留下、插件挂了会不会把线上榜单清空、以及**归属**。
//
// 网关是打桩的（不连 gRPC）—— gRPC 那一段由 `plugin_launch_smoke.rs` 的真插件
// 覆盖，这里要看的是语义，不是通道。

/// 打桩网关。
struct StubGateway {
    /// `(board_key, period) -> 番号列表`。**顺序即排名**。
    numbers: HashMap<(String, String), Vec<String>>,
    /// `resolve_periods` 回什么。
    periods: Vec<String>,
    /// `true` 时两个方法都报错（模拟插件挂了 / 连不上）。
    failing: bool,
    /// 记下 `resolve_periods` 收到的 `periods_with_items`，断言宿主递对了。
    ///
    /// 是 `Arc`：`ranking_host` 把它换成测试手里那一把锁，测试才看得到。
    seen_periods_with_items: Arc<Mutex<Vec<String>>>,
}

impl StubGateway {
    fn new(periods: &[&str]) -> Self {
        Self {
            numbers: HashMap::new(),
            periods: periods.iter().map(|p| (*p).to_owned()).collect(),
            failing: false,
            seen_periods_with_items: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 换成外面那把锁（好让测试读得到 `resolve_periods` 收到了什么）。
    fn watching(mut self, seen: &Arc<Mutex<Vec<String>>>) -> Self {
        self.seen_periods_with_items = Arc::clone(seen);
        self
    }

    fn failing() -> Self {
        Self {
            failing: true,
            ..Self::new(&[])
        }
    }

    /// 给某个 `(board, period)` 配一串番号（顺序即排名）。
    fn serving(mut self, board_key: &str, period: &str, numbers: &[&str]) -> Self {
        self.numbers.insert(
            (board_key.to_owned(), period.to_owned()),
            numbers.iter().map(|n| (*n).to_owned()).collect(),
        );
        self
    }
}

impl RankingGateway for StubGateway {
    fn fetch_ranking<'a>(
        &'a self,
        _source_key: &'a str,
        board_key: &'a str,
        period: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>> {
        let outcome = if self.failing {
            Err(RankingCallError::new(
                "extension_call_failed",
                "打桩：插件挂了",
            ))
        } else {
            Ok(self
                .numbers
                .get(&(board_key.to_owned(), period.to_owned()))
                .cloned()
                .unwrap_or_default())
        };
        Box::pin(async move { outcome })
    }

    fn resolve_periods<'a>(
        &'a self,
        _source_key: &'a str,
        _board_key: &'a str,
        periods_with_items: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>> {
        *self.seen_periods_with_items.lock().expect("没中毒") = periods_with_items.to_vec();
        let outcome = if self.failing {
            Err(RankingCallError::new(
                "extension_call_failed",
                "打桩：插件挂了",
            ))
        } else {
            Ok(self.periods.clone())
        };
        Box::pin(async move { outcome })
    }
}

/// 一份只有一个源、一个榜单的目录（`dynamic_periods = false`）。
fn stub_catalog(owner: &str, board_key: &str, periods: &[&str]) -> RankingSourceCatalog {
    RankingSourceCatalog::new(vec![RankingSourceDefinition {
        source_key: "stub".to_owned(),
        title: "打桩榜单".to_owned(),
        owner_plugin_id: owner.to_owned(),
        boards: vec![RankingBoardDefinition {
            board_key: board_key.to_owned(),
            title: "日榜".to_owned(),
            supported_periods: periods.iter().map(|p| (*p).to_owned()).collect(),
            default_period: periods.first().copied().unwrap_or_default().to_owned(),
            dynamic_periods: false,
        }],
    }])
}

/// 造一个填好排行槽的能力出口，外加一个条目录入器（断言行用）。
fn ranking_host(
    db: &TestDb,
    catalog: RankingSourceCatalog,
    gateway: StubGateway,
    seen: &Arc<Mutex<Vec<String>>>,
) -> (PluginHostService, RankingItemRepository) {
    let slot = RankingSyncSlot::new();
    slot.fill(Arc::new(
        RankingSyncService::new(db.pool().clone(), catalog)
            .with_gateway(Arc::new(gateway.watching(seen))),
    ));
    (
        host_service_with(db, PLUGIN, slot),
        RankingItemRepository::new(db.pool().clone()),
    )
}

/// ★ 名次取自**插件给的顺序**（第 1 个就是第 1 名），库里已有的番号直接复用。
///
/// 两处容易写反：
/// - 名次是 `enumerate` 从 1 开始，**不是**按番号排序、也不是「跳过的也占位」；
/// - 番号不在库里是**跳过**（上游在此处拉 JavDB 详情导入，本仓详情接口还没落地
///   —— 见模块文档的「已知差异」），跳过的那条**不占名次**。
#[tokio::test]
async fn ranking_sync_keeps_the_plugin_order_and_reuses_local_movies() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    let first = movies.insert(&movie("AAA-001")).await.expect("插入");
    let third = movies.insert(&movie("AAA-003")).await.expect("插入");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &["AAA-001", "AAA-002", "AAA-003"]),
        &seen,
    );

    let response = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect("同步应当成功")
        .into_inner();

    assert_eq!(response.fetched_numbers, 3, "插件回了 3 个番号");
    assert_eq!(response.local_hit_movies, 2, "AAA-001 与 AAA-003 在库里");
    assert_eq!(
        response.skipped_movies, 1,
        "AAA-002 不在库里、也导不进来，按上游的失败分支计 skipped"
    );
    assert_eq!(response.stored_items, 2);
    assert_eq!(response.period, "daily", "回的是规整后的周期");

    let items = rows
        .list_by_board("stub", "daily_rank", "daily")
        .await
        .expect("读回条目");
    assert_eq!(items.len(), 2, "只有两条能写进去");
    assert_eq!(
        (
            items[0].rank,
            items[0].movie_number.as_str(),
            items[0].movie_id
        ),
        (1, "AAA-001", first.id),
        "第 1 个番号是第 1 名"
    );
    assert_eq!(
        (
            items[1].rank,
            items[1].movie_number.as_str(),
            items[1].movie_id
        ),
        (3, "AAA-003", third.id),
        "第 3 个番号是第 3 名 —— 跳过的那条**不占名次**"
    );
}

/// ★ 重抓时旧名次必须被**删掉**（整榜替换，不是逐条 upsert）。
///
/// 榜单从 3 条缩到 2 条时，第 3 名如果留着，就会以一个「已经不在榜上」的幽灵
/// 条目继续进推荐打分 —— 而上游 `_replace_scope_items` 先删后插正是为了这个。
#[tokio::test]
async fn a_re_ranking_drops_the_ranks_that_are_gone() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    for number in ["BBB-001", "BBB-002", "BBB-003"] {
        movies.insert(&movie(number)).await.expect("插入");
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &["BBB-001", "BBB-002", "BBB-003"]),
        &seen,
    );
    host.sync_ranking_board(Request::new(SyncRankingBoardRequest {
        source_key: "stub".to_owned(),
        board_key: "daily_rank".to_owned(),
        period: "daily".to_owned(),
    }))
    .await
    .expect("第一次同步");
    assert_eq!(
        rows.list_by_board("stub", "daily_rank", "daily")
            .await
            .expect("读回")
            .len(),
        3
    );

    // 换一个只回两条的网关（同一个目录、同一个插件）。
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &["BBB-001", "BBB-002"]),
        &seen,
    );
    let response = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect("第二次同步")
        .into_inner();
    assert_eq!(response.stored_items, 2);

    let items = rows
        .list_by_board("stub", "daily_rank", "daily")
        .await
        .expect("读回");
    assert_eq!(items.len(), 2, "第 3 名要被删掉，不是留着");
    assert!(
        !items.iter().any(|item| item.movie_number == "BBB-003"),
        "旧的第 3 名不该还在：{items:?}"
    );
}

/// ★ **空榜单是成功**，而且要把这个 scope **清空**。
///
/// 两条都容易写反：把空榜判成错误 → 「榜单下架」永远同步不掉；把空榜当成
/// 「这次不抓」而跳过替换 → 旧条目永远留着。
#[tokio::test]
async fn an_empty_board_is_a_success_and_clears_the_scope() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    movies.insert(&movie("CCC-001")).await.expect("插入");

    let seen = Arc::new(Mutex::new(Vec::new()));
    // 第一次同步只关心「先放一条进去」，行由第二次那个 `rows` 读。
    let (host, _rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &["CCC-001"]),
        &seen,
    );
    host.sync_ranking_board(Request::new(SyncRankingBoardRequest {
        source_key: "stub".to_owned(),
        board_key: "daily_rank".to_owned(),
        period: "daily".to_owned(),
    }))
    .await
    .expect("先放一条进去");

    // 同一个插件、同一个榜，这次回空。
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &[]),
        &seen,
    );
    let response = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect("空榜单是成功")
        .into_inner();

    assert_eq!(response.fetched_numbers, 0);
    assert_eq!(response.stored_items, 0);
    assert!(
        rows.list_by_board("stub", "daily_rank", "daily")
            .await
            .expect("读回")
            .is_empty(),
        "空榜要清空旧条目"
    );
}

/// ★ 插件挂了**绝不能**把线上榜单清空。
///
/// 取数失败与「空榜」是两件事：失败时一行都不能动。写反了就是「JavDB 抽风一天，
/// 用户的榜单全空」。
#[tokio::test]
async fn a_gateway_failure_leaves_the_old_board_alone() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    movies.insert(&movie("DDD-001")).await.expect("插入");

    let seen = Arc::new(Mutex::new(Vec::new()));
    // 同上：行由失败那一次之后那个 `rows` 读（要断言旧条目还在）。
    let (host, _rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&[]).serving("daily_rank", "daily", &["DDD-001"]),
        &seen,
    );
    host.sync_ranking_board(Request::new(SyncRankingBoardRequest {
        source_key: "stub".to_owned(),
        board_key: "daily_rank".to_owned(),
        period: "daily".to_owned(),
    }))
    .await
    .expect("先放一条进去");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::failing(),
        &seen,
    );
    let error = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect_err("插件挂了该报错");
    assert_eq!(error.code(), tonic::Code::Unavailable, "{error:?}");
    assert_eq!(
        rows.list_by_board("stub", "daily_rank", "daily")
            .await
            .expect("读回")
            .len(),
        1,
        "取数失败时旧榜单要原样留着"
    );
}

/// ★ 插件只能同步**自己**的源（上游 `context.py:1488-1503` 对越界直接 `ValueError`）。
#[tokio::test]
async fn a_plugin_cannot_sync_another_plugins_source() {
    let db = TestDb::require().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, _rows) = ranking_host(
        &db,
        stub_catalog("someone_else", "daily_rank", &["daily"]),
        StubGateway::new(&["daily"]),
        &seen,
    );

    let error = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect_err("别人的源不该能同步");
    assert_eq!(error.code(), tonic::Code::PermissionDenied, "{error:?}");
}

/// ★ 一个源都没有的插件调 `sync_ranking_sources` 是**错误**，不是「同步了 0 个」。
///
/// 上游 `context.py:1481-1483` 在这里抛 `RuntimeError`。做成成功的话，「插件把
/// 排行源注册丢了」会表现成「同步成功、0 个目标」—— 最难查的那种故障。
#[tokio::test]
async fn syncing_with_no_owned_sources_is_a_precondition_failure() {
    let db = TestDb::require().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, _rows) = ranking_host(
        &db,
        stub_catalog("someone_else", "daily_rank", &["daily"]),
        StubGateway::new(&["daily"]),
        &seen,
    );

    let error = host
        .sync_ranking_sources(Request::new(SyncRankingSourcesRequest {}))
        .await
        .expect_err("没有自己的源该报错");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error:?}");
}

/// ★ 全量同步：宿主把**自己知道的**「哪些周期已经有条目」递给插件，再按插件回的
/// 周期列表逐个抓。
///
/// 上游 `should_fetch(period, has_items)` 里 `has_items` 那一半在宿主手上，
/// 所以这一条「递过去」的链路断了就没人能判「历史年份不用重抓」。
#[tokio::test]
async fn the_host_tells_the_plugin_which_periods_already_have_items() {
    let db = TestDb::require().await;
    let movies = MovieRepository::new(db.pool().clone());
    movies.insert(&movie("EEE-001")).await.expect("插入");
    let rows = RankingItemRepository::new(db.pool().clone());
    rows.upsert(&NewRankingItem {
        source_key: "stub".to_owned(),
        board_key: "daily_rank".to_owned(),
        period: "daily".to_owned(),
        rank: 1,
        movie_number: "EEE-001".to_owned(),
        movie_id: movies
            .find_by_number("EEE-001")
            .await
            .expect("查")
            .expect("在库里")
            .id,
    })
    .await
    .expect("先放一条");

    // 插件回「还要抓 daily」—— 于是它是被**重新**抓一次的（插件自己决定）。
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (host, rows) = ranking_host(
        &db,
        stub_catalog(PLUGIN, "daily_rank", &["daily"]),
        StubGateway::new(&["daily"]).serving("daily_rank", "daily", &["EEE-001"]),
        &seen,
    );
    let response = host
        .sync_ranking_sources(Request::new(SyncRankingSourcesRequest {}))
        .await
        .expect("全量同步应当成功")
        .into_inner();

    assert_eq!(response.total_targets, 1, "一个榜单 × 一个周期");
    assert_eq!(response.synced_count, 1);
    assert_eq!(response.failed_targets, 0);
    assert_eq!(response.stored_items, 1);
    assert_eq!(
        *seen.lock().expect("没中毒"),
        vec!["daily".to_owned()],
        "宿主要把「daily 已经有条目了」递给插件 —— 插件的 should_fetch 靠它"
    );
    assert_eq!(
        rows.list_by_board("stub", "daily_rank", "daily")
            .await
            .expect("读回")
            .len(),
        1
    );
}

/// 槽**没填**时两个 rpc 都明确失败 —— 不能假成功成「0 个目标」。
#[tokio::test]
async fn an_unfilled_ranking_slot_fails_loudly() {
    let db = TestDb::require().await;
    let host = host_service(&db, PLUGIN);

    let error = host
        .sync_ranking_sources(Request::new(SyncRankingSourcesRequest {}))
        .await
        .expect_err("槽没填就该报错");
    assert_eq!(error.code(), tonic::Code::Unavailable, "{error:?}");

    let error = host
        .sync_ranking_board(Request::new(SyncRankingBoardRequest {
            source_key: "stub".to_owned(),
            board_key: "daily_rank".to_owned(),
            period: "daily".to_owned(),
        }))
        .await
        .expect_err("槽没填就该报错");
    assert_eq!(error.code(), tonic::Code::Unavailable, "{error:?}");
}

/// `query` 是 `oneof`：一个都没设时连「打哪个端点」都不知道 → `InvalidArgument`。
#[tokio::test]
async fn a_missing_query_is_an_invalid_argument_and_never_hits_the_network() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    let error = host_service_against(&db, &server.uri())
        .get_javdb_rank_numbers(Request::new(GetJavdbRankNumbersRequest {
            username: None,
            password: None,
            query: None,
        }))
        .await
        .expect_err("没有 query 该拒");
    assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error:?}");
    // 空 mock 上**一个请求都没发生**（否则会是 404 的 Unavailable）。
    assert!(
        server
            .received_requests()
            .await
            .expect("mock 可读")
            .is_empty(),
        "校验必须发生在发请求之前"
    );
}

/// 白名单外的参数 → `InvalidArgument`（**不是** `Unavailable`）。
///
/// 分类错了会让插件把「我自己映射写错了」当成「JavDB 暂时挂了」而一直重试。
#[tokio::test]
async fn unsupported_rank_arguments_are_invalid_arguments() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    let host = host_service_against(&db, &server.uri());

    // 未知 filter_by：`all` / `high_score` 之外的都该拒。
    let error = host
        .get_javdb_rank_numbers(Request::new(playback_request("recent", "daily")))
        .await
        .expect_err("filter_by=recent 该拒");
    assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error:?}");

    // 未知 period。
    let error = host
        .get_javdb_rank_numbers(Request::new(playback_request("all", "yearly")))
        .await
        .expect_err("period=yearly 该拒");
    assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error:?}");
}

/// TOP250 没带账号 → `FailedPrecondition`，且**不会**退化成「空榜」。
#[tokio::test]
async fn top250_without_an_account_is_a_failed_precondition() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    let error = host_service_against(&db, &server.uri())
        .get_javdb_rank_numbers(Request::new(GetJavdbRankNumbersRequest {
            username: None,
            password: None,
            query: Some(Query::Top(JavdbTopQuery {
                top_type: "all".to_owned(),
                type_value: String::new(),
                max_pages: None,
            })),
        }))
        .await
        .expect_err("没账号该拒");
    // ★ 不能是 `Ok(空榜)`：那会让「没配账号」表现成「TOP250 同步成功但一条都没有」。
    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error:?}");
}

/// HTTP 200 + `success != 1` 是**业务失败** → `Unavailable`（值得下一轮再试），
/// 而不是被当成空榜单。
#[tokio::test]
async fn a_business_failure_is_unavailable_not_an_empty_board() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/rankings/playback"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "success": 0, "message": "ParameterInvalid" })),
        )
        .mount(&server)
        .await;

    let error = host_service_against(&db, &server.uri())
        .get_javdb_rank_numbers(Request::new(playback_request("all", "daily")))
        .await
        .expect_err("success=0 该报错");
    assert_eq!(error.code(), tonic::Code::Unavailable, "{error:?}");
    assert!(error.message().contains("ParameterInvalid"), "{error:?}");
}
