//! axum 路由层。
//!
//! # 本批交付什么
//!
//! 骨架四件套，目的是让 `sm-service` 里已完成的业务规则**第一次能被 HTTP
//! 触达并端到端验证**：
//!
//! | 件 | 位置 | 对齐上游 |
//! |---|---|---|
//! 错误信封 → 响应 | [`error::ErrorResponse`] | `src/api/exception/exception.py:29-73` |
//! 鉴权提取器 | [`auth::CurrentUser`] | `src/service/system/auth_service.py:111-128` |
//! CORS | [`router`] | `src/api/app.py:83-89`（`allow_* = ["*"]`） |
//! 路由 | [`routes::auth`] | `src/api/routers/system/auth.py` |
//! 路由 | [`routes::playlists`] | `src/api/routers/collections/playlists.py` |
//!
//! # 鉴权不是一个开关
//!
//! 端点分三类，**逐个 handler 显式声明**，不要靠 router 级 layer 悄悄生效：
//!
//! | 类 | 例子 | 写法 |
//! |---|---|---|
//! 需要 JWT | `/playlists/*`、`/auth/token-refreshes` | handler 带 `CurrentUser` 参数 |
//! 不需要鉴权 | `/auth/tokens` | 无 `CurrentUser` 参数 |
//! **旁路签名** | `/files/*`、`/media/{id}/play/{path}` | `sm_core::signing`，不是 JWT |
//!
//! # 尚未接入（不要以为已经好了）
//!
//! - **SSE**：13 个事件 / 3 个流，用 axum 自带 `response::sse`，未接。
//! - **签名 URL 的路由**：`sm_core::signing` 与 403 的三个错误码已就位
//!   （见 [`error::ErrorResponse`] 的 `From<SignatureError>`），但
//!   `files/*` 与 `/media/{id}/play/{path}` 这两条**旁路签名**路由还没接 —
//!   它们要读 provider 的 `playback_deliveries`，属插件 ABI 那批。
//! - **multipart**：提取器已就绪（[`extract::Multipart`]），但没有调用它的
//!   路由 —— 上传插件 zip / 图片要等插件与 provider 资源。
//!
//! # 已闭合的坑（别再写成缺口）
//!
//! **405 走信封**：axum 的方法不匹配不经过 router fallback，所以每条
//! `MethodRouter` 都挂了 [`routes::method_not_allowed`]，回归测试见
//! `tests/method_not_allowed_http.rs`。

#![forbid(unsafe_code)]

pub mod auth;
pub mod dto;
pub mod error;
pub mod extract;
pub mod middleware;
pub mod query;
pub mod routes;
pub mod sse;
pub mod state;

pub use auth::CurrentUser;
pub use error::{not_found, ErrorResponse};
pub use state::AppState;

use axum::Router;
use tower_http::cors::CorsLayer;

/// 装配完整路由。
///
/// `with_state` 放最后：先合并业务路由、再挂 fallback、再套 CORS 层，
/// 顺序决定了 fallback 是否也享受 CORS —— 上游 CORS 是全站生效的。
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::auth::routes())
        .merge(routes::config::routes())
        .merge(routes::indexer_settings::routes())
        .merge(routes::playlists::routes())
        .merge(routes::status::routes())
        .fallback(error::not_found)
        .layer(CorsLayer::permissive())
        .with_state(state)
}
