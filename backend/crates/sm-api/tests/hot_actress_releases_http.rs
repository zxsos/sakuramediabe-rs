//! `GET /hot-actress-releases` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! 打分本身有纯函数单测（`hot_actress_release` 模块内）。这里测的是**打分
//! 之后到线格式**那一段，也就是骨架期一直是 `todo!()` 的部分：
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 元素形状 | 卡片字段**平铺**在元素上（`id` / `movie_number` / `title`），**不是** `movie_id` | 上游 `HotActressReleaseMovieResource` 继承卡片 |
//! | `recommendation_score` | 与 `hot_actress.hotness_score` **同值** | 上游两处都是 `round(score, 4)` |
//! | `hot_actress.profile_image` | **覆盖头像优先**（不是 `profile_image_id`） | 用户设的本地头像不生效是这条最容易漏的地方 |
//! | `historical_movie_count` | **扣掉影片自己** | 两个窗口重叠 30 天 |
//! | 黑名单影片 | 不入结果 | 候选查询与卡片查询都必须滤 |
//! | 历史作品不足 3 部 | 整条不入结果 | 上游是**跳过**不是降权 |
//! | 排序 | score 降序 | 上游 `ORDER BY score DESC` |
//! | `page_size=101` | 422 `invalid_hot_actress_release_filter` | 专用码 |
//!
//! # ★ 这组用例的第一个发现：两条 SQL 从来没跑过
//!
//! `repo/discovery.rs` 的 `history_actor_rows` / `candidate_rows` 把
//! `movie_actor` 的列写成了上游 Peewee 的**外键字段名**（`ma.movie` /
//! `ma.actor`），而实际列是 `movie_id` / `actor_id`。这两个方法在 2026-10-09
//! 之前**没有任何调用方**（端点还是 `todo!()`），所以这个错误从未暴露 ——
//! 第一次真库执行就是本文件。已修（见那里的注释）。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Local, NaiveDate, NaiveDateTime};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::discovery::hot_actress_release::{FEMALE_GENDER, MIN_HISTORICAL_MOVIES};
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "hot-actress-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-hot-{tag}-{}", unique()));
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
            username: format!("ha{}", unique()),
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

// ================================================================ 造数

/// 服务打分用的是**本地日期**（`HotActressReleaseQuery::today`），造数必须
/// 用同一个来源，否则窗口边缘会差一天。
fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn at_noon(days_from_today: i64) -> NaiveDateTime {
    (today() + Duration::days(days_from_today))
        .and_hms_opt(12, 0, 0)
        .expect("合法时刻")
}

async fn seed_actor(db: &TestDb, name: &str, gender: i32) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO actor (javdb_id, name, gender) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(format!("Ha-{}-{}", unique(), name))
    .bind(name)
    .bind(gender)
    .fetch_one(db.pool())
    .await
    .expect("insert actor");
    id
}

async fn seed_image(db: &TestDb, origin: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(origin)
        .fetch_one(db.pool())
        .await
        .expect("insert image");
    id
}

async fn seed_movie(db: &TestDb, released_at: NaiveDateTime, heat: i32, blacklisted: bool) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO movie (movie_number, title, release_date, heat, is_blacklisted)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(format!("HA-{:06}-{}", heat, unique() % 100_000))
    .bind(format!("热播女优测试片 heat={heat}"))
    .bind(released_at)
    .bind(heat)
    .bind(blacklisted)
    .fetch_one(db.pool())
    .await
    .expect("insert movie");
    id
}

async fn link(db: &TestDb, movie_id: i32, actor_id: i32) {
    sqlx::query("INSERT INTO movie_actor (movie_id, actor_id) VALUES ($1, $2)")
        .bind(movie_id)
        .bind(actor_id)
        .execute(db.pool())
        .await
        .expect("insert movie_actor");
}

/// 给女优铺 `count` 部**历史窗口内**的作品（t-120 天，heat 相同）。
///
/// 历史窗口是 `[today-180, today-60)`，所以 t-120 落在里面；而候选窗口是
/// `[today-90, today+90)`，t-120 **不在**里面 —— 于是候选影片不会与历史
/// 重叠，「扣掉自己」那条要另设用例（见 `historical_count_excludes_the_candidate_itself`）。
async fn seed_history(db: &TestDb, actor_id: i32, count: i64, heat: i32) {
    for _ in 0..count {
        let movie_id = seed_movie(db, at_noon(-120), heat, false).await;
        link(db, movie_id, actor_id).await;
    }
}

/// 女优的「热播新作」：候选窗口内的影片。
async fn seed_candidate(db: &TestDb, actor_id: i32, heat: i32, blacklisted: bool) -> i32 {
    let movie_id = seed_movie(db, at_noon(-30), heat, blacklisted).await;
    link(db, movie_id, actor_id).await;
    movie_id
}

// ================================================================ 用例

#[tokio::test]
async fn an_item_is_the_flat_movie_card_plus_the_winning_actress() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "上原", FEMALE_GENDER).await;
    seed_history(&db, actress, MIN_HISTORICAL_MOVIES, 100).await;
    let candidate = seed_candidate(&db, actress, 50, false).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    let item = &items[0];

    // ★ 卡片字段**平铺**在元素上（`#[serde(flatten)]`），没有 `movie_id`。
    assert_eq!(item["id"], candidate);
    assert!(item["movie_number"].is_string());
    assert!(item["title"].is_string());
    assert!(item.get("can_play").is_some());
    assert!(item.get("movie_id").is_none(), "影片不是嵌套对象");

    // 两个分同值（上游两处都取 `round(scored_movie.score, 4)`）。
    let recommendation = item["recommendation_score"].as_f64().expect("分数是数");
    assert_eq!(
        item["hot_actress"]["hotness_score"].as_f64(),
        Some(recommendation),
        "recommendation_score 与 hotness_score 必须同值"
    );
    assert!(recommendation > 0.0);

    assert_eq!(item["hot_actress"]["id"], actress);
    assert_eq!(item["hot_actress"]["name"], "上原");
    assert_eq!(
        item["hot_actress"]["display_name"], "上原",
        "没有覆盖时 display_name 回落到 name"
    );
    assert_eq!(
        item["hot_actress"]["historical_movie_count"],
        MIN_HISTORICAL_MOVIES
    );
    assert_eq!(
        item["hot_actress"]["profile_image"],
        Value::Null,
        "没铺头像"
    );
}

#[tokio::test]
async fn display_name_override_wins() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("override-name");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "上原", FEMALE_GENDER).await;
    sqlx::query("UPDATE actor SET display_name_override = $2 WHERE id = $1")
        .bind(actress)
        .bind("うえはら")
        .execute(db.pool())
        .await
        .expect("设 display_name_override");
    seed_history(&db, actress, MIN_HISTORICAL_MOVIES, 100).await;
    seed_candidate(&db, actress, 50, false).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["hot_actress"]["display_name"], "うえはら");
}

/// ★ 头像取的是**生效**头像：用户设的覆盖头像优先于 `profile_image_id`。
///
/// 这条链是 `repo::actor::profile_images_of` 的双 LEFT JOIN。漏掉覆盖分支
/// （直接取 `profile_image_id`）是个「一切正常、只是用户设的头像不生效」的
/// 错法 —— 除了这种端到端用例，别的地方看不出来。
#[tokio::test]
async fn the_effective_profile_image_prefers_the_override() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("override-image");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "上原", FEMALE_GENDER).await;
    let base = seed_image(&db, "actors/base.jpg").await;
    let override_image = seed_image(&db, "actors/override.jpg").await;
    sqlx::query(
        "UPDATE actor SET profile_image_id = $2, profile_image_override_id = $3 WHERE id = $1",
    )
    .bind(actress)
    .bind(base)
    .bind(override_image)
    .execute(db.pool())
    .await
    .expect("设两张头像");

    seed_history(&db, actress, MIN_HISTORICAL_MOVIES, 100).await;
    seed_candidate(&db, actress, 50, false).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let image = &body["items"][0]["hot_actress"]["profile_image"];
    assert_eq!(image["id"], override_image, "覆盖头像应当胜出");
    let origin = image["origin"].as_str().expect("origin 是字符串");
    assert!(
        origin.starts_with("/files/images/"),
        "头像也要签名，实际 {origin:?}"
    );
}

/// `historical_movie_count` **扣掉影片自己**：候选窗口与历史窗口重叠 30 天，
/// 落在重叠区的候选影片既在证据里、又被当成新作。
#[tokio::test]
async fn historical_count_excludes_the_candidate_itself() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("self");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "上原", FEMALE_GENDER).await;
    // 3 部历史（t-120）+ 候选本身落在**重叠区**（t-80：历史窗口 [t-180,t-60)
    // 与候选窗口 [t-90,t+90) 都在），所以证据里一共 4 部。
    seed_history(&db, actress, MIN_HISTORICAL_MOVIES, 100).await;
    let overlapping = seed_movie(&db, at_noon(-80), 100, false).await;
    link(&db, overlapping, actress).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["id"], overlapping);
    assert_eq!(
        body["items"][0]["hot_actress"]["historical_movie_count"], MIN_HISTORICAL_MOVIES,
        "4 部证据里有 1 部是它自己"
    );
}

/// 历史作品不足 `MIN_HISTORICAL_MOVIES` → **整条不入结果**（跳过，不是降权）。
#[tokio::test]
async fn an_actress_below_the_history_floor_is_not_a_candidate() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("floor");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "新人", FEMALE_GENDER).await;
    seed_history(&db, actress, MIN_HISTORICAL_MOVIES - 1, 100).await;
    seed_candidate(&db, actress, 50, false).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert_eq!(body["items"], json!([]));
}

/// 黑名单影片不入结果。
///
/// 本仓的过滤在**候选 SQL** 里（`is_blacklisted = false`）；服务层装配时还有
/// 一道 `card.movie.is_blacklisted` 的兜底，接的是「打分与取卡片之间被拉黑」
/// 的并发窗口 —— 所以这个用例验的是**可观察行为**，两条路都会得到同一个答案。
#[tokio::test]
async fn a_blacklisted_candidate_movie_is_excluded() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("blacklist");
    let token = seed_token(db.pool()).await;

    let actress = seed_actor(&db, "上原", FEMALE_GENDER).await;
    seed_history(&db, actress, MIN_HISTORICAL_MOVIES, 100).await;
    seed_candidate(&db, actress, 50, true).await;

    let (status, body) = send(app(&db, &fixture), get("/hot-actress-releases", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert_eq!(body["items"], json!([]));
}

/// 排序：score 降序（score 是「历史证据均值」，heat 高处明显更高）。
#[tokio::test]
async fn candidates_are_ranked_by_score_desc() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("order");
    let token = seed_token(db.pool()).await;

    let cold = seed_actor(&db, "冷门", FEMALE_GENDER).await;
    let hot = seed_actor(&db, "热门", FEMALE_GENDER).await;
    seed_history(&db, cold, MIN_HISTORICAL_MOVIES, 10).await;
    seed_history(&db, hot, MIN_HISTORICAL_MOVIES, 3_000).await;
    let cold_movie = seed_candidate(&db, cold, 10, false).await;
    let hot_movie = seed_candidate(&db, hot, 10, false).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/hot-actress-releases?page=1&page_size=1", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2, "两条都在候选里");
    assert_eq!(body["items"].as_array().expect("数组").len(), 1);
    assert_eq!(body["items"][0]["id"], hot_movie, "第一页第一条是高分那条");
    assert_eq!(body["items"][0]["hot_actress"]["name"], "热门");

    let (status, body) = send(
        app(&db, &fixture),
        get("/hot-actress-releases?page=2&page_size=1", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["id"], cold_movie);
}

/// 越界分页 → 422 **专用码**（服务层 `validate_page`）。
#[tokio::test]
async fn an_out_of_range_page_size_returns_the_dedicated_filter_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("range");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/hot-actress-releases?page_size=101", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body["error"]["code"], "invalid_hot_actress_release_filter",
        "不是通用的 validation_error"
    );
}
