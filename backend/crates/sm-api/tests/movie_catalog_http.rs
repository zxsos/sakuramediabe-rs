//! 影片目录端点的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! 覆盖 `POST /movies/subscriptions`、`POST /movies/unsubscriptions`
//! 与 `GET /movies/latest`。
//!
//! # 这层测什么（订阅/退订）
//!
//! 两条端点的形状看着简单，真正的规则都在「写什么」上：
//!
//! - **窄更新写了哪些列**：`is_subscribed` / `subscribed_at` / 九列检索状态。
//!   少写一列不会报错，只会让新订阅带着上一次的失败码进重试队列；
//! - **`subscribed_at` 只在需要时覆盖**：重复订阅不该把订阅时间推到现在；
//! - **`skipped` 里的番号是原始展示形态**，不是归一后的大写 key ——
//!   客户端靠它把结果标回用户勾选的那一行；
//! - **部分成功**：不存在 / 已拉黑 / 有媒体都不让整批失败。
//!
//! 每个用例的 `TestDb` 是独立 schema。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary, NewMovie,
    NewUser, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "movie-subs-secret";

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
        let base = std::env::temp_dir().join(format!("sm-msub-{tag}-{}", unique()));
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
            username: format!("ms{}", unique()),
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

fn authed(method: &str, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("序列化请求体")))
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

/// 造一部影片，返回 `(id, 番号)`。番号混大小写，用来验 `UPPER()` 点查。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("Msub{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "订阅测试影片".to_owned(),
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

async fn set_blacklisted(db: &TestDb, movie_id: i32, blacklisted: bool) {
    sqlx::query("UPDATE movie SET is_blacklisted = $2 WHERE id = $1")
        .bind(movie_id)
        .bind(blacklisted)
        .execute(db.pool())
        .await
        .expect("set blacklisted");
}

/// 给影片挂一条媒体（退订的 `has_media` 判定用它）。
async fn seed_media(db: &TestDb, number: &str) {
    let library_id = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("msub-lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("msub-{}.mp4", n()),
            file_size_bytes: 1,
            movie_number: Some(number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("insert media");
}

/// 读回订阅相关的四列。
async fn subscription_state(
    db: &TestDb,
    movie_id: i32,
) -> (bool, Option<chrono::NaiveDateTime>, String, i32) {
    sqlx::query_as::<_, (bool, Option<chrono::NaiveDateTime>, String, i32)>(
        "SELECT is_subscribed, subscribed_at, subscription_search_state, \
         subscription_search_retry_round FROM movie WHERE id = $1",
    )
    .bind(movie_id)
    .fetch_one(db.pool())
    .await
    .expect("读订阅状态")
}

// ================================================================ 用例

#[tokio::test]
async fn subscribing_a_batch_writes_the_state_and_resets_the_search_state() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("sub");
    let token = seed_token(db.pool()).await;
    let (first_id, first_number) = seed_movie(&db).await;
    let (second_id, second_number) = seed_movie(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [first_number, second_number] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["requested_count"], json!(2));
    assert_eq!(body["updated_count"], json!(2));
    assert_eq!(body["skipped_count"], json!(0));
    assert_eq!(body["skipped"], json!([]));

    for id in [first_id, second_id] {
        let (subscribed, subscribed_at, state, retry_round) = subscription_state(&db, id).await;
        assert!(subscribed, "应当已订阅");
        assert!(subscribed_at.is_some(), "订阅时间应当写入");
        // 九列检索状态被重置成「待抓取」—— 新订阅要进抓取队列。
        assert_eq!(state, "pending");
        assert_eq!(retry_round, 1, "retry_round 是加一而不是清零");
    }

    // 再订一次：订阅时间**不许**被推到现在（否则客户端的「最近订阅」排序
    // 会被一次重复点击打乱），检索状态也不许被打回起点。
    let (_, before, _, _) = subscription_state(&db, first_id).await;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [first_number] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let (_, after, state, retry_round) = subscription_state(&db, first_id).await;
    assert_eq!(after, before, "重复订阅不该覆盖订阅时间");
    assert_eq!(state, "pending");
    assert_eq!(
        retry_round, 1,
        "重复订阅不该重置检索状态（retry_round 不涨）"
    );
}

#[tokio::test]
async fn duplicates_collapse_to_one_write_but_requested_count_does_not() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("dedup");
    let token = seed_token(db.pool()).await;
    let (_, number) = seed_movie(&db).await;

    // 同一部影片的三种写法（大小写 / 空白）。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [number.clone(), number.to_lowercase(), format!("  {number}  ")] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    // `requested_count` 数的是**请求里给了几项**，去重只影响写入。
    assert_eq!(body["requested_count"], json!(3));
    assert_eq!(body["updated_count"], json!(1));
    assert_eq!(body["skipped_count"], json!(0));
}

#[tokio::test]
async fn a_missing_number_is_skipped_with_its_original_display_form() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("missing");
    let token = seed_token(db.pool()).await;

    let typed = "  does-not-exist-9  ";
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [typed] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["updated_count"], json!(0));
    assert_eq!(body["skipped_count"], json!(1));
    // 回显**用户输入的原样**（含空白与大小写），客户端据此标回那一行。
    assert_eq!(body["skipped"][0]["movie_number"], json!(typed));
    assert_eq!(body["skipped"][0]["reason"], json!("movie_not_found"));
}

#[tokio::test]
async fn a_blacklisted_movie_is_skipped_but_still_counted_as_updated() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("blacklist");
    let token = seed_token(db.pool()).await;
    let (movie_id, number) = seed_movie(&db).await;
    set_blacklisted(&db, movie_id, true).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [number.clone()] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["skipped"][0]["reason"], json!("blacklisted"));
    // **上游的双重计数**：这条被跳过了，却仍然计进 updated_count。
    // 照抄是为了不静默改客户端已经在渲染的数字 —— 要修应当连同客户端一起改。
    assert_eq!(body["updated_count"], json!(1));
    assert_eq!(body["skipped_count"], json!(1));

    // 而且它**真的没被写**。
    let (subscribed, ..) = subscription_state(&db, movie_id).await;
    assert!(!subscribed, "被拉黑的影片不该被订阅");
}

#[tokio::test]
async fn unsubscribing_a_movie_with_media_is_skipped_not_failed() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("unsub");
    let token = seed_token(db.pool()).await;
    let (with_media_id, with_media) = seed_movie(&db).await;
    let (clean_id, clean) = seed_movie(&db).await;
    seed_media(&db, &with_media).await;
    // 先都订上，再验退订。
    sqlx::query("UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW()")
        .execute(db.pool())
        .await
        .expect("预置订阅态");

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/unsubscriptions",
            &token,
            json!({ "movie_numbers": [with_media, clean] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["updated_count"], json!(1), "只有没媒体的那部真的退了");
    assert_eq!(body["skipped_count"], json!(1));
    assert_eq!(body["skipped"][0]["reason"], json!("has_media"));

    let (with_media_subscribed, ..) = subscription_state(&db, with_media_id).await;
    assert!(with_media_subscribed, "有媒体的不该被退订");
    let (clean_subscribed, clean_at, ..) = subscription_state(&db, clean_id).await;
    assert!(!clean_subscribed, "没媒体的应当退订");
    assert!(clean_at.is_none(), "退订要清掉订阅时间");
}

#[tokio::test]
async fn an_empty_or_blank_list_is_a_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("validation");
    let token = seed_token(db.pool()).await;

    // 空数组：上游 `Field(min_length=1)`。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": [] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));

    // 逐项 strip 后为空：上游的 field_validator。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/subscriptions",
            &token,
            json!({ "movie_numbers": ["  "] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}

// ================================================================ GET /movies/latest

/// 无请求体的 GET。
fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

/// 把某部影片的媒体入库时间往前挪一天。
async fn age_media(db: &TestDb, movie_number: &str) {
    sqlx::query(
        "UPDATE media SET created_at = created_at - INTERVAL '1 day' WHERE movie_number = $1",
    )
    .bind(movie_number)
    .execute(db.pool())
    .await
    .expect("改媒体入库时间");
}

#[tokio::test]
async fn latest_lists_only_movies_with_local_media_newest_first() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("latest");
    let token = seed_token(db.pool()).await;

    let (older_id, older_number) = seed_movie(&db).await;
    let (newer_id, newer_number) = seed_movie(&db).await;
    // 这部没有本地媒体 —— 它不该出现在「最新到货」里。
    let (_, no_media_number) = seed_movie(&db).await;

    seed_media(&db, &older_number).await;
    age_media(&db, &older_number).await;
    seed_media(&db, &newer_number).await;

    let (status, body) = send(app(&db, &fixture), get("/movies/latest", &token)).await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(
        items.len(),
        2,
        "没有本地媒体的影片不该出现（{no_media_number}）：{body}"
    );
    // 排序键是**媒体的入库时间**，不是影片记录的创建时间。
    assert_eq!(items[0]["id"], json!(newer_id));
    assert_eq!(items[1]["id"], json!(older_id));
    assert_eq!(body["total"], json!(2));
    // 卡片字段与播放列表那条路径共用同一份映射，这里只验关键几个。
    assert_eq!(items[0]["movie_number"], json!(newer_number));
    assert_eq!(items[0]["media_count"], json!(1));
    assert_eq!(items[0]["can_play"], json!(true));
    assert!(items[0]["cover_image"].is_null(), "没有封面就是 null");
}

#[tokio::test]
async fn latest_skips_blacklisted_movies_but_the_total_still_counts_them() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("latest-black");
    let token = seed_token(db.pool()).await;

    let (keep_id, keep_number) = seed_movie(&db).await;
    let (hidden_id, hidden_number) = seed_movie(&db).await;
    seed_media(&db, &keep_number).await;
    seed_media(&db, &hidden_number).await;
    set_blacklisted(&db, hidden_id, true).await;

    let (status, body) = send(app(&db, &fixture), get("/movies/latest", &token)).await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], json!(keep_id));
    // **上游的 total 不带黑名单过滤**（`Movie.select().join(Media).group_by(...).count()`），
    // 所以拉黑过一部有媒体的影片后，total 比实际能翻到的条数多：最后一页可能是空的。
    // 照抄是为了不静默改客户端在渲染的分页数字。
    assert_eq!(body["total"], json!(2));
}

#[tokio::test]
async fn latest_paginates_with_the_requested_page_size() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("latest-page");
    let token = seed_token(db.pool()).await;

    for _ in 0..2 {
        let (_, number) = seed_movie(&db).await;
        seed_media(&db, &number).await;
    }

    let (status, body) = send(
        app(&db, &fixture),
        get("/movies/latest?page=1&page_size=1", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"], json!(2));
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["page_size"], json!(1));

    let (_, body) = send(
        app(&db, &fixture),
        get("/movies/latest?page=2&page_size=1", &token),
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ================================================================ 合集标记

/// 读回 `is_collection` 与字段归属（JSONB 文本）。
async fn collection_state(db: &TestDb, movie_id: i32) -> (bool, String) {
    sqlx::query_as::<_, (bool, String)>(
        "SELECT is_collection, COALESCE(field_owners::text, '') FROM movie WHERE id = $1",
    )
    .bind(movie_id)
    .fetch_one(db.pool())
    .await
    .expect("读合集状态")
}

#[tokio::test]
async fn collection_status_matches_by_normalized_number() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("collstatus");
    let token = seed_token(db.pool()).await;
    let (_, number) = seed_movie(&db).await;

    // 用小写输入：`UPPER(movie_number)` 点查，响应回的是**库内规范番号**。
    let (status, body) = send(
        app(&db, &fixture),
        get(
            &format!("/movies/{}/collection-status", number.to_lowercase()),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["movie_number"], json!(number));
    assert_eq!(body["is_collection"], json!(false), "默认不是合集");

    // 找不到时是 404（不是空状态）。
    let (status, body) = send(
        app(&db, &fixture),
        get("/movies/NOPE-999999/collection-status", &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("movie_not_found"));
}

#[tokio::test]
async fn marking_the_collection_type_writes_through_the_ownership_gateway() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("collmark");
    let token = seed_token(db.pool()).await;
    let (movie_id, number) = seed_movie(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            "/movies/collection-type",
            &token,
            json!({ "movie_numbers": [number.clone()], "collection_type": "collection" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["requested_count"], json!(1));
    assert_eq!(body["updated_count"], json!(1));

    let (is_collection, owners) = collection_state(&db, movie_id).await;
    assert!(is_collection, "应当已标记为合集");
    // **关键**：人工标记必须同时打上 host:manual —— 否则下一次自动导入会覆盖回去。
    assert!(
        owners.contains("is_collection") && owners.contains("host:manual"),
        "字段归属没写上，自动规则会覆盖人工标记：{owners}"
    );

    // 标回单片。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            "/movies/collection-type",
            &token,
            json!({ "movie_numbers": [number], "collection_type": "single" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["updated_count"], json!(1));
    let (is_collection, _) = collection_state(&db, movie_id).await;
    assert!(!is_collection, "应当已标回单片");
}

#[tokio::test]
async fn marking_unknown_numbers_updates_fewer_than_requested() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("collmiss");
    let token = seed_token(db.pool()).await;
    let (_, number) = seed_movie(&db).await;

    // 这个端点**没有** skipped 字段：差额就是没命中的。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            "/movies/collection-type",
            &token,
            json!({ "movie_numbers": [number, "NOPE-999999".to_owned()], "collection_type": "collection" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["requested_count"], json!(2));
    assert_eq!(body["updated_count"], json!(1));
}

#[tokio::test]
async fn an_unknown_collection_type_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("collbad");
    let token = seed_token(db.pool()).await;
    let (_, number) = seed_movie(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            "/movies/collection-type",
            &token,
            json!({ "movie_numbers": [number], "collection_type": "trilogy" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}

// ================================================================ 黑名单

/// 读回 `is_blacklisted` 与字段归属（JSONB 文本）。
async fn blacklist_state(db: &TestDb, movie_id: i32) -> (bool, String) {
    sqlx::query_as::<_, (bool, String)>(
        "SELECT is_blacklisted, COALESCE(field_owners::text, '') FROM movie WHERE id = $1",
    )
    .bind(movie_id)
    .fetch_one(db.pool())
    .await
    .expect("读黑名单状态")
}

#[tokio::test]
async fn blacklisting_goes_through_the_gateway_and_unblacklisting_undoes_it() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("black");
    let token = seed_token(db.pool()).await;
    let (movie_id, number) = seed_movie(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PUT",
            "/movies/blacklist",
            &token,
            json!({ "movie_numbers": [number.clone()] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "响应: {body}");
    assert_eq!(body, Value::Null, "204 应当是空响应体");

    let (blacklisted, owners) = blacklist_state(&db, movie_id).await;
    assert!(blacklisted, "应当已拉黑");
    // 人工标记必须带上 host:manual，否则自动规则会把它覆盖回去。
    assert!(
        owners.contains("is_blacklisted") && owners.contains("host:manual"),
        "字段归属没写上：{owners}"
    );

    let (status, _) = send(
        app(&db, &fixture),
        authed(
            "DELETE",
            "/movies/blacklist",
            &token,
            json!({ "movie_numbers": [number] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (blacklisted, _) = blacklist_state(&db, movie_id).await;
    assert!(!blacklisted, "应当已解除");
}

#[tokio::test]
async fn blacklisting_a_subscribed_movie_is_a_409_listing_them() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("black-sub");
    let token = seed_token(db.pool()).await;
    let (movie_id, number) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW() WHERE id = $1")
        .bind(movie_id)
        .execute(db.pool())
        .await
        .expect("预置订阅态");

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PUT",
            "/movies/blacklist",
            &token,
            json!({ "movie_numbers": [number.clone()] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("movie_is_subscribed"));
    // details 是**已订阅的那些**（数组）。
    assert_eq!(body["error"]["details"]["movie_numbers"], json!([number]));

    let (blacklisted, _) = blacklist_state(&db, movie_id).await;
    assert!(!blacklisted, "校验没过就不该有写入");
}

#[tokio::test]
async fn blacklisting_a_missing_number_is_404_with_the_display_form() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("black-missing");
    let token = seed_token(db.pool()).await;
    let (_, number) = seed_movie(&db).await;
    let typed = "  nope-999999  ";

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PUT",
            "/movies/blacklist",
            &token,
            json!({ "movie_numbers": [number.clone(), typed] }),
        ),
    )
    .await;

    // 与订阅端点不同：这里**整批失败**，没有 skipped。
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("movie_not_found"));
    assert_eq!(
        body["error"]["details"]["movie_numbers"],
        json!([typed]),
        "回显用户输入的原样（含空白）"
    );
}

#[tokio::test]
async fn an_over_long_blacklist_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("black-long");
    let token = seed_token(db.pool()).await;

    // 上游这一条有 `max_length=1000`（订阅端点没有）。
    let numbers: Vec<String> = (0..1001).map(|i| format!("X-{i}")).collect();
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PUT",
            "/movies/blacklist",
            &token,
            json!({ "movie_numbers": numbers }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}

// ================================================================ GET /movies

#[tokio::test]
async fn the_movie_list_filters_and_returns_the_card_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let token = seed_token(db.pool()).await;

    let (subscribed_id, subscribed_number) = seed_movie(&db).await;
    let (_, plain_number) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW() WHERE id = $1")
        .bind(subscribed_id)
        .execute(db.pool())
        .await
        .expect("预置订阅态");

    let (status, body) = send(app(&db, &fixture), get("/movies", &token)).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let all = body["items"].as_array().expect("items 是数组");
    assert!(all.len() >= 2, "应当能列出影片：{body}");

    // 卡片字段与播放列表那条路径**共用同一份映射**，所以这里只验关键几个。
    let card = all
        .iter()
        .find(|item| item["movie_number"] == json!(subscribed_number))
        .expect("订阅的那部应当出现");
    assert_eq!(card["is_subscribed"], json!(true));
    assert_eq!(card["id"], json!(subscribed_id));
    assert!(card.get("media_items").is_some(), "应带媒体摘要");

    // 三态筛选：只要已订阅的。
    let (status, body) = send(app(&db, &fixture), get("/movies?status=subscribed", &token)).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert!(
        items
            .iter()
            .all(|item| item["is_subscribed"] == json!(true)),
        "status=subscribed 只该回已订阅的：{body}"
    );
    // `total` 是**筛选后**的总数，与当页同口径。
    assert_eq!(body["total"], json!(items.len() as i64));
    let _ = plain_number;
}

#[tokio::test]
async fn filter_values_that_do_not_parse_are_invalid_movie_filter() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("badfilter");
    let token = seed_token(db.pool()).await;

    // 每一对都是 (查询串, 期望回显在 details 里的那个参数名)。
    for (uri, field) in [
        ("/movies?tag_ids=1,,2", "tag_ids"),
        ("/movies?tag_ids=abc", "tag_ids"),
        ("/movies?tag_ids=0", "tag_ids"),
        ("/movies?director_name=%20", "director_name"),
        ("/movies?maker_name=%20", "maker_name"),
        ("/movies?sort=title:desc", "sort"),
        ("/movies?resolution=8k2", "resolution"),
    ] {
        let (status, body) = send(app(&db, &fixture), get(uri, &token)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri} 应当是 422，响应: {body}"
        );
        assert_eq!(
            body["error"]["code"],
            json!("invalid_movie_filter"),
            "{uri}"
        );
        assert!(
            body["error"]["details"].get(field).is_some(),
            "{uri} 的 details 应当回显 {field}：{body}"
        );
    }

    // `heat_min > heat_max` 是**两个**键一起回显。
    let (status, body) = send(
        app(&db, &fixture),
        get("/movies?heat_min=5&heat_max=1", &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_movie_filter"));
    assert_eq!(body["error"]["details"]["heat_min"], json!(5));
    assert_eq!(body["error"]["details"]["heat_max"], json!(1));
}

#[tokio::test]
async fn an_unknown_enum_value_is_a_plain_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("badenum");
    let token = seed_token(db.pool()).await;

    // 枚举类参数与上面那批**不是同一个错误码** —— 上游分别是 pydantic 与
    // `parse_csv_positive_ints` / `resolve_sort_expression` 给的。
    for uri in [
        "/movies?status=weird",
        "/movies?collection_type=weird",
        "/movies?number_source=weird",
        "/movies?tag_match=weird",
    ] {
        let (status, body) = send(app(&db, &fixture), get(uri, &token)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri} 应当是 422，响应: {body}"
        );
        assert_eq!(body["error"]["code"], json!("validation_error"), "{uri}");
    }
}

// ================================================================ POST /movies/by-series

#[tokio::test]
async fn by_series_lists_only_that_series_and_validates_paging() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("byseries");
    let token = seed_token(db.pool()).await;

    let series =
        sqlx::query_scalar::<_, i32>("INSERT INTO movie_series (name) VALUES ($1) RETURNING id")
            .bind(format!("系列-{}", unique()))
            .fetch_one(db.pool())
            .await
            .expect("insert series");

    let (in_series_id, in_series_number) = seed_movie(&db).await;
    let (_, other_number) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET series_id = $2 WHERE id = $1")
        .bind(in_series_id)
        .bind(series)
        .execute(db.pool())
        .await
        .expect("set series");

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/by-series",
            &token,
            json!({ "series_id": series }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "只该回这个系列里的影片：{body}");
    assert_eq!(items[0]["movie_number"], json!(in_series_number));
    assert_eq!(items[0]["series_id"], json!(series));
    assert_eq!(body["total"], json!(1));
    let _ = other_number;

    // **这个端点的分页是校验过的**（与 /movies、/latest 的裸 int 不同）。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/by-series",
            &token,
            json!({ "series_id": series, "page_size": 101 }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));

    // `series_id` 是必填（请求体缺键 → 422）。
    let (status, _) = send(
        app(&db, &fixture),
        authed("POST", "/movies/by-series", &token, json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ================================================================ 已订阅演员最新影片

/// 造一位演员，返回 id。
async fn seed_actor(db: &TestDb, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("INSERT INTO actor (javdb_id, name) VALUES ($1, $2) RETURNING id")
        .bind(format!("SUBACT-{}", unique()))
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("insert actor")
}

/// 把影片关联到演员。
async fn link_actor(db: &TestDb, movie_id: i32, actor_id: i32) {
    sqlx::query("INSERT INTO movie_actor (movie_id, actor_id) VALUES ($1, $2)")
        .bind(movie_id)
        .bind(actor_id)
        .execute(db.pool())
        .await
        .expect("link actor");
}

#[tokio::test]
async fn subscribed_actor_latest_excludes_unsubscribed_and_collections() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("subact");
    let token = seed_token(db.pool()).await;

    let actor = seed_actor(&db, &format!("订阅演员{}", n())).await;
    sqlx::query("UPDATE actor SET is_subscribed = TRUE WHERE id = $1")
        .bind(actor)
        .execute(db.pool())
        .await
        .expect("订阅演员");

    // 要出现的那部：关联已订阅演员。
    let (wanted_id, wanted_number) = seed_movie(&db).await;
    link_actor(&db, wanted_id, actor).await;

    // **合集番号要排除** —— 合辑不该出现在这个流里。
    let (collection_id, _) = seed_movie(&db).await;
    link_actor(&db, collection_id, actor).await;
    sqlx::query("UPDATE movie SET is_collection = TRUE WHERE id = $1")
        .bind(collection_id)
        .execute(db.pool())
        .await
        .expect("标合集");

    // 未订阅演员的影片不该出现。
    let other_actor = seed_actor(&db, &format!("未订阅演员{}", n())).await;
    let (other_id, _) = seed_movie(&db).await;
    link_actor(&db, other_id, other_actor).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/movies/subscribed-actors/latest", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "只该回已订阅演员的非合集影片：{body}");
    assert_eq!(items[0]["movie_number"], json!(wanted_number));
    assert_eq!(items[0]["id"], json!(wanted_id));
    // total 与当页同口径（DISTINCT 去重）。
    assert_eq!(body["total"], json!(1));
}

// ================================================================ 番号解析

#[tokio::test]
async fn parse_number_recognises_text_and_never_touches_the_database() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("parse");
    let token = seed_token(db.pool()).await;

    // 带杂讯的自由文本：识别出番号，`query` 回显 **strip 后**的输入。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/search/parse-number",
            &token,
            json!({ "query": "  SSNI-888 第 1 集  " }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["query"], json!("SSNI-888 第 1 集"));
    assert_eq!(body["parsed"], json!(true));
    assert_eq!(body["movie_number"], json!("SSNI-888"));
    assert_eq!(body["reason"], json!(null), "成功时 reason 是 null");

    // **识别不出不是错误** —— 这是这条端点最重要的性质：它不是「查影片」，
    // 所以既不会 404、也不会因为库里没有这部影片而失败。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/search/parse-number",
            &token,
            json!({ "query": "完全无关的标题" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["parsed"], json!(false));
    assert_eq!(body["movie_number"], json!(null));
    assert_eq!(body["reason"], json!("movie_number_not_found"));

    // 空白输入才是 422（上游 `min_length=1` + strip 非空校验）。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/movies/search/parse-number",
            &token,
            json!({ "query": "   " }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}
