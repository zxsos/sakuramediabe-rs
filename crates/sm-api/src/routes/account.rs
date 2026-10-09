//! `GET /account`、`PATCH /account`、`POST /account/password`。
//!
//! 对应上游 `src/api/routers/system/account.py`（**3 个端点**）。
//!
//! # 鉴权挂在 handler 上，且**只有这三个端点**
//!
//! 上游 router 级只有 `db_deps`，三个端点各自声明
//! `current_user=Depends(get_current_user)`（`account.py:15/20/26`）。这里照此
//! 逐个写 `CurrentUser` 提取器 —— 而不是靠一个 router 级 layer 悄悄生效，
//! 后者会让「这个端点其实没鉴权」变得看不见。
//!
//! # 改的是**当前登录者**，不接受 body 里带 id
//!
//! 用户身份一律取自 JWT（[`CurrentUser::id`]）。`AccountPasswordChangeRequest`
//! 里刻意**没有** `username` 字段 —— 多一个就意味着「改别人的密码」这条路。
//!
//! # 改密码返回 204 且**响应体为空**
//!
//! 上游 `return Response(status_code=204)`。这里用 `StatusCode::NO_CONTENT`
//! 而不是 `Json(...)` —— 204 带 body 是非法的，axum 会照发，客户端解析失败。
//!
//! # 改密码的**实际作用**是让别人的会话失效
//!
//! `AccountService::change_password` 在写完新哈希后会吊销该用户全部 refresh
//! token（`sm_service::system::account` 的模块文档有完整说明）。所以 204 意味
//! 着「密码已改且所有会话已作废」，客户端应引导用户重新登录。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use sm_service::system::AccountService;

use crate::auth::CurrentUser;
use crate::dto::{AccountPasswordChangeRequest, AccountResource, AccountUpdateRequest};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/account",
            get(get_account)
                .patch(update_account)
                .fallback(method_not_allowed),
        )
        .route(
            "/account/password",
            post(change_password).fallback(method_not_allowed),
        )
}

/// `GET /account` → `AccountResource`
async fn get_account(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<AccountResource>, ErrorResponse> {
    let account = AccountService::new(state.db());
    let user = account.get_account(user.id).await?;
    Ok(Json(AccountResource::from(&user)))
}

/// `PATCH /account` → `AccountResource`
///
/// 只认 `username` 一个字段。上游是 `setattr` 逐字段set，那里「把请求体的
/// key 当列名」；这里显式只处理一个 —— 要加可写字段必须在 service 与此
/// **各加一行**，让「新增可写字段」变成需要被看见的动作。
async fn update_account(
    user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(body): EnvelopeJson<AccountUpdateRequest>,
) -> Result<Json<AccountResource>, ErrorResponse> {
    let account = AccountService::new(state.db());
    let updated = account.update_username(user.id, &body.username).await?;
    Ok(Json(AccountResource::from(&updated)))
}

/// `POST /account/password` → 204 No Content
///
/// 四个失败码各是一种意思，别混：
///
/// | 状态 | 码 | 含义 |
/// |---|---|---|
/// | 401 | `invalid_credentials` | 旧密码错。与登录同一套码 —— 客户端不需要区分 |
/// | 422 | `validation_error` | 新密码不满足哈希库的要求 |
/// | 404 | `user_not_found` | JWT 有效但用户行没了（并发删号） |
async fn change_password(
    user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(body): EnvelopeJson<AccountPasswordChangeRequest>,
) -> Result<StatusCode, ErrorResponse> {
    let account = AccountService::new(state.db());
    account
        .change_password(user.id, &body.current_password, &body.new_password)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
