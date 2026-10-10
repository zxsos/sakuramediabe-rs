//! `GET /download-candidates` —— 下载候选搜索。
//!
//! # 与上游 `src/api/routers/transfers/downloads.py` 的对应
//!
//! 上游这个 router 有 9 条路由，本文件只落了 **1** 条 —— 其余全部落在插件 ABI
//! 宿主（下载器 provider）与任务台账后面，见 `docs/service-progress.md` 的
//! 阻塞地图。
//!
//! | 上游端点 | 依赖 | 状态 |
//! |---|---|---|
//! | `GET /download-candidates` | `DownloadSearchService` | **已落**（本文件） |
//! | `GET/POST /download-clients`、`POST /download-clients/test`、`PATCH/DELETE /download-clients/{id}` | 下载器 provider（插件 ABI） | 阻塞 |
//! | `POST /download-requests` | 下载器 provider + 任务台账 | 阻塞 |
//! | `GET /download-tasks`、`DELETE /download-tasks/{id}`、`POST /download-tasks/{id}/import` | 任务台账 + 导入流水线 | 阻塞 |
//!
//! # 两种「movie_number 不对」给不同的码，不要合并
//!
//! | 请求 | 状态 | code | 谁给的 |
//! |---|---|---|---|
//! | 不带 `movie_number` 键 | 422 | `validation_error` | serde（上游是 pydantic） |
//! | `?movie_number=`（空串） | 422 | `invalid_download_candidate_movie_number` | service |
//!
//! 上游也是两条路径两个码，所以这里不做统一 —— 客户端对前者高亮「没填」，
//! 对后者提示「不能为空」。
//!
//! # DTO 就地定义
//!
//! 与 [`crate::routes::indexer_settings`] 同一个理由：这个资源的形状只服务
//! 这一条路由，放进 `dto.rs` 只会让它离用它的地方更远。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_service::transfers::download_search::DownloadSearchService;
use sm_service::transfers::torznab::{BoundClient, TorznabCandidate};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/download-candidates",
        get(list_download_candidates).fallback(method_not_allowed),
    )
}

/// `GET /download-candidates` 的查询参数（上游 `DownloadCandidatesQuery`）。
#[derive(Debug, Clone, Deserialize)]
struct DownloadCandidatesQuery {
    /// **必填**。缺失时 serde 报错 → 422 `validation_error`（见模块文档）。
    movie_number: String,
    /// `pt` / `bt`；省略或空白 = 搜全部索引器。
    #[serde(default)]
    indexer_kind: Option<String>,
}

/// 候选可选的下载器概要（上游 `DownloadCandidateClientResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadCandidateClientResource {
    pub id: i32,
    pub name: String,
}

/// 一条下载候选（上游 `DownloadCandidateResource`）。
///
/// 顺序即搜索结果顺序（`seeders` 降序、其次 `size_bytes` 降序）—— 客户端
/// 按列表顺序渲染，不排序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadCandidateResource {
    /// 交给下载器的不透明来源，可能是磁力、种子地址或别的形态。
    pub source_uri: String,
    pub indexer_name: String,
    pub indexer_kind: String,
    /// **默认**下载器：该索引器绑定顺序的第一个。
    pub resolved_client_id: i32,
    pub resolved_client_name: String,
    /// 可选的下载器，**按绑定顺序**。
    pub download_clients: Vec<DownloadCandidateClientResource>,
    /// 请求里的番号（**已大写**），不是候选标题里解析出来的那个。
    pub movie_number: String,
    /// 清洗后的 `title` + `description`（去 HTML 标签、折叠空白）。
    pub title: String,
    pub size_bytes: i64,
    pub seeders: i32,
}

impl From<BoundClient> for DownloadCandidateClientResource {
    fn from(value: BoundClient) -> Self {
        Self {
            id: value.id,
            name: value.name,
        }
    }
}

impl From<TorznabCandidate> for DownloadCandidateResource {
    fn from(value: TorznabCandidate) -> Self {
        Self {
            source_uri: value.source_uri,
            indexer_name: value.indexer_name,
            indexer_kind: value.indexer_kind,
            resolved_client_id: value.resolved_client_id,
            resolved_client_name: value.resolved_client_name,
            download_clients: value.download_clients.into_iter().map(Into::into).collect(),
            movie_number: value.movie_number,
            title: value.title,
            size_bytes: value.size_bytes,
            seeders: value.seeders,
        }
    }
}

async fn list_download_candidates(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<DownloadCandidatesQuery>,
) -> Result<Json<Vec<DownloadCandidateResource>>, ErrorResponse> {
    let candidates = DownloadSearchService::new(state.db())
        .search_candidates(&query.movie_number, query.indexer_kind.as_deref())
        .await?;
    Ok(Json(candidates.into_iter().map(Into::into).collect()))
}
