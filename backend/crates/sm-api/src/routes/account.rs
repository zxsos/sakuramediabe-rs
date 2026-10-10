//! `GET /account`、`PATCH /account`、`POST /account/password`，
//! 以及 `/account/api-keys` 的三个端点。
//!
//! 对应上游 `src/api/routers/system/account.py`（**6 个端点**）。
//!
//! # 鉴权挂在 handler 上，且**只有这六个端点**
//!
//! 上游 router 级只有 `db_deps`，六个端点各自声明
//! `current_user=Depends(get_current_user)`（`account.py`）。这里照此
//! 逐个写 `CurrentUser` 提取器 —— 而不是靠一个 router 级 layer 悄悄生效，
//! 后者会让「这个端点其实没鉴权」变得看不见。
//!
//! # 改的是**当前登录者**，不接受 body 里带 id
//!
//! 用户身份一律取自 JWT（[`CurrentUser::id`]）。`AccountPasswordChangeRequest`
//! 里刻意**没有** `username` 字段 —— 多一个就意味着「改别人的密码」这条路。
//! `/account/api-keys` 同理：上游这三个 handler 拿到 `current_user` 却**不按
//! 它过滤**（单用户部署，密钥不属于某个用户），但仍要求登录 —— 所以这里写
//! `_user` 而不是干脆不写，让「必须登录」在签名上可见。
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
//!
//! # `/account/api-keys` 的三条契约要点
//!
//! | 端点 | 状态 | 要点 |
//! |---|---|---|
//! | `GET` | 200 | **顶层 JSON 数组**（不是分页壳）—— 客户端走 `getList` |
//! | `POST` | 201 | 响应多一个 `key` 明文，**仅此一次** |
//! | `DELETE /{id}` | 204 | 删不到 → 404 `api_key_not_found`（无 details） |
//!
//! 列表**不用分页壳**是因为上游 `response_model=list[ApiKeyResource]` 就是
//! 裸数组，而 Flutter 的 `apiClient.getList('/account/api-keys')` 按裸数组
//! 解析 —— 包一层 `{"items": [...]}` 客户端直接抛 "Expected JSON array"。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use sm_service::system::{AccountService, ApiKeyService};

use crate::auth::CurrentUser;
use crate::dto::{
    AccountPasswordChangeRequest, AccountResource, AccountUpdateRequest, ApiKeyCreateRequest,
    ApiKeyCreatedResource, ApiKeyResource,
};
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
        .route(
            "/account/api-keys",
            get(list_api_keys)
                .post(create_api_key)
                .fallback(method_not_allowed),
        )
        .route(
            "/account/api-keys/{key_id}",
            delete(delete_api_key).fallback(method_not_allowed),
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

/// `GET /account/api-keys` → `ApiKeyResource[]`（**裸数组**）
///
/// `_user` 只为「必须登录」而存在：上游这三个 handler 拿到 `current_user`
/// 但不使用它（单用户部署），仍要求登录。不写这个参数就没有鉴权。
async fn list_api_keys(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApiKeyResource>>, ErrorResponse> {
    let service = ApiKeyService::new(state.db());
    let keys = service.list().await?;
    Ok(Json(keys.iter().map(ApiKeyResource::from).collect()))
}

/// `POST /account/api-keys` → 201 `ApiKeyCreatedResource`
async fn create_api_key(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(body): EnvelopeJson<ApiKeyCreateRequest>,
) -> Result<(StatusCode, Json<ApiKeyCreatedResource>), ErrorResponse> {
    let service = ApiKeyService::new(state.db());
    let (row, plain_key) = service.create(&body.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(ApiKeyCreatedResource::new(ApiKeyResource::from(&row), plain_key)),
    ))
}

/// `DELETE /account/api-keys/{key_id}` → 204 No Content
async fn delete_api_key(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(key_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    let service = ApiKeyService::new(state.db());
    service.delete(key_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
