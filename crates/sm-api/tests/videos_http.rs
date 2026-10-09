//! `/videos*` 与 `/video-collections/{id}/items` 的 HTTP 契约测试（真实 PostgreSQL）。
//!
//! # 这一批盯的是什么
//!
//! 播放地址那条链路此前是**未接线**的（`play_url` 恒 `None` / 骨架 `todo!()`），
//! 而它有两个**看起来一样、语义相反**的空值：
//!
//! | 端点 | 字段 | 无值时 | 理由 |
//! |---|---|---|---|
//! | 合集成员 | `play_url` | **`null`** | 可空字段；空串会被客户端当成「有媒体但播不了」 |
//! | 视频详情 | `media_items[].play_url` | **`""`** | 非空 `str`；失效媒体给空串是上游刻意的 |
//!
//! 把这两条写成断言，是为了防止后来者「统一」它们。
//!
//! # 为什么带一个假注册表
//!
//! `playback_deliveries` 来自插件注册表。没有它，详情端点的每一条媒体都查不到
//! provider —— 而上游在那种情况下**漏了 `try/except`，直接 500**（不是 503、也
//! 不是「没有 `play_url`」）。这一条也在这里钉住。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    ImageRepository, MediaLibraryRepository, MediaPointRepository, MediaRepository, NewImage,
    NewMedia, NewMediaLibrary, NewUser, NewVideoItem, UserRepository, VideoItemRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::playback::media_library::{
    LibraryConfigField, LibraryForFuture, MediaLibraryCapability, MediaLibraryRegistry,
    PrepareLibraryFuture, PreparedLibrary, PreviousLibraryHandle, ProviderCatalogEntry,
    SpaceUsageFuture,
};
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "videos-http-secret";
const PROVIDER: &str = "fakelib";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

// ---------------------------------------------------------------- 假注册表

struct FakeCapability;

impl MediaLibraryCapability for FakeCapability {
    fn library_config_fields(&self) -> Vec<LibraryConfigField> {
        Vec::new()
    }

    fn prepare_library(
        &self,
        _submitted: &Value,
        _previous: Option<&PreviousLibraryHandle>,
    ) -> PrepareLibraryFuture<'_> {
        Box::pin(async {
            Ok(PreparedLibrary {
                provider_config: Value::Object(Default::default()),
                account_key: None,
            })
        })
    }
}

/// 只实现这两个端点用到的部分：`list_bundles`（拿 `playback_deliveries`）。
struct FakeRegistry;

impl MediaLibraryRegistry for FakeRegistry {
    fn library_for(&self, _provider_key: &str) -> LibraryForFuture<'_> {
        Box::pin(async {
            Ok(Some(
                Box::new(FakeCapability) as Box<dyn MediaLibraryCapability>
            ))
        })
    }

    fn supports_in_place_import(&self, _provider_key: &str) -> bool {
        false
    }

    fn list_bundles(&self) -> Vec<ProviderCatalogEntry> {
        vec![ProviderCatalogEntry {
            provider_key: PROVIDER.to_owned(),
            display_name: "假库".to_owned(),
            library_config_fields: Vec::new(),
            playback_deliveries: vec!["proxy".to_owned()],
            download_config_fields: None,
        }]
    }

    fn space_usage(
        &self,
        _library_id: i32,
        _provider_key: &str,
        _provider_config: &Value,
    ) -> SpaceUsageFuture<'_> {
        Box::pin(async { None })
    }
}

// ---------------------------------------------------------------- 夹具

fn config() -> ConfigService {
    let path = std::env::temp_dir().join(format!(
        "sm-videos-http-{}-{}.toml",
        std::process::id(),
        unique()
    ));
    std::fs::write(
        &path,
        format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
    )
    .expect("写测试配置");
    ConfigService::new(path)
}

async fn seed_token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("vi{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

/// `registry`：是否注入媒体库能力缝。`None` 用来验「没装插件」那条路径。
fn app(db: &TestDb, registry: bool) -> axum::Router {
    let state = AppState::new(db.pool().clone(), AuthConfig::new(SECRET), config());
    let state = if registry {
        state.with_media_library_registry(Arc::new(FakeRegistry))
    } else {
        state
    };
    router(state)
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn authed(method: &str, uri: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let payload = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(payload).expect("构造请求")
}

// ---------------------------------------------------------------- 种子

async fn seed_library(db: &Db) -> i32 {
    MediaLibraryRepository::new(db.clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", unique()),
            provider_key: PROVIDER.to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("插入媒体库")
        .id
}

async fn seed_video_item(db: &Db, title: &str) -> i32 {
    VideoItemRepository::new(db.clone())
        .insert(&NewVideoItem {
            title: title.to_owned(),
            summary: String::new(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .expect("插入条目")
        .id
}

/// 插一条媒体，返回 id。`valid = false` 走裸 SQL（仓储不提供「判死」这个业务动作）。
async fn seed_media(
    db: &Db,
    library_id: i32,
    video_id: i32,
    resolution: Option<&str>,
    valid: bool,
) -> i32 {
    let media = MediaRepository::new(db.clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("{}.mp4", unique()),
            file_size_bytes: 4096,
            movie_number: None,
            video_item_id: Some(video_id),
            storage_ref: None,
            resolution: resolution.map(str::to_owned),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 90,
            video_info: None,
        })
        .await
        .expect("插入媒体");
    if !valid {
        sqlx::query("UPDATE media SET valid = FALSE WHERE id = $1")
            .bind(media.id)
            .execute(db)
            .await
            .expect("判死媒体");
    }
    media.id
}

/// 给一条媒体挂一个时刻点（连图片），返回图片的相对路径。
async fn seed_point(db: &Db, media_id: i32) -> String {
    let origin = format!("thumbs/{}.webp", unique());
    let (image_id, _) = ImageRepository::new(db.clone())
        .upsert(&NewImage {
            origin: origin.clone(),
        })
        .await
        .expect("登记图片");
    MediaPointRepository::new(db.clone())
        .insert(Some(media_id), None, image_id, None, None, 12)
        .await
        .expect("插入时刻点");
    origin
}

// ---------------------------------------------------------------- 详情

/// `POST /videos` → **201** + 详情形状（`media_items` 是空数组，不是缺失）。
#[tokio::test]
async fn create_video_returns_201_and_the_detail_shape() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, true),
        authed(
            "POST",
            "/videos",
            &token,
            Some(json!({"title": "  素颜  ", "summary": "简介"})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    // 上游 `create_video` 结尾是 `get_video_detail(...)` —— 标题已被 strip()。
    assert_eq!(body["title"], "素颜");
    assert_eq!(body["summary"], "简介");
    assert_eq!(body["media_items"], json!([]));
    assert_eq!(body["can_play"], false);
    assert_eq!(body["media_count"], 0);
    assert!(body["id"].is_i64(), "{body}");
}

/// 详情的 `media_items`：全部媒体（含失效）+ 逐个签名，且 `play_url` **非空 `str`**。
#[tokio::test]
async fn detail_lists_all_media_and_signs_only_valid_ones() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let library = seed_library(db.pool()).await;
    let video = seed_video_item(db.pool(), "条目").await;
    let valid = seed_media(db.pool(), library, video, Some("1280x720"), true).await;
    let invalid = seed_media(db.pool(), library, video, None, false).await;
    let thumb = seed_point(db.pool(), valid).await;

    let (status, body) = send(
        app(&db, true),
        authed("GET", &format!("/videos/{video}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["media_items"].as_array().expect("media_items 是数组");
    assert_eq!(items.len(), 2, "失效媒体也要出现：{body}");

    // 按 `Media.id` 升序。
    let first = &items[0];
    assert_eq!(first["media_id"], valid);
    assert_eq!(first["valid"], true);
    assert!(
        first["play_url"]
            .as_str()
            .expect("play_url 是非空 str")
            .starts_with(&format!("/media/{valid}/play/")),
        "{first}"
    );
    assert!(
        first["play_url"]
            .as_str()
            .unwrap()
            .contains("delivery=proxy"),
        "交付方式取 `playback_deliveries[0]`：{first}"
    );
    assert_eq!(first["playback_deliveries"], json!(["proxy"]));
    assert_eq!(first["progress"], Value::Null, "没看过就是 null");
    assert_eq!(first["points"][0]["offset_seconds"], 12);
    assert!(
        first["points"][0]["image"]["origin"]
            .as_str()
            .unwrap()
            .starts_with("/files/images/"),
        "时刻点图片要签名：{first}"
    );
    assert!(thumb.starts_with("thumbs/"));

    // ★ 失效媒体：`play_url` 是**空串**，不是 `null`（字段本身非空）。
    let second = &items[1];
    assert_eq!(second["media_id"], invalid);
    assert_eq!(second["valid"], false);
    assert_eq!(second["play_url"], "");

    // 列表项那半边：派生字段取**第一条有效媒体**。
    assert_eq!(body["can_play"], true);
    assert_eq!(body["media_count"], 2);
    assert_eq!(body["duration_seconds"], 90);
    assert_eq!(body["file_size_bytes"], 4096);
    assert_eq!(body["cover_width"], 1280);
    assert_eq!(body["cover_height"], 720);
}

/// 没注入注册表 → 详情的 provider 查找失败 → **500**（上游漏接该异常）。
#[tokio::test]
async fn detail_is_a_500_when_no_provider_registry_is_injected() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let library = seed_library(db.pool()).await;
    let video = seed_video_item(db.pool(), "条目").await;
    seed_media(db.pool(), library, video, Some("1280x720"), true).await;

    let (status, body) = send(
        app(&db, false),
        authed("GET", &format!("/videos/{video}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "internal_error", "{body}");
}

/// 没有媒体的条目：详情的 provider 查找一次都不发生 → 不需要注册表也 200。
#[tokio::test]
async fn a_video_without_media_needs_no_registry() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let video = seed_video_item(db.pool(), "空条目").await;

    let (status, body) = send(
        app(&db, false),
        authed("GET", &format!("/videos/{video}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_items"], json!([]));
}

#[tokio::test]
async fn an_unknown_video_is_a_404() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, true),
        authed("GET", "/videos/2147483000", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "video_item_not_found", "{body}");
    assert_eq!(body["error"]["details"]["video_item_id"], 2147483000i64);
}

/// 空更新 → 422；改了标题 → 200 + 详情里跟着变。
#[tokio::test]
async fn update_rejects_an_empty_payload_and_returns_the_detail() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let video = seed_video_item(db.pool(), "旧标题").await;

    let (status, body) = send(
        app(&db, true),
        authed(
            "PATCH",
            &format!("/videos/{video}"),
            &token,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "validation_error", "{body}");

    let (status, body) = send(
        app(&db, true),
        authed(
            "PATCH",
            &format!("/videos/{video}"),
            &token,
            Some(json!({"title": "新标题"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "新标题");
    assert_eq!(body["media_items"], json!([]));
}

#[tokio::test]
async fn delete_is_204_and_then_the_video_is_gone() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let video = seed_video_item(db.pool(), "要删的").await;

    let (status, body) = send(
        app(&db, true),
        authed("DELETE", &format!("/videos/{video}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(body, Value::Null);

    let (status, _) = send(
        app(&db, true),
        authed("GET", &format!("/videos/{video}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- 合集成员

/// ★ 合集成员的 `play_url` 是**可空**：`include_play_url=false` → `null`（键仍在）。
///
/// 这一条与详情那条「失效媒体给空串」是**反的**，别统一。
#[tokio::test]
async fn collection_item_play_url_is_null_unless_requested() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let library = seed_library(db.pool()).await;
    let video = seed_video_item(db.pool(), "条目").await;
    let media = seed_media(db.pool(), library, video, Some("1280x720"), true).await;

    let (_, collection) = send(
        app(&db, true),
        authed(
            "POST",
            "/video-collections",
            &token,
            Some(json!({"name": format!("c-{}", unique()), "description": ""})),
        ),
    )
    .await;
    let collection_id = collection["id"].as_i64().expect("合集 id");
    let (status, body) = send(
        app(&db, true),
        authed(
            "POST",
            &format!("/video-collections/{collection_id}/items"),
            &token,
            Some(json!({"video_item_id": video})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // 不请求 → `play_url` 是 `null`，但键必须在（上游不过滤 None）。
    let (status, body) = send(
        app(&db, true),
        authed(
            "GET",
            &format!("/video-collections/{collection_id}/items"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = &body["items"][0];
    assert!(item.get("play_url").is_some(), "键不能消失：{item}");
    assert_eq!(item["play_url"], Value::Null);
    assert_eq!(
        item["first_media_id"], media,
        "恒返回，与 include_play_url 无关"
    );

    // 请求 → 签名地址。
    let (status, body) = send(
        app(&db, true),
        authed(
            "GET",
            &format!("/video-collections/{collection_id}/items?include_play_url=true"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = &body["items"][0];
    assert!(
        item["play_url"]
            .as_str()
            .expect("有有效媒体就该签名")
            .starts_with(&format!("/media/{media}/play/")),
        "{item}"
    );
}

/// `reorder` 的响应**不带** `play_url`（上游那里没有 `include_play_url`）。
#[tokio::test]
async fn reorder_does_not_sign_play_urls() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let library = seed_library(db.pool()).await;
    let video = seed_video_item(db.pool(), "条目").await;
    seed_media(db.pool(), library, video, Some("1280x720"), true).await;

    let (_, collection) = send(
        app(&db, true),
        authed(
            "POST",
            "/video-collections",
            &token,
            Some(json!({"name": format!("c-{}", unique()), "description": ""})),
        ),
    )
    .await;
    let collection_id = collection["id"].as_i64().expect("合集 id");
    send(
        app(&db, true),
        authed(
            "POST",
            &format!("/video-collections/{collection_id}/items"),
            &token,
            Some(json!({"video_item_id": video})),
        ),
    )
    .await;
    let (_, listed) = send(
        app(&db, true),
        authed(
            "GET",
            &format!("/video-collections/{collection_id}/items"),
            &token,
            None,
        ),
    )
    .await;
    let item_id = listed["items"][0]["item_id"].as_i64().expect("成员行 id");

    let (status, body) = send(
        app(&db, true),
        authed(
            "POST",
            &format!("/video-collections/{collection_id}/items/reorder"),
            &token,
            Some(json!({"ordered_item_ids": [item_id]})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["play_url"], Value::Null, "{body}");
}
