//! `GET/PATCH /indexer-settings` —— 索引器配置。
//!
//! # 与上游 `src/api/routers/system/indexer_settings.py` 的对应
//!
//! | 上游端点 | 依赖 | 状态 |
//! |---|---|---|
//! | `GET ""` | `IndexerSettingsService.get_settings` | **已落**（本文件） |
//! | `PATCH ""` | `IndexerSettingsService.update_settings` | **已落**（本文件） |
//! | `GET /test` | `IndexerSettingsService.test_connection` | 阻塞：`transfers` 域的 Torznab 客户端 |
//!
//! # 鉴权挂在 handler 上
//!
//! 上游这个 router 是 `dependencies=[Depends(db_deps)]` + 逐 handler 声明
//! `current_user`；本仓库统一把 `CurrentUser` 写成 handler 参数（理由见
//! [`crate::routes::playlists`] 的模块文档）。
//!
//! # `api_key` 的三态必须原样传到 service
//!
//! 省略 / `null` / 有值 —— 三者在 service 层是三件不同的事（沿用旧值 /
//! 清空 / 设为新值）。用 `Option<Option<String>>` 承载，serde 的
//! `#[serde(default)]` 区分「键不存在」与「键存在但为 null」。写成
//! `Option<String>` 会把前两种合并，升级后用户不改 key 保存一次就会清空
//! 所有索引器的 key。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::system::indexer_settings::{
    BoundClientResource, IndexerItemResource, IndexerItemUpdate, IndexerSettingsService,
    IndexerSettingsUpdateRequest,
};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/indexer-settings",
        get(get_indexer_settings)
            .patch(update_indexer_settings)
            .fallback(method_not_allowed),
    )
}

/// `GET /indexer-settings` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerSettingsResource {
    pub indexers: Vec<IndexerItemResourceResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerItemResourceResponse {
    pub id: i32,
    pub name: String,
    pub url: String,
    /// `pt` / `bt`。**已校验**。
    pub kind: String,
    /// **明文返回**（上游如此，注释写「前端自律」）。空表示不带 apikey。
    pub api_key: Option<String>,
    /// 绑定的下载器，**按绑定顺序**。
    pub download_clients: Vec<BoundClientResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundClientResponse {
    pub id: i32,
    pub name: String,
}

impl From<sm_service::system::indexer_settings::IndexerSettingsResource>
    for IndexerSettingsResource
{
    fn from(value: sm_service::system::indexer_settings::IndexerSettingsResource) -> Self {
        Self {
            indexers: value.indexers.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<IndexerItemResource> for IndexerItemResourceResponse {
    fn from(value: IndexerItemResource) -> Self {
        Self {
            id: value.id,
            name: value.name,
            url: value.url,
            kind: value.kind,
            api_key: value.api_key,
            download_clients: value
                .download_clients
                .into_iter()
                .map(|c: BoundClientResource| BoundClientResponse {
                    id: c.id,
                    name: c.name,
                })
                .collect(),
        }
    }
}

async fn get_indexer_settings(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<IndexerSettingsResource>, ErrorResponse> {
    Ok(Json(
        IndexerSettingsService::new(state.db())
            .get_settings()
            .await?
            .into(),
    ))
}

// ================================================================ PATCH

/// `PATCH /indexer-settings` 的请求体。
///
/// 字段集合照抄上游 `IndexerSettingsUpdateRequest`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IndexerSettingsUpdateRequestBody {
    /// 兼容旧版前端：已废弃，**记录但不生效**。
    #[serde(rename = "type")]
    pub legacy_type: Option<String>,
    /// 同上。
    pub api_key: Option<String>,
    /// 缺省 = 不动索引器；`Some([])` = 清空全部。
    #[serde(default)]
    pub indexers: Option<Vec<IndexerItemUpdateBody>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IndexerItemUpdateBody {
    pub name: String,
    pub url: String,
    pub kind: String,
    /// **三态**：`None` = 键不存在（沿用旧值）；`Some(None)` = 显式 null
    /// （清空）；`Some(Some(k))` = 设为 `k`。
    ///
    /// 必须配 `de_tri_state_string`（见下）—— 光靠 `Option<Option<String>>`
    /// **做不到**：serde 对「键不存在」用 `default` 给出 `None`，对「键存在但为
    /// null」也让外层 `Option` 解成 `None`。两者变成同一个值，于是
    /// `api_key: null` 会被当成「省略」，而 service 的省略语义是**沿用旧值**
    /// —— 结果是「清空」变成了「保留」，且没有任何报错。
    #[serde(default, deserialize_with = "de_tri_state_string")]
    pub api_key: Option<Option<String>>,
    #[serde(default)]
    pub download_client_ids: Vec<i32>,
}

/// 把「键不存在 / 显式 null / 有值」解析成三态。
///
/// # 为什么不能靠 `Option<Option<String>>`
///
/// serde 对 `Option<T>` 的反序列化规则是「JSON `null` → `None`」，而**键
/// 不存在**时 `#[serde(default)]` 也给 `None`。所以两种输入落到同一个值，
/// 双层 `Option` 白加了。
///
/// # 这里的判据是「反序列化器有没有被调用」
///
/// - **键不存在** → `#[serde(default)]` 生效，反序列化器**根本不被调用**
///   → `None`。
/// - **键存在且为 `null`** → 反序列化器被调用，收到 `null`
///   → `Some(None)`（清空）。
/// - **键存在且有值** → `Some(Some(v))`。
///
/// 同一个文件里 `indexers: Option<Vec<..>>` 只需要 `#[serde(default)]` ——
/// 那里「缺省」与「null」**语义相同**（都是不动索引器），所以不需要三态。
fn de_tri_state_string<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    match raw {
        // 被调用且收到 null => 键存在且显式为 null => 清空
        None => Ok(Some(None)),
        Some(value) => match serde_json::from_value::<String>(value) {
            Ok(s) => Ok(Some(Some(s))),
            // 类型不对（数字/对象/数组）—— 交给 serde 的错误路径，
            // 最终变成 422 `validation_error`，而不是静默当成 null。
            Err(err) => Err(serde::de::Error::custom(format!(
                "api_key 必须是字符串或 null：{err}"
            ))),
        },
    }
}

impl From<IndexerSettingsUpdateRequestBody> for IndexerSettingsUpdateRequest {
    fn from(value: IndexerSettingsUpdateRequestBody) -> Self {
        Self {
            legacy_type: value.legacy_type,
            legacy_api_key: value.api_key,
            indexers: value.indexers.map(|items| {
                items
                    .into_iter()
                    .map(|item| IndexerItemUpdate {
                        name: item.name,
                        url: item.url,
                        kind: item.kind,
                        api_key: item.api_key,
                        download_client_ids: item.download_client_ids,
                    })
                    .collect()
            }),
        }
    }
}

async fn update_indexer_settings(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<IndexerSettingsUpdateRequestBody>,
) -> Result<Json<IndexerSettingsResource>, ErrorResponse> {
    let updated = IndexerSettingsService::new(state.db())
        .update_settings(payload.into())
        .await?;
    // 返回**替换后**的完整设置（上游同样返回 `get_settings()`）——
    // 客户端据此刷新本地状态，不用再发一次 GET。
    Ok(Json(updated.into()))
}
