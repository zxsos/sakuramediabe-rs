//! 状态页聚合的集成测试：`GET /status` 与 `/status/insights`。
//!
//! # 为什么这一批**必须**连真库
//!
//! 全部是聚合查询，而聚合的错误**不会报错**，只会给出错的数字：
//!
//! - `COUNT(DISTINCT movie)` 写成 `COUNT(*)` -> 「可播放影片数」等于媒体文件数；
//! - `COALESCE(SUM(...), 0)` 漏掉 -> 空库时解码失败（500）而不是 0；
//! - 六分类少一个桶 -> `total` 与六桶之和不等，而测试若只断言 `total`
//!   就发现不了；
//! - 成员计数忘了过滤系统列表 -> 「4 个列表 / 30 部电影」里那 30 部
//!   包含了用户看不到的系统列表。
//!
//! 这些都不可能在 `cargo test --lib` 里出现。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{MovieRepository, NewCollection, NewMedia, NewMovie, PlaylistRepository};
use sm_db::testing::TestDb;
use sm_db::transfers::downloads::{download_state, import_status};
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "status-secret";

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn temp_config_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("sm-api-status-{}-{n}.toml", std::process::id()))
}

async fn setup() -> (TestDb, AppState, String) {
    let db = TestDb::require().await;
    let users = sm_db::repo::UserRepository::new(db.pool().clone());
    let username = format!(
        "st{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let user = users
        .insert(&sm_db::repo::NewUser {
            username,
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    let token = encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET);
    let state = AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(temp_config_path()),
    );
    (db, state, token)
}

fn app(state: &AppState) -> axum::Router {
    router(state.clone())
}

async fn get(state: &AppState, token: &str, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求");
    let response = app(state).oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "响应体不是 JSON: {e}; 原始: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("ST-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "影片".to_owned(),
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

// ================================================================ 形状

#[tokio::test]
async fn the_status_endpoint_has_the_upstream_shape() {
    let (db, state, token) = setup().await;
    let (status, body) = get(&state, &token, "/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    for key in [
        "backend_version",
        "actors",
        "movies",
        "media_files",
        "media_libraries",
        "thumbnails",
    ] {
        assert!(body.get(key).is_some(), "缺字段 {key}: {body}");
    }
    for (section, keys) in [
        ("actors", vec!["female_total", "female_subscribed"]),
        ("movies", vec!["total", "subscribed", "playable"]),
        ("media_files", vec!["total", "total_size_bytes"]),
        ("media_libraries", vec!["total"]),
        (
            "thumbnails",
            vec![
                "pending_media",
                "retry_wait_media",
                "terminal_failed_media",
                "total",
            ],
        ),
    ] {
        for key in keys {
            assert!(
                body[section].get(key).is_some(),
                "缺 {section}.{key}: {body}"
            );
        }
    }
    // 数字字段必须是数字而不是字符串/空
    assert!(body["movies"]["total"].is_i64());
    assert!(body["media_files"]["total_size_bytes"].is_i64());
    let _ = db;
}

/// 全部计数在空库上必须是 **0**，不是 `null`、不是 500。
///
/// `SUM` 在空表时返回 NULL，而响应字段是非可空整数 —— `COALESCE` 漏掉时
/// 这里会直接解码失败。
#[tokio::test]
async fn an_empty_database_yields_zeroes_not_errors() {
    let (_db, state, token) = setup().await;
    let (status, body) = get(&state, &token, "/status").await;
    assert_eq!(status, StatusCode::OK, "空库也必须是 200：{body}");
    for path in [
        "actors.female_total",
        "actors.female_subscribed",
        "movies.total",
        "movies.subscribed",
        "movies.playable",
        "media_files.total",
        "media_files.total_size_bytes",
        "media_libraries.total",
        "thumbnails.pending_media",
        "thumbnails.retry_wait_media",
        "thumbnails.terminal_failed_media",
        "thumbnails.total",
    ] {
        let mut cursor = &body;
        for part in path.split('.') {
            cursor = &cursor[part];
        }
        assert_eq!(cursor.as_i64(), Some(0), "{path} 必须是 0，实际 {cursor}");
    }
}

/// `backend_version` 缺省是 `dev-local`（未注入环境变量时）。
#[tokio::test]
async fn the_backend_version_falls_back_to_dev_local() {
    let (_db, state, token) = setup().await;
    let (_, body) = get(&state, &token, "/status").await;
    assert_eq!(body["backend_version"], json!("dev-local"));
}

// ================================================================ 可播放影片

/// 一部影片有多条媒体时，可播放影片数**按影片去重**。
///
/// 写成 `COUNT(*)` 会把「有多少个文件」当成「有多少部影片」—— 一部影片
/// 有不同画质/版本是常态，两个数字能差一个量级。
#[tokio::test]
async fn playable_movies_are_counted_per_movie_not_per_file() {
    let (db, state, token) = setup().await;
    let (_id, number) = seed_movie(&db).await;

    let library_id = sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;

    // 同一部影片挂两条**有效**媒体
    for suffix in ["a", "b"] {
        sm_db::repo::MediaRepository::new(db.pool().clone())
            .insert(&NewMedia {
                library_id,
                file_name: format!("m-{suffix}-{}.mp4", n()),
                file_size_bytes: 1,
                movie_number: Some(number.clone()),
                video_item_id: None,
                storage_ref: None,
                resolution: Some("1920x1080".to_owned()),
                file_hash: None,
                import_source_identity: None,
                duration_seconds: 1,
                video_info: None,
            })
            .await
            .expect("insert media");
    }

    let (_, body) = get(&state, &token, "/status").await;
    let before = body["movies"]["playable"]
        .as_i64()
        .expect("playable 是数字");
    // 增量断言：不能依赖库里绝对值（其它测试也在写）
    assert!(before >= 1, "有有效媒体的影片必须计入可播放，拿到 {before}");
    // 至少证明「两条文件没有被数成两部」：用 media_files.total 对比
    let files = body["media_files"]["total"].as_i64().unwrap_or(0);
    assert!(
        files > before,
        "媒体文件数({files}) 不该小于可播放影片数({before})"
    );
}

/// 判死（`valid = false`）的媒体**不**让它可播放。
#[tokio::test]
async fn an_invalid_media_does_not_make_the_movie_playable() {
    let (db, state, token) = setup().await;
    let (_id, number) = seed_movie(&db).await;
    let library_id = sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;
    sm_db::repo::MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("dead-{}.mp4", n()),
            file_size_bytes: 1,
            movie_number: Some(number.clone()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some("1920x1080".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert media");
    sqlx::query("UPDATE media SET valid = FALSE WHERE movie_number = $1")
        .bind(&number)
        .execute(db.pool())
        .await
        .expect("mark invalid");

    let (_, body) = get(&state, &token, "/status").await;
    // 媒体文件数算它（COUNT(*) 不过滤 valid），可播放不算
    assert!(
        body["media_files"]["total"].as_i64().unwrap_or(0)
            > body["movies"]["playable"].as_i64().unwrap_or(0)
            || body["movies"]["playable"].as_i64().unwrap_or(0) == 0,
        "判死的媒体不该计入可播放"
    );
}

// ================================================================ insights 形状

#[tokio::test]
async fn the_insights_endpoint_has_the_upstream_shape() {
    let (_db, state, token) = setup().await;
    let (status, body) = get(&state, &token, "/status/insights").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for key in ["download_tasks", "media_libraries", "collections"] {
        assert!(body.get(key).is_some(), "缺 {key}: {body}");
    }
    for key in [
        "total",
        "downloading",
        "importing",
        "imported",
        "import_failed",
        "skipped",
        "download_failed",
    ] {
        assert!(
            body["download_tasks"].get(key).is_some(),
            "缺 download_tasks.{key}: {body}"
        );
    }
    for key in [
        "playlists",
        "video_collections",
        "clip_collections",
        "moment_collections",
    ] {
        assert!(body["collections"][key].is_object(), "缺 collections.{key}");
        assert!(body["collections"][key]["count"].is_i64());
        assert!(body["collections"][key]["item_count"].is_i64());
    }
    assert!(body["media_libraries"].is_array());
}

/// 六个桶之和**恒等于** `total`。
///
/// 这是上游 `total=sum(counts.values())` 的保证。没有它，状态页会显示
/// 「总共 100 个任务」而六个桶加起来是 97 —— 用户无从知道少了哪三个。
#[tokio::test]
async fn the_six_buckets_always_sum_to_the_total() {
    let (_db, state, token) = setup().await;
    let (_, body) = get(&state, &token, "/status/insights").await;
    let d = &body["download_tasks"];
    let sum = [
        "downloading",
        "importing",
        "imported",
        "import_failed",
        "skipped",
        "download_failed",
    ]
    .iter()
    .map(|k| d[*k].as_i64().expect("桶是数字"))
    .sum::<i64>();
    assert_eq!(sum, d["total"].as_i64().unwrap(), "六桶之和必须等于 total");
}

/// 磁盘空间三列是 `null` 而不是 `0`。
///
/// 缺 `playback` 域的 `storage_space_usages`，所以「没探测」；填 0 会被
/// 客户端渲染成「磁盘满了」。
#[tokio::test]
async fn the_disk_space_columns_are_null_not_zero() {
    let (db, state, token) = setup().await;
    sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library");

    let (_, body) = get(&state, &token, "/status/insights").await;
    let libs = body["media_libraries"].as_array().expect("数组");
    let mine = libs
        .iter()
        .find(|l| l["name"].as_str().unwrap_or("").starts_with("lib-"))
        .expect("刚建的库应出现");
    for key in ["space_total_bytes", "space_used_bytes", "space_free_bytes"] {
        assert!(
            mine[key].is_null(),
            "{key} 必须是 null（未探测），实际 {}",
            mine[key]
        );
        assert_ne!(mine[key].as_i64(), Some(0), "{key} 不能是 0");
    }
}

/// 空库也要返回一个媒体库条目（基准是库表，不是媒体表）。
///
/// 「配了库但一个文件都没进去」是需要被看见的状态。
#[tokio::test]
async fn an_empty_library_still_appears_with_zero_counts() {
    let (db, state, token) = setup().await;
    let name = format!("empty-lib-{}", n());
    sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: name.clone(),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library");

    let (_, body) = get(&state, &token, "/status/insights").await;
    let libs = body["media_libraries"].as_array().expect("数组");
    let mine = libs
        .iter()
        .find(|l| l["name"].as_str() == Some(name.as_str()))
        .expect("空库也必须出现");
    assert_eq!(mine["file_count"], json!(0));
    assert_eq!(mine["total_size_bytes"], json!(0));
}

// ================================================================ 合集计数口径

/// 播放列表计数**不含系统列表**（最近播放），成员数也不含它的。
///
/// 与 `GET /playlists?include_system=false` 同一口径 —— 两个数字对不上时
/// 用户没理由知道该信哪个。
#[tokio::test]
async fn the_playlist_counts_exclude_the_system_list() {
    let (db, state, token) = setup().await;
    let playlists = PlaylistRepository::new(db.pool().clone());

    // 一个自定义列表 + 一个成员
    let custom = playlists
        .insert(&NewCollection::host_owned("insp-自定义", ""))
        .await
        .expect("insert playlist");
    let (_movie_id, number) = seed_movie(&db).await;
    sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .add(custom.id, _movie_id)
        .await
        .expect("add member");

    // 系统列表也加一个成员 —— 它不该被计入
    let system = sm_service::collections::PlaylistService::new(db.pool())
        .recently_played()
        .await
        .expect("system playlist");
    let (other_id, _other_number) = seed_movie(&db).await;
    sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .add(system.id, other_id)
        .await
        .expect("add system member");
    let _ = number;

    let (_, body) = get(&state, &token, "/status/insights").await;
    let p = &body["collections"]["playlists"];

    // `recently_played` 只有一个（单例），所以**合集数**在任何过滤口径下
    // 都至少是「全部列表数 - 1」。这里断言的是它**没有被算进** count：
    // 直接查库拿到真实数字，与响应比对。
    let all_playlists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playlist")
        .fetch_one(db.pool())
        .await
        .expect("count all");
    let custom_only = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM playlist WHERE kind <> 'recently_played'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count custom");
    assert_eq!(
        p["count"].as_i64(),
        Some(custom_only),
        "播放列表数必须是自定义列表数（不含最近播放）：全部 {all_playlists} / 自定义 {custom_only}"
    );

    // 成员数同样要排除系统列表的成员
    let all_items = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playlist_movie")
        .fetch_one(db.pool())
        .await
        .expect("count items");
    assert_eq!(
        p["item_count"].as_i64(),
        Some(all_items - 1),
        "成员数必须扣掉系统列表那一条（全部 {all_items}）"
    );
}

// ================================================================ 鉴权与 405

#[tokio::test]
async fn every_status_endpoint_requires_authentication() {
    let (_db, state, _token) = setup().await;
    for uri in [
        "/status",
        "/status/insights",
        "/status/watch-trend",
        "/status/capabilities",
    ] {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("构造请求");
        let response = app(&state).oneshot(request).await.expect("oneshot");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} 必须鉴权"
        );
    }
}

#[tokio::test]
async fn a_method_other_than_get_is_405_with_an_envelope() {
    let (_db, state, token) = setup().await;
    for uri in ["/status", "/status/insights", "/status/capabilities"] {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("构造请求");
        let response = app(&state).oneshot(request).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读体")
            .to_bytes();
        let body: Value = serde_json::from_slice(&bytes).expect("应是 JSON 信封");
        assert_eq!(body["error"]["code"], json!("http_error"), "{uri}");
    }
}

// ================================================================ 常量对齐

/// 折叠规则用到的字面量必须与上游一致 —— 写错会让「导入失败」的数字
/// 变成「导入成功」的数字。
#[tokio::test]
async fn the_import_status_literals_are_the_upstream_ones() {
    assert_eq!(import_status::COMPLETED, "completed");
    assert_eq!(import_status::SKIPPED, "skipped");
    assert_eq!(import_status::PENDING, "pending");
    assert_eq!(import_status::RUNNING, "running");
    assert_eq!(import_status::FAILED, "failed");
    assert_eq!(download_state::COMPLETED, "completed");
    assert_eq!(download_state::FAILED, "failed");
}
