//! `GET /playlists/{id}/movies` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! 影片卡片是「四张表 + 一条聚合查询」拼出来的，而每一个环节都能安静地错：
//!
//! - **字段集合**：上游 `PlaylistMovieListItemResource` = 23 个影片字段
//!   + 列表关系时间，多一个少一个都是契约变更；
//! - **排序**：`heat` / `release_date` 是列，`added_at` / `bitrate` 是**相关
//!   子查询** —— 子查询错位不会报错，只会让顺序看着「差不多」；
//! - **分辨率筛选**：档位是半开区间（`4K → [6, 7)`），写成闭区间会让 8K 影片
//!   同时命中 4K；
//! - **`can_play` 是 any**：一条有效媒体就够；
//! - **封面 origin 必须签名**：不签名客户端拿不到图，而状态码是 200。
//!
//! 每个用例的 `TestDb` 是独立 schema，所以「新列表是空的」这类断言不会被
//! 别的用例污染。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, NaiveDate, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    ImageRepository, MediaLibraryRepository, MediaRepository, MovieRepository, NewCollection,
    NewImage, NewMedia, NewMediaLibrary, NewMovie, NewUser, PlaylistMovieRepository,
    PlaylistRepository, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "playlist-movies-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 造数值型唯一后缀（番号、名字都用它）。
fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 只提供一个可写的临时配置（带签名密钥），避免碰到真实的 `config.toml`。
struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-plmovies-{tag}-{}", unique()));
        std::fs::create_dir_all(&base).expect("建临时目录");
        let config_path = base.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
        )
        .expect("写测试配置");
        Self { config_path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.config_path.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

async fn seed_token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("pm{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
    ))
}

fn authed(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; 原始: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

// ================================================================ 造数

async fn seed_playlist(db: &TestDb) -> i32 {
    PlaylistRepository::new(db.pool().clone())
        .insert(&NewCollection::host_owned(
            &format!("pm-列表-{}", unique()),
            "",
        ))
        .await
        .expect("insert playlist")
        .id
}

/// 造一部影片，返回 `(id, 番号)`。
async fn seed_movie(db: &TestDb, title: &str) -> (i32, String) {
    let number = format!("PM-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: title.to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 120,
            score: 0.0,
            score_number: 0,
            series_id: None,
            cover_image_id: None,
            thin_cover_image_id: None,
            metadata_source: None,
        })
        .await
        .expect("insert movie");
    (movie.id, number)
}

/// 把影片挂进列表。
async fn link(db: &TestDb, playlist_id: i32, movie_id: i32) {
    PlaylistMovieRepository::new(db.pool().clone())
        .add(playlist_id, movie_id)
        .await
        .expect("link movie");
}

async fn seed_series(db: &TestDb, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("INSERT INTO movie_series (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("insert series")
}

async fn seed_image(db: &TestDb, origin: &str) -> i32 {
    ImageRepository::new(db.pool().clone())
        .upsert(&NewImage {
            origin: origin.to_owned(),
        })
        .await
        .expect("insert image")
        .0
}

async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("pm-lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id
}

/// 给影片挂一条媒体。
async fn seed_media(
    db: &TestDb,
    number: &str,
    library_id: i32,
    resolution: &str,
    video_info: Option<Value>,
) -> i32 {
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("pm-{}-{}.mp4", n(), unique()),
            file_size_bytes: 2048,
            movie_number: Some(number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some(resolution.to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 90,
            video_info,
        })
        .await
        .expect("insert media")
        .id
}

/// 直接改 `movie.heat` —— 仓储层不暴露它（由业务流程累加）。
async fn set_heat(db: &TestDb, movie_id: i32, heat: i32) {
    sqlx::query("UPDATE movie SET heat = $2 WHERE id = $1")
        .bind(movie_id)
        .bind(heat)
        .execute(db.pool())
        .await
        .expect("set heat");
}

// ================================================================ 用例

#[tokio::test]
async fn an_unknown_playlist_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("404");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/playlists/2147483647/movies", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(code_of(&body), "playlist_not_found");
    assert_eq!(
        body["error"]["details"]["playlist_id"],
        json!(2147483647_i64)
    );
}

#[tokio::test]
async fn an_empty_playlist_returns_an_empty_page_with_total_zero() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/playlists/{playlist_id}/movies"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["total"], json!(0));
    // 四个分页字段缺省值照抄上游。
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["page_size"], json!(20));
}

#[tokio::test]
async fn the_card_is_the_upstream_resource() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    let series_id = seed_series(&db, &format!("PM 系列-{}", unique())).await;
    let cover_id = seed_image(&db, &format!("pm/covers/{}.jpg", unique())).await;
    let thin_id = seed_image(&db, &format!("pm/thin/{}.jpg", unique())).await;
    let (movie_id, number) = seed_movie(&db, "卡片标题").await;
    sqlx::query(
        "UPDATE movie SET series_id = $2, cover_image_id = $3, thin_cover_image_id = $4, \
         release_date = $5 WHERE id = $1",
    )
    .bind(movie_id)
    .bind(series_id)
    .bind(cover_id)
    .bind(thin_id)
    .bind(
        NaiveDate::from_ymd_opt(2024, 5, 6)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    )
    .execute(db.pool())
    .await
    .expect("补齐影片列");
    link(&db, playlist_id, movie_id).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/playlists/{playlist_id}/movies"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let card = &body["items"][0];

    // 字段集合：上游 `MovieListItemResource` 的 23 个 + 列表关系时间。
    let mut keys: Vec<&str> = card
        .as_object()
        .expect("卡片是对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "can_play",
            "comment_count",
            "cover_image",
            "duration_minutes",
            "heat",
            "id",
            "is_blacklisted",
            "is_collection",
            "is_subscribed",
            "javdb_id",
            "media_count",
            "media_items",
            "metadata_source",
            "movie_number",
            "playlist_item_updated_at",
            "release_date",
            "score",
            "score_number",
            "series_id",
            "series_name",
            "thin_cover_image",
            "title",
            "want_watch_count",
            "watched_count",
        ],
        "卡片的字段集合变了"
    );

    assert_eq!(card["id"], json!(movie_id));
    assert_eq!(card["movie_number"], json!(number));
    assert_eq!(card["title"], json!("卡片标题"));
    assert_eq!(card["series_id"], json!(series_id));
    assert!(card["series_name"].as_str().is_some(), "应带系列名");
    assert_eq!(card["release_date"], json!("2024-05-06"));
    assert_eq!(card["duration_minutes"], json!(120));
    assert_eq!(card["javdb_id"], json!(null));
    assert_eq!(card["metadata_source"], json!(null));
    assert_eq!(card["media_items"], json!([]));
    assert_eq!(card["media_count"], json!(0));
    assert_eq!(card["can_play"], json!(false), "没有媒体就不能播");

    // 封面 origin 必须**签名**（原样返回客户端拿不到图，而状态码是 200）。
    let cover = card["cover_image"]["origin"]
        .as_str()
        .expect("封面应带 origin");
    assert!(
        cover.starts_with("/files/images/"),
        "封面 origin 没走签名路由：{cover}"
    );
    assert!(cover.contains("expires=") && cover.contains("signature="));
    assert_ne!(card["cover_image"]["id"], card["thin_cover_image"]["id"]);
}

#[tokio::test]
async fn media_items_carry_the_summary_and_the_derived_counts() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("media");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;
    let library_id = seed_library(&db).await;

    let (movie_id, number) = seed_movie(&db, "有媒体的影片").await;
    link(&db, playlist_id, movie_id).await;
    let media_id = seed_media(
        &db,
        &number,
        library_id,
        "1920x1080",
        Some(json!({ "video": { "bit_rate": "4500000" } })),
    )
    .await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/playlists/{playlist_id}/movies"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let card = &body["items"][0];
    assert_eq!(card["media_count"], json!(1));
    assert_eq!(card["can_play"], json!(true), "一条有效媒体就够");

    let media = &card["media_items"][0];
    assert_eq!(media["media_id"], json!(media_id));
    assert_eq!(media["library_id"], json!(library_id));
    assert_eq!(media["resolution"], json!("1920x1080"));
    assert_eq!(media["file_size_bytes"], json!(2048));
    assert_eq!(media["duration_seconds"], json!(90));
    assert_eq!(media["valid"], json!(true));
    // `video_info` 是**对象**而不是字符串（上游是 `dict[str, Any]`）。
    assert_eq!(media["video_info"]["video"]["bit_rate"], json!("4500000"));
}

#[tokio::test]
async fn the_resolution_filter_uses_half_open_buckets() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("res");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;
    let library_id = seed_library(&db).await;

    let (hd_id, hd_number) = seed_movie(&db, "1080P 影片").await;
    let (uhd_id, uhd_number) = seed_movie(&db, "4K 影片").await;
    let (none_id, _) = seed_movie(&db, "没有媒体的影片").await;
    for id in [hd_id, uhd_id, none_id] {
        link(&db, playlist_id, id).await;
    }
    seed_media(&db, &hd_number, library_id, "1920x1080", None).await;
    seed_media(&db, &uhd_number, library_id, "3840x2160", None).await;

    // `1080P -> [4, 5)`：4K 影片(6) 不该落进来。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?resolution=1080P"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(1), "应该只有 1080P 那部：{body}");
    assert_eq!(body["items"][0]["movie_number"], json!(hd_number));

    // `4K -> [6, 7)`：只有 4K 影片。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?resolution=4k"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["movie_number"], json!(uhd_number));

    // 不筛时三部都在 —— 没有媒体的影片**不计入任何档位**。
    let (_, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/playlists/{playlist_id}/movies"), &token),
    )
    .await;
    assert_eq!(body["total"], json!(3));
}

#[tokio::test]
async fn sorting_by_heat_orders_and_breaks_ties_by_id() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("sort");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    let (low_id, low_number) = seed_movie(&db, "低热度").await;
    let (high_id, high_number) = seed_movie(&db, "高热度").await;
    link(&db, playlist_id, low_id).await;
    link(&db, playlist_id, high_id).await;
    set_heat(&db, low_id, 3).await;
    set_heat(&db, high_id, 99).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?sort=heat:desc"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["items"][0]["movie_number"], json!(high_number));
    assert_eq!(body["items"][1]["movie_number"], json!(low_number));

    let (_, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?sort=HEAT:ASC"),
            &token,
        ),
    )
    .await;
    assert_eq!(body["items"][0]["movie_number"], json!(low_number));
}

#[tokio::test]
async fn pagination_reports_the_filtered_total() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("page");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    for index in 0..3 {
        let (movie_id, _) = seed_movie(&db, &format!("分页影片-{index}")).await;
        link(&db, playlist_id, movie_id).await;
    }

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?page=1&page_size=2"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    // `total` 是**过滤后的总数**，不是本页条数 —— 否则客户端不会翻第二页。
    assert_eq!(body["total"], json!(3));
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["page_size"], json!(2));

    let (_, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?page=2&page_size=2"),
            &token,
        ),
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], json!(3));
}

#[tokio::test]
async fn a_bad_sort_expression_is_422_with_the_raw_value() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("badsort");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    for bad in ["title:desc", "heat", "heat:up", ":desc", "heat:desc:extra"] {
        let (status, body) = send(
            app(&db, &fixture),
            authed(
                "GET",
                &format!("/playlists/{playlist_id}/movies?sort={bad}"),
                &token,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad:?} 应当是 422，响应: {body}"
        );
        assert_eq!(code_of(&body), "invalid_playlist_filter");
        // details 回显**原始输入**。
        assert_eq!(body["error"]["details"]["sort"], json!(bad));
    }
}

#[tokio::test]
async fn a_bad_resolution_is_422_with_its_own_detail_key() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("badres");
    let token = seed_token(db.pool()).await;
    let playlist_id = seed_playlist(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/playlists/{playlist_id}/movies?resolution=8k2"),
            &token,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    // 与排序**共用错误码**，靠 details 的键区分是哪个参数错了。
    assert_eq!(code_of(&body), "invalid_playlist_filter");
    assert_eq!(body["error"]["details"]["resolution"], json!("8k2"));
}
