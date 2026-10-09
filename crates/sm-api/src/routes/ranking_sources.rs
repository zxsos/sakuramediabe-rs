//! `GET /ranking-sources*` —— 排行榜读侧三个端点。
//!
//! # 与上游 `src/api/routers/discovery/ranking_sources.py` 的对应
//!
//! | 上游端点 | 依赖 | 状态 |
//! |---|---|---|
//! | `GET /ranking-sources` | `AppState::ranking`（组合根注入） | **已接** |
//! | `GET /ranking-sources/{source_key}/boards` | 同上 | ❌ 榜单定义无存放处，见下 |
//! | `GET /ranking-sources/{source_key}/boards/{board_key}/items` | DB（`ranking_item`） | **已接** |
//!
//! # 三个端点全是读侧，**不依赖 provider 插件**
//!
//! 榜单的**写侧**（`RankingSyncService`）确实被插件卡住，但**读侧不卡** ——
//! 条目来自 PostgreSQL。所以这三个端点可以先上，插件接上后数据会自动出现。
//!
//! # 数据来源：组合根注入的快照，**不是**数据库，也**不是**插件注册表
//!
//! 「有哪些排行源」这件事只由**插件注册时声明的**。而
//! `sm-plugins -> sm-scheduler -> sm-service` 是一条依赖链，
//! **`sm-service` 依赖 `sm-plugins` 会成环**；`sm-api` 同样不依赖它。
//!
//! 所以走 `AppState::ranking`（[`RankingSourceCatalog`]）—— 与
//! `AppState::jobs`（`JobCatalog`）**同一个模式**：组合根读完塞进来。
//! 详见 `sm-service/src/discovery/ranking.rs` 里 `RankingSourceCatalog` 的文档。
//!
//! # `list_boards` 的缺口是真的
//!
//! 榜单定义（`title` / `supported_periods` / `default_period`）在 Rust 侧
//! **无处存放** —— 已核实：
//!
//! - `sm_db::discovery::RankingItem` 只有条目，没有定义字段
//! - `RankingItemRepository::list_boards` 返回 `Page<(String, String, String)>`
//!   —— 是 `(board_key, period, source_key)` **元组**，不是带标题的定义
//! - `sm_plugins::registry::ProviderRegistration` 只有 `provider_key` /
//!   `display_name` / `plugin_id` / `capabilities` / `data_plane_endpoint`
//!   —— **没有 boards**
//!
//! 上游的定义来自插件**加载期**的 `register_plugin_ranking_sources(accepted, owners)`
//! （`ranking_plugin_adapter.py:109`）。Rust 侧还没接住那份载荷。
//!
//! **不写「返回空数组」的假实现** —— 那会让接口「成功」但永远没数据，
//! 比报缺口更难查。
//!
//! # `period` 与 `sort` 的错误语义不同，别统一处理
//!
//! - `period`：给了但该榜单不支持 → **422** `ranking_period_unsupported`
//!   （静默回退到默认周期会让调用方以为拿到的是周榜 —— 榜单位次差得很远，
//!   这个错误在界面上看不出来）
//! - `sort`：非法值**降级**（见 [`crate::query`] 的既有约定），因为上游这里
//!   本身宽松
//!
//! 同一端点里两种错误语义是上游的真实状态，不是本仓库的疏忽。

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use sm_service::discovery::ranking::{BoardItemPage, RankingCatalogService};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/ranking-sources",
            get(list_ranking_sources).fallback(method_not_allowed),
        )
        .route(
            "/ranking-sources/{source_key}/boards",
            get(list_ranking_boards).fallback(method_not_allowed),
        )
        .route(
            "/ranking-sources/{source_key}/boards/{board_key}/items",
            get(list_ranking_board_items).fallback(method_not_allowed),
        )
}

/// `GET /ranking-sources`
///
/// 无参数。**没有排行插件时返回空列表，不是 503** —— 「没装插件」是正常
/// 状态，返回 503 会让前端把「没数据」显示成「服务坏了」。
async fn list_ranking_sources(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<sm_service::discovery::ranking::RankingSourceResource>>, ErrorResponse> {
    let service = RankingCatalogService::new(state.db().clone())
        .with_sources(state.ranking().clone());
    Ok(Json(service.list_sources()))
}

/// `GET /ranking-sources/{source_key}/boards`
///
/// 源不存在 → **404**。源存在但榜单定义无存放处 → **404**
/// `ranking_board_definitions_unavailable`（见模块文档）。
async fn list_ranking_boards(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(source_key): Path<String>,
) -> Result<Json<Vec<sm_service::discovery::ranking::RankingBoardResource>>, ErrorResponse> {
    // 先确认源存在 —— 「源不存在」与「定义没接入」是两个不同的 404。
    state.ranking().require_definition(&source_key)?;
    let service = RankingCatalogService::new(state.db().clone())
        .with_sources(state.ranking().clone());
    service.list_boards(&source_key).await.map(Json)
}

/// `GET /ranking-sources/{source_key}/boards/{board_key}/items`
#[derive(Debug, Default, Deserialize)]
struct BoardItemsQuery {
    /// 不传则用榜单默认周期；传了但不支持 → **422**。
    period: Option<String>,
    /// 非法值**降级**（见模块文档）。
    sort: Option<String>,
    /// 可选：只取前 N 条。**不传就是全量** —— 榜单通常 10~100 条，
    /// 分页只会逼调用方写取完所有页的循环（仓储层注释原话）。
    limit: Option<i64>,
}

/// 榜单条目响应。**刻意没有 `total`** —— 这个端点不分页。
async fn list_ranking_board_items(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((source_key, board_key)): Path<(String, String)>,
    EnvelopeQuery(query): EnvelopeQuery<BoardItemsQuery>,
) -> Result<Json<BoardItemPage>, ErrorResponse> {
    let service = RankingCatalogService::new(state.db().clone())
        .with_sources(state.ranking().clone());

    // 源与榜单都要存在 —— 两个 404 语义不同，不要合并。
    let (_, board) = state.ranking().require_source_and_board(&source_key, &board_key)?;
    // 周期解析：不支持则 422，**不静默回退**。
    let period = RankingCatalogService::resolve_period(board, query.period.as_deref())?;

    service
        .list_board_items(&source_key, &board_key, &period, query.limit)
        .await
        .map(Json)
}
