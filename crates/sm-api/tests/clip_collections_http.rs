//! 片段合集端点的 HTTP 契约测试，**真实 PostgreSQL + 真实文件**。
//!
//! # 为什么必须同时有真库与真文件
//!
//! 合集读路径的核心语义是「只数产物有效的成员，并顺带回收失效的那些」。
//! 判定要看磁盘：文件在不在、字节数对不对得上。空目录替身会让**每个**成员
//! 都被判成无效，于是「返回 200」照样通过，而真实行为（计数偏小、数据被删）
//! 完全测不到。
//!
//! 所以每个用例都：**建产物文件 → 造库行 → 打 HTTP → 回读库与磁盘**。

use std::fs;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewMediaClip, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "clipcoll-http-secret";

/// 片段产物根目录 + 配置文件路径。
struct Fixture {
    root: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-clipcoll-{tag}-{}", unique()));
        let root = base.join("clips");
        fs::create_dir_all(&root).expect("建片段根目录");
        let root = root.canonicalize().expect("规范化片段根目录");
        let config_path = base.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[auth]\nfile_signature_secret = \"{SECRET}\"\n\n\
                 [media]\nmedia_clip_root_path = \"{}\"\n",
                root.display()
            ),
        )
        .expect("写测试配置");
        Self { root, config_path }
    }

    /// 在根目录下写一个产物文件，返回 (相对路径, 字节数)。
    fn write_artifact(&self, relative: &str, bytes: &[u8]) -> (String, i64) {
        let target = self.root.join(relative);
        fs::create_dir_all(target.parent().expect("有父目录")).expect("建父目录");
        fs::write(&target, bytes).expect("写产物");
        (
            relative.to_owned(),
            i64::try_from(bytes.len()).expect("长度转 i64"),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.config_path.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

async fn seed_token(db: &Db) -> String {
    let users = UserRepository::new(db.clone());
    let user = users
        .insert(&NewUser {
            username: format!("cc{}", unique()),
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

/// 造一个产物完好的片段，返回库里的 id。
async fn seed_clip(db: &TestDb, fixture: &Fixture, movie: &str) -> i32 {
    let (file_path, size) =
        fixture.write_artifact(&format!("{movie}/{}.mp4", unique()), b"clip-body");
    insert(db, movie, &file_path, size).await
}

/// 造一个产物**缺失**的片段：库里有行，磁盘上没有文件。
async fn seed_clip_without_artifact(db: &TestDb, movie: &str) -> i32 {
    insert(db, movie, &format!("{movie}/{}.mp4", unique()), 999).await
}

async fn insert(db: &TestDb, movie: &str, file_path: &str, size: i64) -> i32 {
    sm_db::repo::MediaClipRepository::new(db.pool().clone())
        .insert(&NewMediaClip {
            media_id: None,
            movie_number: Some(movie.to_owned()),
            start_offset_seconds: 0,
            end_offset_seconds: 30,
            title: String::new(),
            file_path: file_path.to_owned(),
            file_size_bytes: size,
            duration_seconds: 30,
        })
        .await
        .expect("插入片段")
        .id
}

async fn clip_count(db: &TestDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_clip")
        .fetch_one(db.pool())
        .await
        .expect("数片段")
}

/// 建一个合集，返回它的 id。
async fn new_collection(db: &TestDb, fixture: &Fixture, token: &str, name: &str) -> i32 {
    let (status, body) = send(
        app(db, fixture),
        authed(
            "POST",
            "/clip-collections",
            token,
            Some(json!({ "name": name })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "建合集失败: {body}");
    body["id"].as_i64().expect("响应应有 id") as i32
}

/// 把一个片段加入合集，断言 204。
async fn add_member(db: &TestDb, fixture: &Fixture, token: &str, collection_id: i32, clip_id: i32) {
    let (status, body) = send(
        app(db, fixture),
        authed(
            "PUT",
            &format!("/clip-collections/{collection_id}/clips/{clip_id}"),
            token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "加入 {clip_id}: {body}");
}

// ------------------------------------------------------------------ 列表

/// 列表字段集与上游 `ClipCollectionResource` 一致。
#[tokio::test]
async fn the_list_endpoint_returns_the_upstream_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    new_collection(&db, &fixture, &token, "合集甲").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/clip-collections", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body.as_array().expect("顶层是数组");
    let first = &items[0];
    for key in [
        "id",
        "name",
        "description",
        "clip_count",
        "cover_image",
        "created_at",
        "updated_at",
    ] {
        assert!(first.get(key).is_some(), "缺字段 {key}: {first}");
    }
    assert_eq!(
        first.as_object().expect("是对象").len(),
        7,
        "字段数必须是 7 —— 多一个少一个都是契约变更: {first}"
    );
    assert_eq!(first["clip_count"], 0, "新建的合集没有成员");
    assert!(first["cover_image"].is_null(), "没有成员就没有封面");
}

/// **`clip_count` 只数产物有效的成员，并顺带回收失效的。**
///
/// 这是本文件最重要的一条：合集列表的计数口径与片段列表**共用同一个判定**。
/// 三个成员、磁盘上只有两个产物时，计数是 2，且那一个失效的会被删掉。
#[tokio::test]
async fn the_count_includes_only_members_whose_artifact_survives() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("count");
    let token = seed_token(db.pool()).await;

    let good_a = seed_clip(&db, &fixture, "AAA-001").await;
    let good_b = seed_clip(&db, &fixture, "BBB-002").await;
    let doomed = seed_clip_without_artifact(&db, "CCC-003").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;

    for clip_id in [good_a, good_b, doomed] {
        add_member(&db, &fixture, &token, id, clip_id).await;
    }
    assert_eq!(clip_count(&db).await, 3, "前提：三个片段都在库里");

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/clip-collections", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = body
        .as_array()
        .expect("数组")
        .iter()
        .find(|r| r["id"] == id)
        .expect("合集应出现");
    assert_eq!(
        row["clip_count"], 2,
        "计数必须是 2（产物有效的），不是成员行数 3: {row}"
    );

    // 回收是写副作用：那个失效片段的行必须被删掉
    assert_eq!(
        clip_count(&db).await,
        2,
        "失效成员的行应被回收 —— 回收是读路径的副作用"
    );
    assert!(
        sm_db::repo::MediaClipRepository::new(db.pool().clone())
            .find_by_id(doomed)
            .await
            .expect("回读")
            .is_none(),
        "被回收的片段在库里不该还在"
    );
}

/// 成员列表的 `total` 同样只算有效的。
#[tokio::test]
async fn the_member_page_total_also_counts_only_valid_members() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("member-total");
    let token = seed_token(db.pool()).await;

    let good = seed_clip(&db, &fixture, "AAA-001").await;
    let doomed = seed_clip_without_artifact(&db, "BBB-002").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    add_member(&db, &fixture, &token, id, good).await;
    add_member(&db, &fixture, &token, id, doomed).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/clip-collections/{id}/clips"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], 1, "total 必须是过滤后的 1: {body}");
    assert_eq!(body["items"].as_array().expect("数组").len(), 1);
}

// ------------------------------------------------------------------ 增删改

#[tokio::test]
async fn creating_a_collection_returns_201_with_zero_count() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("create");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/clip-collections",
            &token,
            Some(json!({ "name": "  我的合集  ", "description": "  说明  " })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "响应: {body}");
    assert_eq!(body["name"], "我的合集", "名称两端空白要裁掉");
    assert_eq!(body["description"], "说明", "描述两端空白要裁掉");
    assert_eq!(body["clip_count"], 0);
    assert!(body["cover_image"].is_null());
}

/// 空白名称 422（上游是 `min_length=1` + validator 双重保证）。
#[tokio::test]
async fn a_blank_name_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("blank-name");
    let token = seed_token(db.pool()).await;
    let shared = app(&db, &fixture);

    for bad in ["", "   ", "\t\n"] {
        let (status, body) = send(
            shared.clone(),
            authed(
                "POST",
                "/clip-collections",
                &token,
                Some(json!({ "name": bad })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}: {body}");
        assert_eq!(code_of(&body), "validation_error", "{bad:?}");
    }
}

/// 名称唯一：重名 409，且 details 回显名字。
#[tokio::test]
async fn a_duplicate_name_is_409() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("dup");
    let token = seed_token(db.pool()).await;
    new_collection(&db, &fixture, &token, "同名").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/clip-collections",
            &token,
            Some(json!({ "name": "同名" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "响应: {body}");
    assert_eq!(code_of(&body), "clip_collection_name_conflict");
    assert_eq!(body["error"]["details"]["name"], "同名");
}

/// 改名成自己原来的名字不算冲突。
#[tokio::test]
async fn renaming_to_its_own_name_is_not_a_conflict() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("self-rename");
    let token = seed_token(db.pool()).await;
    let id = new_collection(&db, &fixture, &token, "原名").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/clip-collections/{id}"),
            &token,
            Some(json!({ "name": "原名" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改成自己的名字不该撞唯一性: {body}");
}

/// 空更新 422。
#[tokio::test]
async fn an_empty_patch_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty-patch");
    let token = seed_token(db.pool()).await;
    let id = new_collection(&db, &fixture, &token, "合集").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/clip-collections/{id}"),
            &token,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "一个字段都没给应当 422: {body}"
    );
    assert_eq!(code_of(&body), "validation_error");
}

/// **显式 `null` 等于「不改」** —— 刻意偏离上游，见 DTO 文档。
///
/// 上游 `{"name": null}` 会让 `_normalize_name(None)` 调 `.strip()` 崩成 500。
/// Rust 侧 `Option<String>` 天然把「不给出」与「给出 null」收成同一个 `None`。
#[tokio::test]
async fn an_explicit_null_is_treated_as_not_provided() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("null-patch");
    let token = seed_token(db.pool()).await;

    let (status, created) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/clip-collections",
            &token,
            Some(json!({ "name": "原名", "description": "原说明" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().expect("id");

    // 只把 name 显式给成 null，description 照常给值 —— 这样才能隔离出
    // 「null 等于不提供」这一条，而不被「空更新 422」那条规则盖住。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/clip-collections/{id}"),
            &token,
            Some(json!({ "name": null, "description": "新说明" })),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "显式 null 不该 500（上游会 AttributeError -> 500）: {body}"
    );
    assert_eq!(body["name"], "原名", "null 的字段应保持原值");
    assert_eq!(body["description"], "新说明", "给了值的字段应被改");

    // 两个字段都是 null == 都没给 == 空更新 422。这不是 500，也不是 200。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/clip-collections/{id}"),
            &token,
            Some(json!({ "name": null, "description": null })),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "全 null 与空更新同义: {body}"
    );
}

/// 空串描述是合法的「清空」，与「不给出」不同。
#[tokio::test]
async fn an_empty_description_clears_it() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("clear-desc");
    let token = seed_token(db.pool()).await;

    let (_, created) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/clip-collections",
            &token,
            Some(json!({ "name": "合集", "description": "说明" })),
        ),
    )
    .await;
    let id = created["id"].as_i64().expect("id");

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/clip-collections/{id}"),
            &token,
            Some(json!({ "description": "" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "", "空串是清空，不是「不改」");
}

/// 幂等：重复加入同一个片段都成功，且不改变位置。
#[tokio::test]
async fn adding_the_same_clip_twice_is_idempotent() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("idempotent-add");
    let token = seed_token(db.pool()).await;
    let clip = seed_clip(&db, &fixture, "AAA-001").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;

    add_member(&db, &fixture, &token, id, clip).await;
    add_member(&db, &fixture, &token, id, clip).await;

    let (_, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            &format!("/clip-collections/{id}/clips"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(body["total"], 1, "重复加入不该产生第二行");
    assert_eq!(body["items"][0]["position"], 0, "位置不该被改动");
}
/// `set_clips` 同时覆盖重排与批量设置，且去重保序。
#[tokio::test]
async fn set_clips_replaces_the_whole_ordered_list() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("set");
    let token = seed_token(db.pool()).await;

    let a = seed_clip(&db, &fixture, "AAA-001").await;
    let b = seed_clip(&db, &fixture, "BBB-002").await;
    let c = seed_clip(&db, &fixture, "CCC-003").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    let uri = format!("/clip-collections/{id}/clips");

    // 顺序 [c, a]，且 c 出现两次 -> 去重后 [c, a]
    let (status, body) = send(
        app(&db, &fixture),
        authed("PUT", &uri, &token, Some(json!({ "clip_ids": [c, a, c] }))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "响应: {body}");

    let (_, listed) = send(app(&db, &fixture), authed("GET", &uri, &token, None)).await;
    let items = listed["items"].as_array().expect("数组");
    assert_eq!(items.len(), 2, "重复 id 应被去重: {listed}");
    assert_eq!(items[0]["clip_id"], c, "以首次出现的位置为准");
    assert_eq!(items[0]["position"], 0);
    assert_eq!(items[1]["clip_id"], a);
    assert_eq!(items[1]["position"], 1);

    // 再设成 [b] -> 覆盖，且位置重置为 0
    let (status, _) = send(
        app(&db, &fixture),
        authed("PUT", &uri, &token, Some(json!({ "clip_ids": [b] }))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = send(app(&db, &fixture), authed("GET", &uri, &token, None)).await;
    let items = listed["items"].as_array().expect("数组");
    assert_eq!(items.len(), 1, "整体替换，不是追加");
    assert_eq!(items[0]["clip_id"], b);
    assert_eq!(items[0]["position"], 0, "位置应重置为 0");
}

/// `set_clips` 里的片段不存在时 404，且**合集成员不变**。
///
/// 上游在事务外先逐个校验，所以「收 404 时合集还是原样」是契约的一部分。
#[tokio::test]
async fn set_clips_validates_everything_before_touching_anything() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("set-404");
    let token = seed_token(db.pool()).await;
    let good = seed_clip(&db, &fixture, "AAA-001").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    let uri = format!("/clip-collections/{id}/clips");

    send(
        app(&db, &fixture),
        authed("PUT", &uri, &token, Some(json!({ "clip_ids": [good] }))),
    )
    .await;

    let ghost = 2_147_483_647i64;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PUT",
            &uri,
            &token,
            Some(json!({ "clip_ids": [good + 1, ghost] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(
        code_of(&body),
        "media_clip_not_found",
        "报错的是片段不是合集，且码用实体名而非详情键"
    );

    let (_, listed) = send(app(&db, &fixture), authed("GET", &uri, &token, None)).await;
    assert_eq!(
        listed["total"], 1,
        "失败的请求不该动数据 —— 原来的成员应还在: {listed}"
    );
}

// ------------------------------------------------------------------ 删除

/// 删合集清掉成员行，但**不动片段本体**。
#[tokio::test]
async fn deleting_a_collection_cascades_members_but_keeps_clips() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("delete");
    let token = seed_token(db.pool()).await;
    let clip = seed_clip(&db, &fixture, "AAA-001").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    add_member(&db, &fixture, &token, id, clip).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("DELETE", &format!("/clip-collections/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "响应: {body}");
    assert!(body.is_null(), "204 不该有响应体");

    let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clip_collection_item")
        .fetch_one(db.pool())
        .await
        .expect("数成员");
    assert_eq!(members, 0, "成员行应由外键 CASCADE 清掉");
    assert_eq!(clip_count(&db).await, 1, "片段本体必须保留");
}

/// 移出成员后 `total` 归零；重复移出幂等。
#[tokio::test]
async fn removing_a_member_lowers_the_count() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("remove");
    let token = seed_token(db.pool()).await;
    let clip = seed_clip(&db, &fixture, "AAA-001").await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    let uri = format!("/clip-collections/{id}/clips");
    add_member(&db, &fixture, &token, id, clip).await;

    let (_, before) = send(app(&db, &fixture), authed("GET", &uri, &token, None)).await;
    assert_eq!(before["total"], 1);

    for _ in 0..2 {
        let (status, body) = send(
            app(&db, &fixture),
            authed("DELETE", &format!("{uri}/{clip}"), &token, None),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    let (_, after) = send(app(&db, &fixture), authed("GET", &uri, &token, None)).await;
    assert_eq!(after["total"], 0, "计数应归零");
}
// ------------------------------------------------------------------ 错误与鉴权

#[tokio::test]
async fn an_unknown_collection_is_404_with_the_id_echoed() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("404");
    let token = seed_token(db.pool()).await;
    let shared = app(&db, &fixture);

    for (method, uri) in [
        ("GET", "/clip-collections/2147483647"),
        ("PATCH", "/clip-collections/2147483647"),
        ("DELETE", "/clip-collections/2147483647"),
        ("GET", "/clip-collections/2147483647/clips"),
    ] {
        let (status, body) =
            send(shared.clone(), authed(method, uri, &token, Some(json!({})))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
        assert_eq!(
            code_of(&body),
            "clip_collection_not_found",
            "{method} {uri}"
        );
    }
}

/// 分页越界用本域错误码。
#[tokio::test]
async fn rejected_page_parameters_use_the_domain_error_code() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("page-codes");
    let token = seed_token(db.pool()).await;
    let id = new_collection(&db, &fixture, &token, "合集").await;
    let shared = app(&db, &fixture);

    for query in ["page=0", "page=-1", "page_size=0", "page_size=101"] {
        let (status, body) = send(
            shared.clone(),
            authed(
                "GET",
                &format!("/clip-collections/{id}/clips?{query}"),
                &token,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{query} 应当 422: {body}"
        );
        assert_eq!(
            code_of(&body),
            "invalid_clip_collection_filter",
            "{query} 应当用本域错误码"
        );
    }
}

/// 每个端点都必须 401。逐个断言 —— 挂错 `CurrentUser` 是这类改动最常见的漏，
/// 而漏一个端点不会让其它端点失败。
#[tokio::test]
async fn every_endpoint_requires_authentication() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("auth");
    seed_token(db.pool()).await; // 用户存在，但请求不带 token
    let shared = app(&db, &fixture);

    let requests: Vec<(&str, &str)> = vec![
        ("GET", "/clip-collections"),
        ("POST", "/clip-collections"),
        ("GET", "/clip-collections/1"),
        ("PATCH", "/clip-collections/1"),
        ("DELETE", "/clip-collections/1"),
        ("GET", "/clip-collections/1/clips"),
        ("PUT", "/clip-collections/1/clips"),
        ("PUT", "/clip-collections/1/clips/2"),
        ("DELETE", "/clip-collections/1/clips/2"),
    ];

    for (method, uri) in requests {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(br#"{"name":"x"}"#.to_vec()))
            .unwrap();
        let (status, body) = send(shared.clone(), request).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} 应当 401，得到 {status}: {body}"
        );
        assert_eq!(code_of(&body), "unauthorized", "{method} {uri}");
    }
}
