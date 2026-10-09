//! `GET /ranking-sources*` —— 排行榜读侧三个端点。
//!
//! # 与上游 `src/api/routers/discovery/ranking_sources.py` 的对应
//!
//! | 上游端点 | 依赖 | 本 crate |
//! |---|---|---|
//! | `GET /ranking-sources` | `RankingCatalogService.list_sources` | [`list_ranking_sources`] |
//! | `GET /ranking-sources/{source_key}/boards` | `list_boards` | [`list_ranking_boards`] |
//! | `GET /ranking-sources/{source_key}/boards/{board_key}/items` | `list_board_items` | [`list_ranking_board_items`] |
//!
//! # 三个端点全是读侧，**不依赖 provider 插件**
//!
//! 这是本文件存在的意义：排行榜的**写侧**（`RankingSyncService`）确实被
//! provider 插件卡住，但**读侧不卡** —— 榜单定义来自插件的注册信息，条目来自
//! PostgreSQL。所以这三个端点可以先上，插件接上后数据会自动出现。
//!
//! # `period` 与 `sort` 的错误语义不同，别统一处理
//!
//! - `period`：给了但该榜单不支持 → **422**（上游 [`ranking::RankingCatalogService`]
//!   的 `_resolve_period` 明确报错）。静默回退到默认周期会让调用方以为拿到
//!   的是周榜。
//! - `sort`：非法值**降级**（见 [`crate::query`] 的既有约定），因为上游这里本身宽松。
//!
//! 同一个端点里两种错误语义是上游的真实状态，不是本仓库的疏忽。

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::discovery::ranking::{
    BoardItemPage, BoardItemResource, RankingBoardResource, RankingSourceResource,
};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ranking-sources", get(list_ranking_sources))
        .route(
            "/ranking-sources/{source_key}/boards",
            get(list_ranking_boards),
        )
        .route(
            "/ranking-sources/{source_key}/boards/{board_key}/items",
            get(list_ranking_board_items),
        )
}

/// `GET /ranking-sources`
///
/// 上游 `list_ranking_sources`（`:20-22`）。无参数，返回全部排行源。
async fn list_ranking_sources(
    State(_state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<RankingSourceResource>>, ErrorResponse> {
    todo!("骨架：接 sm_service::discovery::ranking::RankingCatalogService::list_sources")
}

/// `GET /ranking-sources/{source_key}/boards`
///
/// 上游 `list_ranking_boards`（`:25-27`）。源不存在 → **404**。
async fn list_ranking_boards(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(source_key): Path<String>,
) -> Result<Json<Vec<RankingBoardResource>>, ErrorResponse> {
    todo!("骨架：接 RankingCatalogService::list_boards（404 由 service 的 not_found 产生）")
}

/// `GET /ranking-sources/{source_key}/boards/{board_key}/items`
///
/// 上游 `list_ranking_board_items`（`:33+`）。
#[derive(Debug, Deserialize)]
struct BoardItemsQuery {
    /// 不传则用榜单默认周期；传了但不支持 → 422。
    period: Option<String>,
    /// 非法值**降级**（见模块文档）。
    sort: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

/// 榜单条目分页响应。
#[derive(Debug, Serialize)]
struct BoardItemsResponse {
    items: Vec<BoardItemResource>,
    next_cursor: Option<String>,
    total: Option<i64>,
}

async fn list_ranking_board_items(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path((source_key, board_key)): Path<(String, String)>,
    axum::extract::Query(_query): axum::extract::Query<BoardItemsQuery>,
) -> Result<Json<BoardItemsResponse>, ErrorResponse> {
    todo!("骨架：接 RankingCatalogService::list_board_items（period 不支持 -> 422）")
}