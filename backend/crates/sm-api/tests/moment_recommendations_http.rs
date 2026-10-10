//! `GET /moment-recommendations` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! 读侧要跨四张表装配（`moment_recommendation` × `media` × `movie` ×
//! `media_thumbnail` × `image`），每一件事都能「看起来对」：
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 元素形状 | **嵌套** `image` + `movie`，不是拍平的影片字段 | 上游 `MomentRecommendationItemResource` |
//! | `image.origin` | 签名后的 URL（`/files/images/…`） | 未签名的相对路径客户端取不到图 |
//! | 失效媒体 / 拉黑影片 | `items` 与 `total` **一起**少 | `SELECT` 与 `COUNT` 必须同一套过滤 |
//! | 缩略图被删 | 推荐行**被级联删掉**（`total` 一起少） | 三列都是 `ON DELETE CASCADE`，所以「取不到图的孤儿」不存在 |
//! | `generated_at` | 全表最新（**不带**有效过滤） | 上游 `:521-528` |
//! | `page_size=101` | 422 `invalid_moment_recommendation_filter` | 服务层 `validate_page`，专用码 |
//! | `?page=abc` | 422 **错误信封** | 查询参数也要走信封提取器 |
//!
//! 每个用例的 `TestDb` 是独立 schema。
//!
//! # 为什么断言「级联」而不是「跳过」
//!
//! 服务层的「取不到图/卡片就跳过且不补位」**没有上游数据可造**：三列外键都是
//! `ON DELETE CASCADE`，`list_valid` 又对 `media`/`movie` 都是 INNER JOIN。
//! 一开始按「删掉缩略图 → 留一条取不到图的孤儿」写了用例，真库上直接红了 ——
//! 它把「级联」误当成了「跳过」。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::common::time::now_utc;
// ⚠️ 从 `repo::moment` 取而不是 `repo::`：`moment_recommendation` 这张表有
// **两份**同名仓储（`repo::moment` 与 `repo::recommendation`），而
// `repo/mod.rs` 只重导出后者。读侧服务用的是 `repo::moment` 这一份
// （`list_valid` / `count_valid` / `latest_generated_at`），所以这里也必须
// 用同一份 —— 否则「写进去的行读不出来」会以一种极难查的方式出现。
use sm_db::repo::moment::{MomentRecommendationRepository, NewMomentRecommendation};
use sm_db::repo::{MovieRepository, NewMovie, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "moment-rec-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-moment-{tag}-{}", unique()));
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
            username: format!("mr{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(
        i64::from(user.id),
        chrono::Utc::now() + chrono::Duration::hours(1),
        SECRET,
    )
}

fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
    ))
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
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
    body["error"]["code"].as_str().unwrap_or_default()
}

// ================================================================ 造数

/// 造一部影片，返回 `(id, 番号)`。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("MR-{:06}-{}", n(), unique() % 100_000);
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "时刻推荐测试影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 0,
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

async fn seed_library(db: &TestDb) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO media_library (name, provider_key) VALUES ($1, 'local') RETURNING id",
    )
    .bind(format!("lib-{}", unique()))
    .fetch_one(db.pool())
    .await
    .expect("insert media_library");
    id
}

/// 造一条媒体（`valid = true`），返回 `(media_id, thumbnail_id)` 所需的一半。
async fn seed_media(db: &TestDb, library_id: i32, movie_number: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO media (movie_number, library_id, thumbnail_generation_state, duration_seconds)
         VALUES ($1, $2, 'succeeded', 3600) RETURNING id",
    )
    .bind(movie_number)
    .bind(library_id)
    .fetch_one(db.pool())
    .await
    .expect("insert media");
    id
}

/// 造一张图片，返回 `(image_id, origin)`。
async fn seed_image(db: &TestDb, origin: &str) -> (i32, String) {
    let (id,): (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(origin)
        .fetch_one(db.pool())
        .await
        .expect("insert image");
    (id, origin.to_owned())
}

/// 造一张缩略图，返回 `thumbnail_id`。
async fn seed_thumbnail(db: &TestDb, media_id: i32, image_id: i32, offset: i32) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO media_thumbnail (media_id, image_id, \"offset\", image_search_index_status)
         VALUES ($1, $2, $3, 0) RETURNING id",
    )
    .bind(media_id)
    .bind(image_id)
    .bind(offset)
    .fetch_one(db.pool())
    .await
    .expect("insert media_thumbnail");
    id
}

async fn set_media_valid(db: &TestDb, media_id: i32, valid: bool) {
    sqlx::query("UPDATE media SET valid = $2 WHERE id = $1")
        .bind(media_id)
        .bind(valid)
        .execute(db.pool())
        .await
        .expect("set media.valid");
}

async fn set_blacklisted(db: &TestDb, movie_id: i32, blacklisted: bool) {
    sqlx::query("UPDATE movie SET is_blacklisted = $2 WHERE id = $1")
        .bind(movie_id)
        .bind(blacklisted)
        .execute(db.pool())
        .await
        .expect("set is_blacklisted");
}

/// 造一条推荐（**整表替换**，本表唯一正确的写入方式）。
async fn seed_snapshot(db: &TestDb, rows: &[NewMomentRecommendation]) {
    MomentRecommendationRepository::new(db.pool().clone())
        .replace_all(rows, now_utc())
        .await
        .expect("replace_all 快照");
}

/// 一条推荐的完整造数链：影片 + 媒体 + 图片 + 缩略图 → 返回 `(movie_id, media_id, thumbnail_id, image_id, origin, movie_number)`。
struct Seeded {
    movie_id: i32,
    media_id: i32,
    thumbnail_id: i32,
    image_id: i32,
    origin: String,
    movie_number: String,
}

async fn seed_chain(db: &TestDb, library_id: i32) -> Seeded {
    let (movie_id, movie_number) = seed_movie(db).await;
    let media_id = seed_media(db, library_id, &movie_number).await;
    let origin = format!("moments/{}-{}.jpg", n(), unique());
    let (image_id, origin) = seed_image(db, &origin).await;
    let thumbnail_id = seed_thumbnail(db, media_id, image_id, 1800).await;
    Seeded {
        movie_id,
        media_id,
        thumbnail_id,
        image_id,
        origin,
        movie_number,
    }
}

fn recommendation(seeded: &Seeded, rank: i32, score: f64) -> NewMomentRecommendation {
    NewMomentRecommendation {
        rank,
        score,
        strategy: "visual".to_owned(),
        reason: "画面相似".to_owned(),
        movie_id: seeded.movie_id,
        media_id: seeded.media_id,
        thumbnail_id: seeded.thumbnail_id,
        offset_seconds: 1800,
        seed_point_id: None,
        seed_thumbnail_id: None,
        source_movie_id: None,
        visual_score: Some(0.9),
        movie_similarity_score: None,
    }
}

// ================================================================ 用例

#[tokio::test]
async fn an_empty_snapshot_returns_an_empty_page() {
    // 生成侧还没接（见 `routes/recommendations.rs`），空表是**正常状态**。
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(app(&db, &fixture), get("/moment-recommendations", &token)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["total"], 0);
    assert_eq!(body["generated_at"], Value::Null, "还没生成过就是 null");
    assert_eq!(body["page"], 1);
    assert_eq!(body["page_size"], 20);
}

#[tokio::test]
async fn an_item_carries_the_signed_thumbnail_and_the_nested_movie_card() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;

    let seeded = seed_chain(&db, library_id).await;
    seed_snapshot(&db, &[recommendation(&seeded, 1, 0.9)]).await;

    let (status, body) = send(app(&db, &fixture), get("/moment-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    let item = &items[0];

    // 快照行自己的 8 个字段。
    assert_eq!(item["rank"], 1);
    assert_eq!(item["score"], 0.9);
    assert_eq!(item["strategy"], "visual");
    assert_eq!(item["reason"], "画面相似");
    assert_eq!(item["media_id"], seeded.media_id);
    assert_eq!(item["thumbnail_id"], seeded.thumbnail_id);
    assert_eq!(item["offset_seconds"], 1800);
    assert!(item["recommendation_id"].as_i64().is_some(), "有 id");

    // `image` 是**嵌套对象**（不是拍平），且 origin 已签名。
    assert_eq!(item["image"]["id"], seeded.image_id);
    let signed = item["image"]["origin"].as_str().expect("origin 是字符串");
    assert!(
        signed.starts_with("/files/images/"),
        "origin 应是签名后的 URL，实际 {signed:?}"
    );
    assert_ne!(signed, seeded.origin, "不该把相对路径原样透出");

    // `movie` 是完整卡片。
    assert_eq!(item["movie"]["id"], seeded.movie_id);
    assert_eq!(item["movie"]["movie_number"], seeded.movie_number);
    assert_eq!(item["movie"]["title"], "时刻推荐测试影片");
    // 卡片里挂了**有效媒体** → `can_play` 为 true。上游 `:551` 那行
    // `base_resource.can_play = bool(getattr(movie, "can_play", False))`
    // 在本仓由卡片自身的媒体挂载提供（`MovieCard.media[*].can_play`）。
    assert_eq!(item["movie"]["can_play"], true);
    // 页级**没有**拍平的影片字段。
    assert!(body.get("title").is_none());
}

#[tokio::test]
async fn invalid_media_and_blacklisted_movies_leave_no_slot_in_pagination() {
    // 两条过滤（`media.valid` / `movie.is_blacklisted`）在 `SELECT` 与 `COUNT`
    // 里必须一致，否则失效行会占掉分页槽位：`total` 偏大、最后一页变短。
    let db = TestDb::require().await;
    let fixture = Fixture::new("filter");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;

    let invalid_media = seed_chain(&db, library_id).await;
    let blacklisted = seed_chain(&db, library_id).await;
    let shown = seed_chain(&db, library_id).await;
    seed_snapshot(
        &db,
        &[
            recommendation(&invalid_media, 1, 0.9),
            recommendation(&blacklisted, 2, 0.8),
            recommendation(&shown, 3, 0.7),
        ],
    )
    .await;
    set_media_valid(&db, invalid_media.media_id, false).await;
    set_blacklisted(&db, blacklisted.movie_id, true).await;

    let (status, body) = send(app(&db, &fixture), get("/moment-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "失效媒体与拉黑影片都不占总数");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["movie"]["movie_number"], shown.movie_number);
    assert_eq!(items[0]["rank"], 3);
}

/// ★ 缩略图被删 → 推荐行**被级联删掉**（不是「留一条取不到图的孤儿」）。
///
/// # 这条钉住一个**不存在**的状态，从而保护一段防御代码
///
/// 上游 `list_items:556-557` 有 `if thumbnail is None or movie is None: continue`
/// （服务层按此实现了「跳过且不补位」）。但在本仓的 schema 里这个状态
/// **造不出来**：
///
/// | 依赖 | 约束（`docker/schema.sql:522-526`） |
/// |---|---|
/// | `thumbnail_id` | `REFERENCES media_thumbnail(id) ON DELETE CASCADE` |
/// | `media_id` | `REFERENCES media(id) ON DELETE CASCADE` |
/// | `movie_id` | `REFERENCES movie(id) ON DELETE CASCADE` |
///
/// 三列都会级联；而 `list_valid` 又对 `media` / `movie` 都是 INNER JOIN
/// （`media.valid = true` 也一并过滤）。所以那两处 `continue` 只能接住
/// **两次查询之间的并发删除**（毫秒级窗口），不是一种持久状态。
///
/// 断言「级联」而不是「跳过」：把前者写成后者会让下一个看到 `continue`
/// 的人以为它有稳定触发路径（从而写出错的覆盖）。
#[tokio::test]
async fn a_deleted_thumbnail_takes_the_recommendation_row_with_it() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("cascade");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;

    let kept = seed_chain(&db, library_id).await;
    let removed = seed_chain(&db, library_id).await;
    seed_snapshot(
        &db,
        &[
            recommendation(&kept, 1, 0.9),
            recommendation(&removed, 2, 0.8),
        ],
    )
    .await;

    sqlx::query("DELETE FROM media_thumbnail WHERE id = $1")
        .bind(removed.thumbnail_id)
        .execute(db.pool())
        .await
        .expect("删缩略图");

    let (status, body) = send(app(&db, &fixture), get("/moment-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "级联删掉的那条不占总数");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["rank"], 1, "留下的是 rank=1 那条");
}

#[tokio::test]
async fn generated_at_is_the_latest_regardless_of_validity() {
    // `generated_at` **不带**有效过滤（上游 `:521-528`）：池子里最新的那批
    // 若全是失效媒体，时间戳仍然显示那个时间，而 `items` 是空的。
    let db = TestDb::require().await;
    let fixture = Fixture::new("generated");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;

    let seeded = seed_chain(&db, library_id).await;
    seed_snapshot(&db, &[recommendation(&seeded, 1, 0.9)]).await;
    set_media_valid(&db, seeded.media_id, false).await;

    let (status, body) = send(app(&db, &fixture), get("/moment-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]), "失效媒体不出现在 items");
    let generated_at = body["generated_at"].as_str().expect("generated_at 是串");
    assert!(
        generated_at.starts_with(&now_utc().format("%Y-%m-%dT").to_string()),
        "仍是全表最新时间，实际 {generated_at:?}"
    );
}

#[tokio::test]
async fn pagination_walks_rank_order() {
    // ⚠️ 这条钉住一个**只在有跨页数据时才显形**的错法：仓储的 `list_valid`
    // 第二个参数是 **offset**，把页码直接递进去会让 `page=1` 跳掉第一条。
    let db = TestDb::require().await;
    let fixture = Fixture::new("page");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;

    let first = seed_chain(&db, library_id).await;
    let second = seed_chain(&db, library_id).await;
    let third = seed_chain(&db, library_id).await;
    seed_snapshot(
        &db,
        &[
            recommendation(&first, 1, 0.9),
            recommendation(&second, 2, 0.7),
            recommendation(&third, 3, 0.5),
        ],
    )
    .await;

    let (status, page_one) = send(
        app(&db, &fixture),
        get("/moment-recommendations?page=1&page_size=2", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_one["total"], 3);
    assert_eq!(page_one["page"], 1);
    assert_eq!(page_one["page_size"], 2);
    let items = page_one["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["rank"], 1, "第一页第一条必须是 rank=1");
    assert_eq!(items[1]["rank"], 2);

    let (status, page_two) = send(
        app(&db, &fixture),
        get("/moment-recommendations?page=2&page_size=2", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page_two["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "跨页后只有 1 条");
    assert_eq!(items[0]["rank"], 3);
}

/// ★ 分页越界 → 422 **专用码**（不是 `validation_error`）。
///
/// 上游 moment 的查询参数没有 `le`，所以这一道拦在服务层 —— 与
/// daily / hot-actress（pydantic 先拦）的**码不同**，见路由模块文档第 1 条。
#[tokio::test]
async fn an_out_of_range_page_size_returns_the_dedicated_filter_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("range");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/moment-recommendations?page_size=101", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "invalid_moment_recommendation_filter");
    assert_eq!(body["error"]["details"]["page_size"], 101);
}

/// ★ 坏的查询参数值也要是**错误信封**，不是 axum 默认的 400 + 纯文本。
///
/// 这条在 `?page=abc` 上暴露：三个列表端点此前用的是裸 `axum::extract::Query`
/// （见 `crate::extract` 的模块文档 —— 那个包装正是为此而存在）。
#[tokio::test]
async fn a_bad_query_value_returns_the_error_envelope() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("bad-query");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/moment-recommendations?page=abc", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "不是 axum 的 400");
    assert_eq!(code_of(&body), "validation_error");
}
