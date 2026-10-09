//! 排行榜目录与同步（上游 `ranking_service.py`，21.7KB / 519 行）。
//!
//! **纯 PostgreSQL** —— `sm-db/src/discovery/rankings.rs`（13.4KB）已就位。
//! 这是 discovery 域里最大的一块，也是**零外部依赖**的那块。
//!
//! # 两层结构
//!
//! | 上游 | 职责 |
//! |---|---|
//! | `RankingCatalogService` | **读**。榜单定义、榜单条目、某影片的排名 |
//! | `RankingSyncService` | **写**。从 provider 拉榜单、覆盖式写回 DB |
//!
//! 分开的理由不是「读写分离」这种套话：**目录的定义（哪些榜单、什么周期）
//! 来自插件注册，而数据来自 provider**。写路径依赖 provider 插件，读路径不依赖 ——
//! 所以插件没接上时**读侧仍应可用**，不能因为写路径缺 provider 就 503。
//!
//! # 未实现的部分
//!
//! 本文件是**骨架**：`RankingSyncService` 整体依赖 provider 插件（未移植），
//! 因此它的方法体全部是 `todo!()`。`RankingCatalogService` 的方法体同样留空，
//! 但**签名与错误语义已按上游定好**，可直接照上游实现。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 榜单定义（冻结）。
///
/// 字段照抄上游 `:30-41`。`supported_periods` 是**允许的周期全集**，
/// `default_period` 是缺省选择 —— 两者分开是因为有些榜单只有一种周期。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingBoardDefinition {
    /// 榜单键，如 `javbus` 源的某个榜单。
    pub board_key: String,
    /// 展示名。
    pub title: String,
    /// 该榜单支持哪些周期。
    pub supported_periods: Vec<String>,
    /// 未指定周期时用哪个。**必须**是 `supported_periods` 之一。
    pub default_period: String,
    /// 条目是否按分数降序。
    pub descending: bool,
}

/// 排行源定义（冻结）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingSourceDefinition {
    /// 源键。
    pub source_key: String,
    /// 展示名。
    pub title: String,
    /// 该源提供哪些榜单。
    pub boards: Vec<RankingBoardDefinition>,
}

/// 读侧：榜单目录。
pub struct RankingCatalogService;

impl RankingCatalogService {
    /// 按 key 取榜单定义；没有返回 `None`。
    pub fn board_by_key(board: &RankingSourceDefinition, board_key: &str) -> Option<&RankingBoardDefinition> {
        todo!("骨架：照上游 `:51-56` 实现")
    }

    /// 榜单支持的周期（元组语义，顺序即优先级）。
    pub fn board_supported_periods(board: &RankingBoardDefinition) -> (&[String], &str) {
        todo!("骨架：照上游 `:58-68` 实现")
    }

    /// 源不存在 → 404。上游 `_require_source`（`:88`）。
    fn require_source(source_key: &str) -> Result<RankingSourceDefinition, ServiceError> {
        todo!("骨架：照上游 `:88-98` 实现")
    }

    /// 源或榜单不存在 → 404。上游 `_require_board`（`:100`）。
    fn require_board<'a>(
        source_key: &str,
        board_key: &str,
    ) -> Result<(&'a RankingSourceDefinition, &'a RankingBoardDefinition), ServiceError> {
        todo!("骨架：照上游 `:100-111` 实现")
    }

    /// 解析周期：`None` → 榜单默认周期；给了但不支持 → 422。
    ///
    /// 上游 `_resolve_period`（`:113`）。**不支持的周期要报错而不是回退到默认** ——
    /// 静默回退会让调用方以为拿到的是周榜。
    fn resolve_period(
        board: &RankingBoardDefinition,
        period: Option<&str>,
    ) -> Result<&str, ServiceError> {
        todo!("骨架：照上游 `:113-143` 实现")
    }

    /// 某影片在各榜单上的排名。上游 `list_movie_rankings`（`:185`）。
    pub fn list_movie_rankings(movie_id: i64) -> Result<Vec<MovieRankingResource>, ServiceError> {
        todo!("骨架：照上游 `:185-216` 实现（走 sm_db::discovery::rankings）")
    }

    /// 全部排行源。上游 `list_sources`（`:218`）。
    pub fn list_sources() -> Result<Vec<RankingSourceResource>, ServiceError> {
        todo!("骨架：照上游 `:218-227` 实现")
    }

    /// 某源下的榜单列表。上游 `list_boards`（`:229`）。
    pub fn list_boards(source_key: &str) -> Result<Vec<RankingBoardResource>, ServiceError> {
        todo!("骨架：照上游 `:229-241` 实现")
    }

    /// 某榜单某周期的条目（分页）。上游 `list_board_items`（`:243`）。
    pub fn list_board_items(
        source_key: &str,
        board_key: &str,
        period: Option<&str>,
        page_size: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<BoardItemPage, ServiceError> {
        todo!("骨架：照上游 `:243-320` 实现")
    }
}

/// 写侧：从 provider 拉榜单并覆盖式写回。
///
/// **整体依赖 provider 插件**（上游未移植的 `provider_protocol` 那一层），
/// 所以所有方法体都是 `todo!()`。但它**对外仍然可用** —— 插件接上后填实现即可，
/// 签名不必再改。
pub struct RankingSyncService {
    _private: (),
}

impl RankingSyncService {
    /// 同步单个榜单某周期。
    ///
    /// 上游 `sync_board_period`（`:375-463`，88 行）。**覆盖式**替换：
    /// 先 `_replace_scope_items`（`:357`）删掉该 scope 全部条目再写入。
    pub async fn sync_board_period(
        &self,
        source_key: &str,
        board_key: &str,
        period: Option<&str>,
    ) -> Result<SyncOutcome, ServiceError> {
        todo!("骨架：依赖 provider 插件，移植后实现（上游 `:375-463`）")
    }

    /// 同步全部榜单。上游 `sync_all_rankings`（`:501`）。
    pub async fn sync_all_rankings(&self) -> Result<HashMap<String, SyncOutcome>, ServiceError> {
        todo!("骨架：依赖 provider 插件，移植后实现（上游 `:501+`）")
    }
}

/// 同步结果计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    /// 写入条目数。
    pub written: u32,
    /// 删除条目数。
    pub removed: u32,
}

/// 排行源响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingSourceResource {
    pub source_key: String,
    pub title: String,
}

/// 榜单响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingBoardResource {
    pub board_key: String,
    pub title: String,
    pub supported_periods: Vec<String>,
    pub default_period: String,
}

/// 单个榜单条目响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardItemResource {
    pub rank: i64,
    pub movie_id: i64,
    pub score: Option<f64>,
    pub title: Option<String>,
}

/// 某影片的排名响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovieRankingResource {
    pub source_key: String,
    pub board_key: String,
    pub period: String,
    pub rank: i64,
    pub score: Option<f64>,
}

/// 榜单条目分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardItemPage {
    pub items: Vec<BoardItemResource>,
    /// 游标；`None` = 没有下一页。
    pub next_cursor: Option<String>,
}