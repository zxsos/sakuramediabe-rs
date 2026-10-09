//! 演员端点（`/actors*`）的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! service 层的规则已由 `sm-service/tests/actor_catalog.rs` 钉住（真库）。这一层
//! 测的是**契约翻译**是否正确：
//!
//! - DTO 的**字段集合与数量**（多一个少一个都是契约变更）；
//! - 查询参数到 `ActorListParams` 的映射（`gender`/`subscription_status` 枚举、
//!   `cups` 的逗号解析与 `ge` 校验），这些上游发生在 service **之前**；
//! - 错误信封的状态码与 code（`404 actor_not_found`、`422 invalid_actor_filter`、
//!   `422 validation_error`）。
//!
//! 列出用例都会建**带唯一名字**的演员并写库，因此不依赖表是空的 —— 集成测试
//! 并行跑、共享同一个库。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, NaiveDate, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    ActorRepository, MovieActorRepository, MovieRepository, MovieTagRepository, NewActor, NewMovie,
    NewUser, TagRepository, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "actors-http-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 只提供一个可写的临时配置（带签名密钥），避免碰到真实的 `config.toml`。
struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-actors-{tag}-{}", unique()));
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
    let users = UserRepository::new(db.clone());
    let user = users
        .insert(&NewUser {
            username: format!("act{}", unique()),
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

fn authed(method: &str, uri: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(value) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        builder
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
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

fn date(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("日期字面量")
}

/// 造一位演员，返回 `(id, name)`。名字唯一，便于在共享库里定位。
async fn seed_actor(db: &TestDb) -> (i32, String) {
    let name = format!("ZZA{}", unique());
    let actor = ActorRepository::new(db.pool().clone())
        .insert(&NewActor {
            javdb_id: format!("ACT-{}", unique()),
            name: name.clone(),
        })
        .await
        .expect("insert actor");
    (actor.id, name)
}

/// 造一部影片，返回 `(id, movie_number)`。
async fn seed_movie(db: &TestDb, release_date: Option<NaiveDate>) -> (i32, String) {
    let number = format!("ACT-{}", unique());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: release_date.map(|d| d.and_hms_opt(0, 0, 0).unwrap()),
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

const LIST_FIELDS: [&str; 16] = [
    "id",
    "javdb_id",
    "name",
    "alias_name",
    "display_name",
    "profile_image",
    "is_subscribed",
    "subscribed_at",
    "movie_count",
    "age",
    "birthday",
    "height_cm",
    "bust_cm",
    "waist_cm",
    "hips_cm",
    "cup",
];

const DETAIL_ONLY_FIELDS: [&str; 7] = [
    "gender",
    "birthplace",
    "blood_type",
    "display_name_override",
    "has_profile_image_override",
    "mutation_revision",
    "manual_fields",
];

// ================================================================ 详情

#[tokio::test]
async fn the_detail_endpoint_returns_the_upstream_field_set() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("detail");
    let token = seed_token(db.pool()).await;
    let (id, name) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{id}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let object = body.as_object().expect("响应是对象");
    for key in LIST_FIELDS.iter().chain(DETAIL_ONLY_FIELDS.iter()) {
        assert!(object.contains_key(*key), "缺字段 {key}: {body}");
    }
    assert_eq!(
        object.len(),
        23,
        "详情字段数必须是 23 —— 多一个少一个都是契约变更: {body}"
    );
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["name"], json!(name));
    assert_eq!(body["display_name"], json!(name), "无覆盖时展示名等于 name");
    assert!(body["profile_image"].is_null(), "没头像就是 null");
    assert_eq!(body["is_subscribed"], json!(false));
    assert_eq!(body["gender"], json!(0));
    assert_eq!(body["movie_count"], json!(0));
    assert_eq!(body["mutation_revision"], json!(0));
    assert_eq!(body["has_profile_image_override"], json!(false));
    assert!(body["manual_fields"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_actor_is_404_actor_not_found() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("missing");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors/2147483000", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "actor_not_found");
}

// ================================================================ 列表

#[tokio::test]
async fn the_list_endpoint_uses_the_actor_resource_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let token = seed_token(db.pool()).await;
    let (id, name) = seed_actor(&db).await;

    // page_size 放大：库里还有别的用例造的演员。上游不校验 page_size 上界。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?page_size=10000", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    let ours = items
        .iter()
        .find(|item| item["id"] == json!(id))
        .unwrap_or_else(|| panic!("列表里应有 id={id}（name={name}）"));
    let object = ours.as_object().expect("列表项是对象");
    assert_eq!(
        object.len(),
        16,
        "列表项字段数必须是 16（不含详情字段）: {ours}"
    );
    for key in LIST_FIELDS {
        assert!(object.contains_key(key), "缺字段 {key}: {ours}");
    }
    for key in DETAIL_ONLY_FIELDS {
        assert!(
            !object.contains_key(key),
            "列表项不该带详情字段 {key}: {ours}"
        );
    }
    assert_eq!(ours["name"], json!(name));
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["page_size"], json!(10000));
    assert!(body["total"].as_i64().unwrap() >= 1);
}

// ================================================================ 订阅

#[tokio::test]
async fn subscription_endpoints_toggle_and_return_204() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("subs");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, _) = send(
        app(&db, &fixture),
        authed("PUT", &format!("/actors/{id}/subscription"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{id}"), &token, None),
    )
    .await;
    assert_eq!(body["is_subscribed"], json!(true));
    assert!(
        body["subscribed_at"].is_string(),
        "订阅后应有时间戳: {body}"
    );

    let (status, _) = send(
        app(&db, &fixture),
        authed(
            "DELETE",
            &format!("/actors/{id}/subscription"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{id}"), &token, None),
    )
    .await;
    assert_eq!(body["is_subscribed"], json!(false));
    assert!(body["subscribed_at"].is_null(), "退订后清空时间戳");
}

// ================================================================ PATCH

#[tokio::test]
async fn patch_updates_fields_and_marks_them_manual() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("patch");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/actors/{id}"),
            &token,
            Some(json!({ "height_cm": 165, "cup": " c " })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["height_cm"], json!(165));
    assert_eq!(body["cup"], json!("C"), "罩杯 trim 后大写");
    let manual: Vec<&str> = body["manual_fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(manual, vec!["cup", "height_cm"], "人工改过的字段升序");
}

#[tokio::test]
async fn an_empty_patch_is_422_empty_actor_update() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("PATCH", &format!("/actors/{id}"), &token, Some(json!({}))),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "empty_actor_update");
}

#[tokio::test]
async fn an_explicit_null_gender_is_rejected() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("gender");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/actors/{id}"),
            &token,
            Some(json!({ "gender": null })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

// ================================================================ 关联读端点

#[tokio::test]
async fn movie_ids_tags_and_years_read_the_relations() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("rels");
    let token = seed_token(db.pool()).await;
    let (actor_id, _) = seed_actor(&db).await;
    let (movie_id, _) = seed_movie(&db, Some(date("2019-05-04"))).await;

    MovieActorRepository::new(db.pool().clone())
        .link(movie_id, actor_id)
        .await
        .expect("link movie_actor");
    let tag = TagRepository::new(db.pool().clone())
        .upsert_by_name(&format!("话题{}", unique()))
        .await
        .expect("upsert tag");
    MovieTagRepository::new(db.pool().clone())
        .link(movie_id, tag.id)
        .await
        .expect("link movie_tag");

    let (status, ids) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/actors/{actor_id}/movie-ids"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ids}");
    assert_eq!(ids, json!([movie_id]));

    let (status, tags) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{actor_id}/tags"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tags}");
    assert_eq!(tags, json!([{ "tag_id": tag.id, "name": tag.name }]));

    let (status, years) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{actor_id}/years"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{years}");
    assert_eq!(years, json!([{ "year": 2019, "movie_count": 1 }]));
}

// ================================================================ 筛选项

#[tokio::test]
async fn filter_options_returns_the_upstream_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("filter");
    let token = seed_token(db.pool()).await;
    let _ = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors/filter-options", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    for key in ["actor_count", "as_of_date", "age", "height_cm", "cups"] {
        assert!(body.get(key).is_some(), "缺字段 {key}: {body}");
    }
    assert_eq!(body.as_object().unwrap().len(), 5);
    for range in ["age", "height_cm"] {
        for key in ["min", "max", "populated_count"] {
            assert!(
                body[range].get(key).is_some(),
                "{range} 缺字段 {key}: {body}"
            );
        }
    }
    assert!(body["as_of_date"].as_str().unwrap().len() == 10);
}

// ================================================================ 查询参数校验

#[tokio::test]
async fn an_unknown_gender_enum_is_422_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("genderenum");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?gender=banana", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn a_bad_cups_filter_is_422_invalid_actor_filter() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("cups");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?cups=AB,12", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "invalid_actor_filter");
    assert_eq!(body["error"]["details"]["cups"], json!("AB,12"));
}

#[tokio::test]
async fn a_negative_age_min_is_422_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("age");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?age_min=-1", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

// ================================================================ 合并

#[tokio::test]
async fn merge_endpoint_folds_sources_into_the_target() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("merge");
    let token = seed_token(db.pool()).await;
    let (target, target_name) = seed_actor(&db).await;
    let (source, source_name) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/actors/{target}/merge"),
            &token,
            Some(json!({ "source_actor_ids": [source] })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["id"], json!(target), "返回保留记录");
    assert!(
        body["alias_name"]
            .as_str()
            .unwrap_or_default()
            .contains(&source_name),
        "来源名字应并进别名: {body}"
    );
    let _ = target_name;

    // 来源端点会「跳一跳」解析到保留记录。
    let (status, resolved) = send(
        app(&db, &fixture),
        authed("GET", &format!("/actors/{source}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved["id"], json!(target), "墓碑应解析到保留记录");
}

#[tokio::test]
async fn merging_into_self_is_422_invalid_actor_merge() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("mergeself");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/actors/{id}/merge"),
            &token,
            Some(json!({ "source_actor_ids": [id] })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "invalid_actor_merge");
    assert_eq!(body["error"]["details"]["reason"], json!("merge_self"));
}

#[tokio::test]
async fn an_empty_source_list_is_422_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("mergeempty");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/actors/{id}/merge"),
            &token,
            Some(json!({ "source_actor_ids": [] })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn a_non_positive_source_id_is_422_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("mergeneg");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/actors/{id}/merge"),
            &token,
            Some(json!({ "source_actor_ids": [0] })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn merging_a_missing_source_is_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("mergemissing");
    let token = seed_token(db.pool()).await;
    let (id, _) = seed_actor(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/actors/{id}/merge"),
            &token,
            Some(json!({ "source_actor_ids": [2147000000] })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "actor_not_found");
}

#[tokio::test]
async fn the_lax_bool_query_accepts_one() {
    // 上游 `has_playable_movies: bool = False` 走 pydantic 的 lax 布尔，
    // `?has_playable_movies=1` 必须能解析（serde 的 bool 只认 true/false）。
    let db = TestDb::require().await;
    let fixture = Fixture::new("laxbool");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?has_playable_movies=1", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");

    // 拼错的值必须 422，而不是被当成 false。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/actors?has_playable_movies=ture", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}
