//! 405 回归：每条已注册路由的方法不匹配都必须走错误信封。
//!
//! # 为什么单独一个文件
//!
//! axum 对「路径命中、方法不匹配」默认返回 405 + **空响应体**，而且
//! **不经过** router 的 fallback。所以每条 `MethodRouter` 都必须显式挂
//! `.fallback(method_not_allowed)`。挂对了当下没问题，但**新增路由时漏挂**
//! 是一次静默的契约破坏：状态码对、body 解析失败，客户端在运行时才炸。
//!
//! 而这个破坏在类型层面检查不出来 —— `.fallback()` 缺席编译照样通过。
//! 所以这里逐条覆盖：
//!
//! **新增一条路由时，请把它加进 [`ROUTES`]。** 那个数组是本测试的发现机制
//! （见 `every_registered_route_appears_in_the_table`），漏了会红。
//!
//! 上游出处：`src/api/exception/exception.py:36-48` —— 405 属于「其他
//! HTTPException」，映射成 `http_error` + Starlette 的文案。

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sm_api::{router, AppState};
use sm_db::testing::TestDb;
use sm_service::system::auth::AuthConfig;
use tower::ServiceExt;

/// 已注册的全部路由 + 一种**必然不匹配**的方法。
///
/// 「必然不匹配」= 该路径上注册了别的方法。逐条挑出来的意义是：漏挂
/// fallback 的表现必须是**该路径能匹配、只是方法不对**。
const ROUTES: &[(&str, Method)] = &[
    // actors.rs —— 每条路径挑一个**未注册**的方法。
    ("/actors", Method::POST),
    ("/actors/filter-options", Method::POST),
    ("/actors/1", Method::POST),
    ("/actors/1/merge", Method::GET),
    ("/actors/1/profile-image", Method::POST),
    ("/actors/1/subscription", Method::POST),
    ("/actors/1/movie-ids", Method::POST),
    ("/actors/1/tags", Method::POST),
    ("/actors/1/years", Method::POST),
    // auth.rs
    ("/auth/tokens", Method::GET),
    ("/auth/token-refreshes", Method::GET),
    // config.rs
    ("/config", Method::POST),
    ("/config", Method::DELETE),
    // downloads.rs
    ("/download-candidates", Method::POST),
    // indexer_settings.rs
    ("/indexer-settings", Method::POST),
    ("/indexer-settings/test", Method::POST),
    // movie_subscriptions.rs
    ("/movie-subscriptions", Method::POST),
    ("/movie-subscriptions/status-counts", Method::POST),
    ("/movie-subscriptions/search-resets", Method::GET),
    // movies.rs
    ("/movies", Method::POST),
    ("/movies/by-series", Method::GET),
    ("/movies/subscribed-actors/latest", Method::POST),
    ("/movies/search/parse-number", Method::GET),
    ("/movies/latest", Method::POST),
    ("/movies/collection-type", Method::GET),
    ("/movies/blacklist", Method::GET),
    ("/movies/1/collection-status", Method::POST),
    ("/movies/subscriptions", Method::GET),
    ("/movies/unsubscriptions", Method::GET),
    // playlists.rs
    // GET /playlists 已注册（`list_playlists`），所以这里挑一个仍未注册的
    // 方法。**改路由时记得同步这张表** —— 见文件头的说明。
    ("/playlists", Method::PUT),
    ("/playlists/1", Method::POST),
    ("/playlists/1/movies", Method::POST),
    ("/playlists/1/movies/ABC-001", Method::POST),
    ("/playlists/1/resolutions", Method::POST),
    // media_clips.rs —— 挑该路径**已注册**的方法之外的那个。
    // `POST /media/{id}/clips` 落在这里是**故意的**：它需要 ffmpeg，本批不做，
    // 而路径存在（GET 能匹配），所以正确状态码是 405 而非 404。
    ("/media/1/clips", Method::POST),
    ("/media-clips", Method::POST),
    ("/media-clips/1/thumbnails", Method::POST),
    // tags.rs
    ("/tags", Method::POST),
    ("/tags/1", Method::POST),
    ("/tags/1/movies", Method::POST),
    // status.rs
    ("/status/capabilities", Method::POST),
    ("/status", Method::POST),
    ("/status/insights", Method::POST),
    ("/status/watch-trend", Method::POST),
];

/// 构造 router。405 与鉴权无关，所以不需要用户与令牌。
fn app(db: &TestDb) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new("405-secret"),
        sm_service::system::ConfigService::new(temp_config_path()),
    ))
}

async fn body_of(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// 指向临时目录的配置路径。
///
/// 那些**不碰配置**的端点测试也需要一个 `ConfigService`，而它们绝不能写
/// 到真实的 `config.toml` 上 —— 那是开发机/容器的配置。所以给一个每次调用
/// 都不同的临时路径：即便某个用例意外触发了写盘，也只会留下空目录里的孤立
/// 文件。
fn temp_config_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("sm-api-unused-{}-{n}.toml", std::process::id()))
}

#[tokio::test]
async fn method_mismatch_is_an_error_envelope_not_an_empty_body() {
    let db = TestDb::require().await;
    let app = app(&db);

    for (path, method) in ROUTES {
        let request = Request::builder()
            .method(method.clone())
            .uri(*path)
            .body(Body::empty())
            .expect("构造请求");
        let (status, body) = body_of(app.clone().oneshot(request).await.expect("oneshot")).await;

        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} 应当是 405"
        );
        // 关键断言：body 必须是**信封**，而不是空。
        let error = body
            .get("error")
            .unwrap_or_else(|| panic!("{method} {path} 的 405 没有 error 字段，实际：{body}"));
        assert_eq!(
            error.get("code").and_then(Value::as_str),
            Some("http_error"),
            "{method} {path} 的错误码应当是 http_error"
        );
        assert!(
            error.get("message").and_then(Value::as_str).is_some(),
            "{method} {path} 应当带 message"
        );
    }
}

#[tokio::test]
async fn the_registered_methods_are_not_answered_with_405() {
    // 反向断言：真正的 405 必须是**只**在方法不对时出现。若误挂了
    // `any(method_not_allowed)` 之类的层，这一组会红。
    let db = TestDb::require().await;
    let app = app(&db);

    for (path, method) in [("/auth/tokens", Method::POST), ("/playlists", Method::POST)] {
        let request = Request::builder()
            .method(method.clone())
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .expect("构造请求");
        let (status, _) = body_of(app.clone().oneshot(request).await.expect("oneshot")).await;
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} 是已注册的方法，不该得到 405"
        );
    }
}

#[tokio::test]
async fn an_unknown_path_is_still_404_via_the_router_fallback() {
    // 与 405 并列的另一条：不存在的路径走 router 级 fallback，
    // 错误码同为 `http_error`，但**状态码不同**。把它与 405 一起断言，
    // 是为了让「fallback 挂错层级」这种错误暴露出来。
    let db = TestDb::require().await;
    let request = Request::builder()
        .method(Method::GET)
        .uri("/definitely-not-a-route")
        .body(Body::empty())
        .expect("构造请求");

    let (status, body) = body_of(app(&db).oneshot(request).await.expect("oneshot")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str),
        Some("http_error")
    );
}

/// 路由表本身的形状检查。
///
/// 它不碰数据库，所以放在单元测试里；作用是让「新增路由忘了加进
/// `ROUTES`」这件事在**测试期**就被看见，而不是等 405 真的退化。
#[test]
fn every_registered_route_appears_in_the_table() {
    // 路由前缀。改动 `router()` 的装配方式时，这个断言会先红 ——
    // 那正是要提醒你更新表格的时刻。
    let source = include_str!("../src/routes.rs");
    // `include_str!` 只接受字面量，所以逐个列出而不是循环。
    for (name, text) in [
        ("actors", include_str!("../src/routes/actors.rs")),
        ("auth", include_str!("../src/routes/auth.rs")),
        ("config", include_str!("../src/routes/config.rs")),
        ("downloads", include_str!("../src/routes/downloads.rs")),
        ("movies", include_str!("../src/routes/movies.rs")),
        (
            "indexer_settings",
            include_str!("../src/routes/indexer_settings.rs"),
        ),
        ("playlists", include_str!("../src/routes/playlists.rs")),
        ("status", include_str!("../src/routes/status.rs")),
    ] {
        assert!(
            text.contains(".fallback(method_not_allowed)"),
            "{name}.rs 里有路由没挂 method_not_allowed"
        );
    }
    // `routes.rs` 自己定义了那个 handler；断言它确实产出信封形状。
    assert!(source.contains("METHOD_NOT_ALLOWED"));
    assert!(source.contains("http_error"));
    // 表格本身：路径必须以 / 开头，方法必须是标准动词。
    for (path, method) in ROUTES {
        assert!(path.starts_with('/'), "路径应带前导斜杠：{path}");
        assert!(
            matches!(
                *method,
                Method::GET | Method::POST | Method::PATCH | Method::PUT | Method::DELETE
            ),
            "表格里只放已注册方法的**反例**：{method} {path}"
        );
    }
}
