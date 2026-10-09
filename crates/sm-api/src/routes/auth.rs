//! `POST /auth/tokens` 与 `POST /auth/token-refreshes`。
//!
//! # 两个端点的鉴权要求**不一样**（照抄上游，别"顺手统一"）
//!
//! | 端点 | 是否要 access token | 上游 |
//! |---|---|---|
//! `/auth/tokens` | **不需要** | `routers/system/auth.py:15-26`，无 `get_current_user` |
//! `/auth/token-refreshes` | **需要** | 同文件 `:34-43`，有 `Depends(get_current_user)` |
//!
//! 刷新要带 access token 这件事看起来多余（都有有效 token 了还刷新什么），
//! 但它是上游的既有行为，改掉会让「access 过期、只剩 refresh」的客户端
//! 拿不到新令牌 —— 那正是刷新存在的场景。上游既然这么写，就照搬。
//!
//! # `/auth/docs-token` 是**表单**登录，且不进 OpenAPI 文档
//!
//! 上游有个 `include_in_schema=False` 的 `POST /auth/docs-token`
//! （`routers/system/auth.py:46-55`），只做两件事：
//!
//! 1. 收 **form 编码**的 `username` / `password`（`OAuth2PasswordRequestForm`，
//!    不是 JSON —— 与本模块另两个端点的 `EnvelopeJson` 不同）
//! 2. 只回 `{access_token, token_type}` 两个字段（**没有** `refresh_token`）
//!
//! 它的**唯一消费方是 Swagger UI 的 OAuth2 密码表单**
//! （`deps.py:8` 的 `tokenUrl="/auth/docs-token"`）。
//!
//! ## 本仓没有那个消费方，但**照上游把端点补齐**
//!
//! 实测：`sm-api` 无 `/docs` 路由，全仓无 `utoipa` 依赖（0 处引用），
//! 无 Swagger / Redoc / Rapidoc / Scalar。一度据此判断「不实现」；**现决定
//! 照上游实现** —— 契约以**端点集合**为准，`include_in_schema=False` 那层
//! 「文档可见性」在本仓没有对应物（`enable_docs` 配置键无人读），由部署方
//! 决定是否暴露，所以本仓**不加**任何文档可见性开关。
//!
//! 代价：为 `Form` 补一个保留错误信封的提取器（见 [`crate::extract::Form`]）——
//! axum 原生的 `Form` rejection 是 415/422 的**纯文本**，与上游的 422 信封不符。

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::routing::post;
use axum::Router;
use sm_service::system::auth::{AuthService, TokenPair};

use crate::auth::CurrentUser;
use crate::dto::{TokenCreateRequest, TokenRefreshRequest, TokenResource};
use crate::error::ErrorResponse;
// 表单提取器（`application/x-www-form-urlencoded`）—— 只有 `/auth/docs-token` 用。
use crate::extract::Form as EnvelopeForm;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/auth/tokens",
            post(create_token_pair).fallback(method_not_allowed),
        )
        .route(
            "/auth/token-refreshes",
            post(refresh_token_pair).fallback(method_not_allowed),
        )
        // 形同 `/auth/tokens` 的**表单**登录。上游标了 `include_in_schema=False`
        // （给 Swagger UI 用），但本仓没有「文档可见性」这层，所以**照常注册**。
        .route(
            "/auth/docs-token",
            post(docs_login).fallback(method_not_allowed),
        )
}

/// 登录。**不带鉴权** —— 这是拿 token 的地方。
async fn create_token_pair(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    EnvelopeJson(payload): EnvelopeJson<TokenCreateRequest>,
) -> Result<(StatusCode, axum::Json<TokenResource>), ErrorResponse> {
    let service = AuthService::new(state.db());
    let pair = service
        .login(
            state.auth(),
            &payload.username,
            &payload.password,
            // 客户端 IP 需要 `ConnectInfo`（真实监听才有 socket addr），
            // 本批用 oneshot 驱动 Router，拿不到，故留空。审计列可空。
            None,
            user_agent_of(&headers).as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(TokenResource::from(pair))))
}

/// 刷新。**带鉴权**（上游行为，见模块文档）。
async fn refresh_token_pair(
    _user: CurrentUser,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    EnvelopeJson(payload): EnvelopeJson<TokenRefreshRequest>,
) -> Result<(StatusCode, axum::Json<TokenResource>), ErrorResponse> {
    let service = AuthService::new(state.db());
    let pair = service
        .refresh(
            state.auth(),
            &payload.refresh_token,
            None,
            user_agent_of(&headers).as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(TokenResource::from(pair))))
}

fn user_agent_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// 让 TokenPair → TokenResource 的转换只写一处。
impl From<TokenPair> for TokenResource {
    fn from(value: TokenPair) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            token_type: value.token_type,
            expires_in: value.expires_in,
            expires_at: crate::dto::format_utc(value.expires_at),
            refresh_expires_at: crate::dto::format_utc(value.refresh_expires_at),
            user: crate::dto::AuthUserSummary {
                username: value.username,
            },
        }
    }
}

/// `POST /auth/docs-token` —— 文档登录。**不带鉴权**。
///
/// # ★ 请求体是 **form-urlencoded**，不是 JSON
///
/// 上游签名是 `form_data: OAuth2PasswordRequestForm = Depends()`，即
/// `application/x-www-form-urlencoded` 的 `username` / `password` 字段。
///
/// ⚠️ 别写成 `Json<...>`：Swagger UI 的 `authorize` 按钮发的是表单，
/// 改成 JSON 会让文档里的「Try it out」**永远 422**。
///
/// 与 `POST /auth/tokens` 的差别：请求体是**表单**、响应**只有两个字段**，
/// 且**不传** `client_ip` / `user_agent`（上游就是这么调的）。认证逻辑相同。
async fn docs_login(
    State(state): State<AppState>,
    EnvelopeForm(form): EnvelopeForm<DocsLoginForm>,
) -> Result<axum::Json<DocsTokenResource>, ErrorResponse> {
    // 上游 `create_token_pair(username=..., password=...)` **不带** client_ip /
    // user_agent（`routers/system/auth.py:47-51`）—— 别顺手抄 `/auth/tokens` 那两行。
    let pair = AuthService::new(state.db())
        .login(state.auth(), &form.username, &form.password, None, None)
        .await?;
    Ok(axum::Json(DocsTokenResource {
        access_token: pair.access_token,
        token_type: "bearer",
    }))
}

/// `POST /auth/docs-token` 的响应体 —— **只有两个字段**。
///
/// 上游返回的是内联 dict（`routers/system/auth.py:49-52`），**不是** `TokenResource`：
/// 它**故意不给** `refresh_token` / `expires_*` / `user`（只喂给 Swagger UI 的
/// OAuth2 表单填 access token）。别图省事直接回 [`TokenResource`]。
#[derive(Debug, serde::Serialize)]
pub struct DocsTokenResource {
    pub access_token: String,
    /// 上游这个字段**写死** `"bearer"`，不是从令牌对里取。
    pub token_type: &'static str,
}

/// `OAuth2PasswordRequestForm` 的字段。`scope` / `client_id` / `client_secret` /
/// `grant_type` 上游有但**不用**，故不取（多出来的表单字段会被 serde 忽略）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DocsLoginForm {
    pub username: String,
    pub password: String,
}
