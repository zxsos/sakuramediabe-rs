//! 排行榜目录与同步（上游 `ranking_service.py`，21.7KB / 519 行）。
//!
//! **纯 PostgreSQL + 插件注册表**，无 Qdrant、无推理服务。
//!
//! # 读侧现在就能用；写侧要等真有插件
//!
//! | | 依赖 | 现状 |
//! |---|---|---|
//! | `list_movie_rankings` | [`RankingItemRepository::list_by_movie`] | ✅ **可写** |
//! | `list_board_items` | [`RankingItemRepository::list_by_board`] / `top_by_board` | ✅ **可写** |
//! | `list_sources` | `ProviderRegistry::providers_with(EXTENSION_RANKING_SOURCE)` | ✅ **可写**（无插件时返回空列表）|
//! | `list_boards` | **榜单定义的存放处** | ❌ **缺口**，见下 |
//! | `RankingSyncService::*` | `extension_calls::fetch_ranking` + 插件 channel | ❌ 要等真有 provider 插件 |
//!
//! ## `list_boards` 的缺口是真的，不是「还没写」
//!
//! 榜单定义（`title` / `supported_periods` / `default_period` / `descending`）
//! **在 Rust 侧无处存放**。已确认：
//!
//! - `sm_db::discovery::RankingItem` 只有条目（`source_key` / `board_key` /
//!   `period` / `rank` / `movie_number` / `movie_id`），没有定义字段
//! - `sm_db::repo::RankingItemRepository::list_boards` 返回
//!   `Page<(String, String, String)>` —— 是 `(board_key, period, source_key)`
//!   **元组**，不是带标题的定义
//! - `sm_plugins::registry::ProviderRegistration` 只有 `provider_key` /
//!   `display_name` / `plugin_id` / `capabilities` / `data_plane_endpoint`
//!   —— **没有 boards 列表**
//!
//! 上游的定义来自插件注册时的 `register_plugin_ranking_sources(accepted, owners)`
//! （`ranking_plugin_adapter.py:109`），也就是**加载期**从插件的注册载荷里取。
//! Rust 侧的 `sm-plugins` 还没接住那份载荷。
//!
//! **所以 `list_boards` 不能靠猜字段填。** 要么给 `ProviderRegistration` 加
//! 榜单定义（改插件 ABI 层），要么新增一张榜单定义表。**两者都不是路由层能
//! 决定的**，所以这里留 `todo!()` 并把缺口写清 —— 写一个「看起来能跑」的
//! 版本只会让 `supported_periods` 永远是空数组。
//!
//! # `period` 用**空串**表示「不限定周期」，不是 `Option::None`
//!
//! 仓储层签名是 `period: &str`，而 `NewRankingItem::period` 的文档写明
//! 「**空串**代表不限定周期（如总榜），不是 `None`」。
//!
//! 所以路由层传来的 `Option<&str>` 要在这里翻译：���` → `""`。
//! **传 `None` 会被 `period.trim()` 绑成空串之外的语义** —— 而
//! `list_by_board` 的 `WHERE period = $3` 是**精确匹配**，`""` 与任何非空
//! 周期都不相等。
//!
//! # 不分页 vs 分页：**两处刻意不同**
//!
//! | 方法 | 上游 | 仓储 | 为什么 |
//! |---|---|---|---|
//! | `list_board_items` | **不分页** | `list_by_board` 不分页 | 榜单通常 10~100 条，分页只会逼调用方写取完所有页的循环（仓储层注释原话）|
//! | `list_movie_rankings` | **不分页** | `list_by_movie` **分页** | 仓储层分页了，服务层要取完 —— 见该方法注释 |
//!
//! 第二处是**真实的形状冲突**，不是笔误，处理方式写在方法注释里。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sm_db::common::page::PageRequest;
use sm_db::repo::discovery::{NewRankingItem, RankingItemRepository};
// `sm_db` 没有公开导出 `PgPool`（它是 `sqlx::PgPool` 的私有别名），
// 直接引 `sqlx` —— 本 crate 本来就依赖它。
use sqlx::PgPool;

use crate::error::ServiceError;

/// 榜单定义（冻结）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingBoardDefinition {
    pub board_key: String,
    pub title: String,
    /// 该榜单支持哪些周期。**空数组意味着「只有总榜」**，不是「未知」。
    pub supported_periods: Vec<String>,
    /// 未指定周期时用哪个。**必须**是 `supported_periods` 之一。
    pub default_period: String,
    /// 条目是否按分数降序。为 `false` 时按名次升序（名次小在前）。
    pub descending: bool,
}

/// 排行源定义（冻结）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingSourceDefinition {
    pub source_key: String,
    pub title: String,
    pub boards: Vec<RankingBoardDefinition>,
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
    pub movie_number: String,
}

/// 榜单条目分页。
///
/// **刻意没有 `total`** —— 上游这个端点不分页（见模块文档）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardItemPage {
    pub items: Vec<BoardItemResource>,
}

/// 某影片的排名响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovieRankingResource {
    pub source_key: String,
    pub board_key: String,
    pub period: String,
    pub rank: i64,
}
/// 读侧：榜单目录。
pub struct RankingCatalogService {
    pool: PgPool,
    /// 组合根注入的排行源快照。**默认为空** —— 只有组合根会填。
    sources: RankingSourceCatalog,
}

impl RankingCatalogService {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            sources: RankingSourceCatalog::default(),
        }
    }

    /// 挂上排行源快照。**只有组合根会调**（它才能读插件注册表）。
    pub fn with_sources(mut self, sources: RankingSourceCatalog) -> Self {
        self.sources = sources;
        self
    }

    /// 仓储句柄。**每次调用新建一个** —— `RankingItemRepository` 只是
    /// `PgPool` 的薄包装（`new` / `pool` 两个方法），没有内部状态，
    /// 存一个字段反而让它看起来像有缓存。
    fn repo(&self) -> RankingItemRepository {
        RankingItemRepository::new(self.pool.clone())
    }

    /// `None` → `""`（表示「不限定周期」）。
    ///
    /// 见模块文档：`period` 是**精确匹配**的，所以 `None` 与 `""` 在
    /// `WHERE period = $3` 下的行为完全不同。
    pub fn period_or_all(period: Option<&str>) -> &str {
        period.unwrap_or("").trim()
    }

    /// 榜单是否支持某周期。
    pub fn board_supports_period(board: &RankingBoardDefinition, period: &str) -> bool {
        // 空周期 = 总榜，**任何榜单都支持**。
        period.is_empty() || board.supported_periods.iter().any(|p| p == period)
    }

    /// 解析周期：`None` → 榜单默认周期。
    ///
    /// **给了但不支持 → 422**（`ServiceError::validation`），**不静默回退**。
    /// 静默回退会让调用方以为拿到的是周榜，实际是日榜 —— 榜单位次差得很远，
    /// 这个错误在界面上看不出来。
    pub fn resolve_period(
        board: &RankingBoardDefinition,
        period: Option<&str>,
    ) -> Result<String, ServiceError> {
        let requested = Self::period_or_all(period);
        if requested.is_empty() {
            return Ok(board.default_period.clone());
        }
        if Self::board_supports_period(board, requested) {
            return Ok(requested.to_owned());
        }
        Err(ServiceError::validation(
            "ranking_period_unsupported",
            format!(
                "榜单 {} 不支持周期 {}，支持的周期是 {:?}",
                board.board_key, requested, board.supported_periods
            ),
        ))
    }

    /// 某影片在各榜单上的排名。
    ///
    /// 上游 `list_movie_rankings`（`:185-216`）返回**不分页**的列表。
    ///
    /// # 形状冲突：仓储层分页了
    ///
    /// `RankingItemRepository::list_by_movie` 返回 `Page<RankingItem>`，而
    /// 上游要的是「这个影片在所有榜单上的排名」——**全量**。
    ///
    /// 处理：**循环取完所有页**。一部影片的榜单条目数是几十量级（每个源几个
    /// 周期），不会多到需要担心，但**不能只取第一页** —— 那会让「排第 1 的
    /// 三个榜」变成「排第 1 的一个榜」，而且**缺的那几个不会报错**。
    ///
    /// # 页面大小只能是 100，不能更大
    ///
    /// `PageRequest::new` 走 `validate_page`，而 `page_size` **超过 100 会
    /// 返回错误**（`first_page` 文档原话：「`page_size` 超过 100 会失败而不是
    /// 被静默截断」）。
    ///
    /// 所以想「一次取完 200 条」是**做不到的** —— 只能循环。这不是缺陷，校验本身
    /// 是对的：静默截断会让客户端拿到一个它不知道自己没拿全的结果。
    pub async fn list_movie_rankings(
        &self,
        movie_id: i64,
    ) -> Result<Vec<MovieRankingResource>, ServiceError> {
        const PAGE_SIZE: i64 = 100;
        let repo = self.repo();
        let mut page_number = 1i64;
        let mut out = Vec::new();
        loop {
            // 校验失败（页码越界等）直接冒泡 —— 那是调用方的参数问题。
            let request = PageRequest::new(page_number, PAGE_SIZE)?;
            let page = repo.list_by_movie(movie_id as i32, request).await?;
            let got = page.items.len() as i64;
            out.extend(page.items.into_iter().map(|item| MovieRankingResource {
                source_key: item.source_key,
                board_key: item.board_key,
                period: item.period,
                rank: item.rank as i64,
            }));
            // 取满或取空都收手。`total` 与 items 出自**同一快照**
            // （`paged_list!` 走 `in_snapshot_tx`），所以它可信。
            if got == 0 || page_number * PAGE_SIZE >= page.total {
                break;
            }
            page_number += 1;
        }
        Ok(out)
    }

    /// 某榜单某周期的条目。**不分页**（见模块文档）。
    pub async fn list_board_items(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
        limit: Option<i64>,
    ) -> Result<BoardItemPage, ServiceError> {
        let repo = self.repo();
        // 传了 limit 就用 top_by_board（带 LIMIT），否则取全量。
        // 两条路径都走同一个索引 (`source_key, board_key, period`)，排序一致。
        let items = match limit {
            Some(limit) if limit > 0 => {
                repo.top_by_board(source_key, board_key, period, limit as i32)
                    .await?
            }
            _ => repo.list_by_board(source_key, board_key, period).await?,
        };
        Ok(BoardItemPage {
            items: items
                .into_iter()
                .map(|item| BoardItemResource {
                    rank: item.rank as i64,
                    movie_id: item.movie_id as i64,
                    movie_number: item.movie_number,
                })
                .collect(),
        })
    }

    /// 全部排行源。
    ///
    /// 数据来自**组合根注入的快照**（[`RankingSourceCatalog`]），不是数据库
    /// —— 源的身份是插件注册时声明的，库里只有条目。
    ///
    /// **没有插件时返回空列表，不是 503。** 「没装排行插件」是正常状态，不是
    /// 故障 —— 返回 503 会让前端把「没数据」显示成「服务坏了」。
    ///
    /// # 为什么是快照，而不是直接读插件注册表
    ///
    /// `sm-plugins -> sm-scheduler -> sm-service` 已经是一条依赖链 ——
    /// **`sm-service` 再依赖 `sm-plugins` 就成环**。
    ///
    /// 而 `sm-api` 同样不依赖 `sm-plugins`（只有 `sm-core` / `sm-service` /
    /// `sm-db`），所以路由层也拿不到注册表。
    ///
    /// 唯一能读注册表的是组合根 `sm-server`（它依赖全部 crate）。所以走
    /// 「组合根读 -> 快照进 `AppState`」—— 这与 `AppState::jobs`
    /// （`JobCatalog`）是**同一个模式**，不是新发明。
    pub fn list_sources(&self) -> Vec<RankingSourceResource> {
        self.sources.entries()
    }

    /// 某源下的榜单列表。
    ///
    /// # ❌ 缺口：榜单定义在 Rust 侧无处存放
    ///
    /// 见模块文档的详细分析。**能做的只有**从 `ranking_item` 反推
    /// 「有哪些 (board_key, period) 组合」—— 但那给不出 `title` /
    /// `supported_periods` / `default_period`。
    ///
    /// **不写「返回空数组」的假实现** —— 那会让接口「成功」但永远没数据，
    /// 比报缺口更难查。
    pub async fn list_boards(
        &self,
        _source_key: &str,
    ) -> Result<Vec<RankingBoardResource>, ServiceError> {
        Err(ServiceError::not_found_with(
            "ranking_board_definitions_unavailable",
            "榜单定义尚未接入：插件注册载荷里的榜单定义还没有 Rust 侧的存放处",
            [("source_key".to_owned(), serde_json::json!(_source_key))]
                .into_iter()
                .collect(),
        ))
    }
}
/// 写侧：从 provider 拉榜单并覆盖式写回。
///
/// # 现在**完全不可用**，且原因是结构性的
///
/// 上游 `_replace_scope_items`（`:357`）是「先删该 scope 全部条目，再逐条
/// 写入」。这个语义需要：
///
/// 1. `RankingItemRepository::delete_board(source_key, board_key, period)`
/// 2. `RankingItemRepository::upsert_in(...)`（事务内变体）
/// 3. `RankingItemRepository::upsert(...)`
///
/// —— 这三个**仓储方法都已存在**。缺的是第 4 步：**从插件拿数据**。
///
/// `sm_plugins::extension_calls::fetch_ranking`（`:103`）已经存在，签名是
/// `(&mut RankingSourceExtensionServiceClient<Channel>, FetchRankingRequest,
/// Option<Duration>)` —— 但它要一个**已建立的 gRPC channel**，而
/// `ProviderRegistration` 只有 `data_plane_endpoint: Option<String>`
/// （**可能为 `None`**，注释说「只有控制面能力的 provider 不一定有」）。
///
/// # 所以这里不做「半个实现」
///
/// 写一个「先删后写但拿不到数据」的 `sync_board_period` 毫无意义 ——
/// 它只会把「还没接通插件」伪装成「同步过了但榜单是空的」。
/// **清空数据比不同步更糟**：线上榜单会从有内容变成空。
pub struct RankingSyncService {
    _private: (),
}

/// 同步结果计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    /// 删除的旧条目数。
    pub removed: u32,
    /// 写入的新条目数。
    pub written: u32,
    /// 跳过的条目数（番号在库里查不到 `movie_id`）。
    ///
    /// `NewRankingItem::movie_id` 是**必填**的（类型文档：「指向 `Movie.id`。
    /// **必填**」）—— 库里没有这个番号的影片时，条目**写不进去**。
    /// 上游 `_get_movie_detail`（`:349`）就是干这个解析的。
    pub skipped: u32,
}

impl RankingSyncService {
    /// 尚未接通 provider 插件。**恒返回 Err。**
    ///
    /// # 为什么保留这个函数而不是删掉
    ///
    /// 它是 `rankings.rs` 顶层文档那张表里「写侧要等真有插件」那行的**代码
    /// 化身**。删掉的话后来人只会看到读侧齐全，以为这功能完整。
    /// 留着并让它明确失败，比留一个静默成功的空实现安全。
    pub async fn sync_board_period(
        &self,
        _source_key: &str,
        _board_key: &str,
        _period: &str,
    ) -> Result<SyncOutcome, ServiceError> {
        Err(ServiceError::unavailable(
            "ranking_sync_unavailable",
            "排行同步尚不可用：还没有任何 provider 插件提供排行源，\
             且 provider 可能没有 data_plane_endpoint",
        ))
    }

    /// 尚未接通 provider 插件。**恒返回 Err**（理由同 [`Self::sync_board_period`]）。
    pub async fn sync_all_rankings(&self) -> Result<HashMap<String, SyncOutcome>, ServiceError> {
        Err(ServiceError::unavailable(
            "ranking_sync_unavailable",
            "排行同步尚不可用：还没有任何 provider 插件提供排行源",
        ))
    }

    /// 把插件返回的一条目映射成待写入的 `NewRankingItem`。
    ///
    /// # `movie_id` 是必填的，所以这一层**必然可能失败**
    ///
    /// 插件给的是 `movie_number`（它只认番号），而 `NewRankingItem::movie_id`
    /// 必填 —— 中间必须有一次「番号 -> 影片」解析（上游 `_get_movie_detail`，
    /// `:349`）。查不到就**跳过这一条**并计入 `skipped`，
    /// **不要**用 0 或 -1 之类占位 —— 那会让条目指向不存在的影片。
    pub fn to_new_item(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
        rank: i32,
        movie_number: &str,
        movie_id: Option<i32>,
    ) -> Result<NewRankingItem, ServiceError> {
        let Some(movie_id) = movie_id else {
            return Err(ServiceError::validation(
                "ranking_item_movie_unresolved",
                format!("番号 {movie_number} 在库里没有对应影片，无法写入榜单条目"),
            ));
        };
        Ok(NewRankingItem {
            source_key: source_key.to_owned(),
            board_key: board_key.to_owned(),
            period: period.to_owned(),
            rank,
            movie_number: movie_number.to_owned(),
            movie_id,
        })
    }
}
/// 排行源目录 —— **组合根注入的快照**。
///
/// # 它为什么在 sm-service 而不在 sm-plugins
///
/// 因为 `sm-plugins` 依赖 `sm-scheduler`，而 `sm-scheduler` 依赖
/// `sm-service` —— 所以 `sm-service` **不能**依赖 `sm-plugins`（会成环）。
/// 而 `sm-api` 也不依赖它。
///
/// 组合根 `sm-server` 依赖全部 crate，是唯一能读 `ProviderRegistry` 的地方。
/// 它把读到的结果转成这个类型，塞进 `AppState`。
///
/// **同一个模式**：`AppState::jobs` 里的 `JobCatalog` 就是这么来的
/// （见 `sm-api/src/state.rs` 的注释「插件表由组合根持有，而 API 层不能
/// 反向依赖组合根」）。
///
/// # 缺省为空
///
/// `Default` 是空目录。**没填时 `list_sources()` 返回空列表** —— 与「装了
/// 插件但没配排行源」的表现一致。这是有意的：两者在界面上都该显示
/// 「没有排行源」，而不是一个 503。
#[derive(Debug, Clone, Default)]
pub struct RankingSourceCatalog {
    entries: Vec<RankingSourceDefinition>,
}

impl RankingSourceCatalog {
    /// 由组合根构造。
    pub fn new(entries: Vec<RankingSourceDefinition>) -> Self {
        Self { entries }
    }

    /// 全部源定义。
    pub fn entries(&self) -> Vec<RankingSourceResource> {
        self.entries
            .iter()
            .map(|definition| RankingSourceResource {
                source_key: definition.source_key.clone(),
                title: definition.title.clone(),
            })
            .collect()
    }

    /// 按 key 取源。**没有就是没有** —— 不做「猜一个默认源」。
    pub fn definition(&self, source_key: &str) -> Option<&RankingSourceDefinition> {
        self.entries
            .iter()
            .find(|definition| definition.source_key == source_key)
    }

    /// 源不存在 → 404。这是路由层「源不存在」错误的**唯一**来源。
    pub fn require_definition(
        &self,
        source_key: &str,
    ) -> Result<&RankingSourceDefinition, ServiceError> {
        self.definition(source_key).ok_or_else(|| {
            ServiceError::not_found_with(
                "ranking_source_not_found",
                format!("排行源 {source_key} 不存在"),
                [("source_key".to_owned(), serde_json::json!(source_key))]
                    .into_iter()
                    .collect(),
            )
        })
    }

    /// 源与榜单都存在 → 返回定义。任一不存在 → **404**。
    pub fn require_board(
        &self,
        source_key: &str,
        board_key: &str,
    ) -> Result<&RankingBoardDefinition, ServiceError> {
        let definition = self.require_definition(source_key)?;
        definition
            .boards
            .iter()
            .find(|board| board.board_key == board_key)
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "ranking_board_not_found",
                    format!("排行源 {source_key} 下没有榜单 {board_key}"),
                    [
                        ("source_key".to_owned(), serde_json::json!(source_key)),
                        ("board_key".to_owned(), serde_json::json!(board_key)),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
    }

    /// 源与榜单都存在 → 返回**两者**。`list_board_items` 要同时拿周期规则
    /// 与条目，所以一次返回定义与源。
    pub fn require_source_and_board(
        &self,
        source_key: &str,
        board_key: &str,
    ) -> Result<(&RankingSourceDefinition, &RankingBoardDefinition), ServiceError> {
        let definition = self.require_definition(source_key)?;
        let board = definition
            .boards
            .iter()
            .find(|board| board.board_key == board_key)
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "ranking_board_not_found",
                    format!("排行源 {source_key} 下没有榜单 {board_key}"),
                    [
                        ("source_key".to_owned(), serde_json::json!(source_key)),
                        ("board_key".to_owned(), serde_json::json!(board_key)),
                    ]
                    .into_iter()
                    .collect(),
                )
            })?;
        Ok((definition, board))
    }

    /// 源数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
