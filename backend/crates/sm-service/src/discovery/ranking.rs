//! 排行榜目录与同步（上游 `ranking_service.py`，21.7KB / 519 行）。
//!
//! **纯 PostgreSQL + 插件注册表**，无 Qdrant、无推理服务。
//!
//! # 读侧与写侧都已落地
//!
//! | | 依赖 | 现状 |
//! |---|---|---|
//! | `list_movie_rankings` | [`RankingItemRepository::list_by_movie`] | ✅ 可写 |
//! | `list_board_items` | [`RankingItemRepository::list_by_board`] / `top_by_board` | ✅ 可写 |
//! | `list_sources` | 组合根注入的 [`RankingSourceCatalog`] | ✅ 可写（无插件时返回空列表）|
//! | `list_boards` | 同上（榜单定义跟着源一起来）| ✅ 可写 |
//! | `RankingSyncService::*` | [`RankingGateway`] + `sm-db` 仓储 | ✅ 可写 |
//!
//! ## 榜单定义从哪来：**插件注册载荷**，不是数据库
//!
//! 上游的定义来自插件加载时的
//! `register_plugin_ranking_sources(accepted, owners)`（`ranking_plugin_adapter.py:109`）
//! —— 也就是说「有哪些源、每个源有哪些榜、榜单支持哪些周期」全在**插件的注册
//! 载荷**里，库里只有条目（`ranking_item`）。
//!
//! Rust 侧对应的存放处是 `sm_plugins::extensions::ExtensionRegistry` 里的
//! `RankingSourceRegistration`（含 `boards`），由组合根读出来转成
//! [`RankingSourceCatalog`] 注入。骨架期这条链断了两次：`Plugins::ranking_sources()`
//! 读的是 `ProviderRegistry`（那里**没有** boards）并硬写空数组，而载荷里的
//! `RankingBoard` 又**只有** `board_key` + `display_name`（没有周期字段）。
//! 两处都已补齐。
//!
//! ## 写侧的取数靠**依赖倒置**
//!
//! 插件在另一个进程里，调用要过 gRPC。但 `sm-plugins` 依赖 `sm-scheduler`、
//! `sm-scheduler` 依赖 `sm-service` —— 所以 `sm-service` **不能**依赖
//! `sm-plugins`。取向与 `StorageGateway` / `MovieMetadataImporter` 一致：
//! 这里只定义窄接口 [`RankingGateway`]，实现（连插件的 channel）在组合根。
//!
//! ## ⚠️ 与上游的**已知差异**：番号不在库里时不会导入
//!
//! 上游 `sync_board_period`（`ranking_service.py:405-445`）对「本地没有的番号」
//! 会拉 JavDB 详情并 `import_movie_if_missing` 入库。本仓的 JavDB **详情**接口
//! 还没落地（[`crate::catalog::movie_javdb_backfill::JavdbProvider`] 是个未实现
//! 的 trait），所以那些番号落进上游的**失败分支**：计 `skipped_movies` + warn。
//! 结果就是**库里没有的影片不会因为上了榜而被自动收录** —— 其余语义（复用已有
//! 影片、整榜替换、周期解析）都一致。补上详情接口后把那个 `continue` 换成导入即可。
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

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sm_db::common::page::PageRequest;
use sm_db::repo::discovery::{NewRankingItem, RankingItemRepository};
use sm_db::repo::{commit_or_rollback, Ctx, MovieRepository};
// `sm_db` 没有公开导出 `PgPool`（它是 `sqlx::PgPool` 的私有别名），
// 直接引 `sqlx` —— 本 crate 本来就依赖它。
use sqlx::PgPool;

use crate::error::ServiceError;

/// 榜单定义（冻结）。
///
/// 整份来自插件**加载期**的注册载荷（`RankingBoardRegistration`）—— 库里只有
/// 条目，没有定义。骨架期这里还有一个 `descending`，**上游没有这个字段**，
/// 载荷里也没有它的来源，所以删掉而不是填一个想当然的值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingBoardDefinition {
    pub board_key: String,
    pub title: String,
    /// 该榜单**静态**声明支持的周期。**空数组意味着「只有总榜」**，不是「未知」。
    ///
    /// 动态周期的榜单（见 [`Self::dynamic_periods`]）这里也是空 —— 它的周期随
    /// 年份滚动，注册期给不出来。
    pub supported_periods: Vec<String>,
    /// 未指定周期时用哪个。
    ///
    /// 上游的约束是「必须是 `supported_periods` 之一」；动态周期的榜单不适用
    /// （它给的是代表值，如 TOP250 的 `all`）—— 所以**加载期只校验非空**，
    /// 不做「属于列表」的判定。
    pub default_period: String,
    /// 周期要不要问插件的 `ResolveRankingPeriods` 才知道。
    ///
    /// 上游判据是 `supported_periods_provider is not None`。为 `true` 时
    /// [`RankingCatalogService::board_supports_period`] 会**放行任意非空周期**
    /// —— 否则历史年份（`"2019"`）会被判成不支持，而插件其实抓得动。
    pub dynamic_periods: bool,
}

/// 排行源定义（冻结）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingSourceDefinition {
    pub source_key: String,
    pub title: String,
    pub boards: Vec<RankingBoardDefinition>,
    /// ★ **归属**：声明这个源的那个插件。
    ///
    /// 上游把它单独放在 `RANKING_SOURCE_OWNERS`（`source_key -> plugin_id`，
    /// `ranking_service.py:68`），因为那份注册表是**和插件共享**的类型，插件
    /// 不该看到「谁拥有谁」。本仓的 [`RankingSourceDefinition`] 只在宿主内部
    /// 流通（给 API 的是投影 [`RankingSourceResource`]），所以直接带上更省事
    /// —— 省掉一条要单独传递的表。
    ///
    /// 用途是**授权**，不是展示：插件只能同步自己声明的源
    /// （上游 `context.py:1476-1503`，越界直接 `ValueError`）。
    pub owner_plugin_id: String,
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
    /// **静态**周期。动态周期的榜单这里是空的，见 [`Self::dynamic_periods`]。
    pub supported_periods: Vec<String>,
    pub default_period: String,
    /// 这个榜还有「随年份滚动」的周期，清单要问插件。
    ///
    /// 加这个标志而不是去读路径调插件：插件不在线时榜单目录也要能列出来
    /// （上游是同进程回调，不存在这个失败面）。
    pub dynamic_periods: bool,
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
    ///
    /// 两种放行：空周期（总榜，任何榜单都支持），以及**动态周期的榜单**
    /// （它的周期清单在插件那边，宿主无从校验，只能交给插件裁决）。
    pub fn board_supports_period(board: &RankingBoardDefinition, period: &str) -> bool {
        period.is_empty()
            || board.dynamic_periods
            || board.supported_periods.iter().any(|p| p == period)
    }

    /// 解析周期。**照抄上游 `_resolve_period`**（`ranking_service.py:112-142`）。
    ///
    /// # `default_period` 是给客户端的提示，**不是**服务端的回退值
    ///
    /// 骨架期这里写的是「`None` → 榜单默认周期」，那是**错的**：上游对有周期
    /// 集合的榜单**必须**显式给周期，空周期直接 422 `period is required`。
    /// `default_period` 只出现在响应里（`RankingBoardResource.default_period`），
    /// 服务端一次都没拿它做过回退。
    ///
    /// 四档语义（与上游逐条对应）：
    ///
    /// | 榜单 | 给了周期 | 结果 |
    /// |---|---|---|
    /// | 有周期（静态或动态） | 空 | 422 `invalid_ranking_period`（`period is required`）|
    /// | 有静态周期 | 不在集合里 | 422 `invalid_ranking_period` |
    /// | 有动态周期 | 任意非空 | **放行** —— 清单在插件那边，宿主无从校验 |
    /// | 单期榜（无任何周期） | 空 | `""`（表示「不限定周期」）|
    /// | 单期榜 | 非空 | 422 `invalid_ranking_period` |
    ///
    /// 周期**先 `trim` 再 `to_lowercase`**（上游 `(period or "").strip().lower()`）。
    /// 少了归一化，`"Daily"` 会被判成不支持 —— 而它指的就是 `"daily"`。
    pub fn resolve_period(
        board: &RankingBoardDefinition,
        period: Option<&str>,
    ) -> Result<String, ServiceError> {
        let requested = period.unwrap_or("").trim().to_lowercase();
        // 「有周期集合」= 静态非空**或**动态。两类都不接受空周期。
        let has_periods = board.dynamic_periods || !board.supported_periods.is_empty();
        if has_periods {
            if requested.is_empty() {
                return Err(Self::invalid_period(
                    board,
                    "",
                    "period is required for this board",
                ));
            }
            if Self::board_supports_period(board, &requested) {
                return Ok(requested);
            }
            return Err(Self::invalid_period(
                board,
                &requested,
                "period is not supported",
            ));
        }
        // 单期榜：只接受空周期。
        if !requested.is_empty() {
            return Err(Self::invalid_period(
                board,
                &requested,
                "period is not supported for this board",
            ));
        }
        Ok(String::new())
    }

    /// `invalid_ranking_period` 的构造（上游三种情况**同一个码**）。
    fn invalid_period(
        board: &RankingBoardDefinition,
        requested: &str,
        reason: &str,
    ) -> ServiceError {
        ServiceError::validation(
            "invalid_ranking_period",
            format!(
                "{reason}：榜单 {} 收到周期 {requested:?}，静态支持 {:?}{}",
                board.board_key,
                board.supported_periods,
                if board.dynamic_periods {
                    "（另有随年份滚动的动态周期）"
                } else {
                    ""
                }
            ),
        )
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
    /// 定义来自**组合根注入的快照**（[`RankingSourceCatalog`]），与
    /// [`Self::list_sources`] 同源 —— 库里只有条目，没有定义。
    ///
    /// 源不存在 → **404**（`ranking_source_not_found`），与上游
    /// `_require_source` 一致。源存在但没有榜单 → 空列表（不是错误）。
    ///
    /// # `supported_periods` 只给**静态**那一半
    ///
    /// 动态周期的榜单（TOP250）在快照里是空数组 + `dynamic_periods = true`
    /// —— 精确的那份清单要问插件（`ResolveRankingPeriods`），而**读路径不该
    /// 依赖插件在线**：插件没起来时榜单列表也要能看。所以这里如实返回快照，
    /// 由 [`RankingBoardResource`] 上的标志告诉调用方「这个榜的周期不止这些」。
    pub async fn list_boards(
        &self,
        source_key: &str,
    ) -> Result<Vec<RankingBoardResource>, ServiceError> {
        let definition = self.sources.require_definition(source_key)?;
        Ok(definition
            .boards
            .iter()
            .map(|board| RankingBoardResource {
                board_key: board.board_key.clone(),
                title: board.title.clone(),
                supported_periods: board.supported_periods.clone(),
                default_period: board.default_period.clone(),
                dynamic_periods: board.dynamic_periods,
            })
            .collect())
    }
}
/// 向排行榜插件取数的能力。**窄接口在 sm-service，实现由组合根注入。**
///
/// # 为什么不直接调 `sm_plugins::extension_calls`
///
/// `sm-plugins` 依赖 `sm-scheduler`，而 `sm-scheduler` 依赖 `sm-service`
/// —— `sm-service` 依赖 `sm-plugins` 就成环。取向与
/// `StorageGateway` / `MovieMetadataImporter` 完全一致。
///
/// # ★「空结果」不是失败，两者处理相反
///
/// - 插件回空番号列表 → `Ok(vec![])`：榜单**真的**空了，要照常替换（清空）。
/// - 连不上 / 插件报错 / 超时 → `Err`：**绝不能碰库**，旧榜单留着。
///
/// 把两者混起来（比如把空榜也当错误）会让「榜单下架」永远同步不掉；
/// 反过来（把失败当空榜）会**把线上榜单清空**。
pub trait RankingGateway: Send + Sync {
    /// 拉某个榜某周期的番号，**顺序即排名**（第 1 个就是第 1 名）。
    fn fetch_ranking<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        period: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, RankingCallError>>;

    /// 问插件「这个榜此刻要抓哪些周期」。
    ///
    /// 上游 `should_fetch(period, has_items)`（`ranking_service.py:36`）里
    /// `has_items` 那一半由宿主递（`periods_with_items`），另一半点
    /// （账号配了没、历史年份跳过规则）只有插件知道 —— 所以裁决在插件。
    ///
    /// 返回的周期会被**原样**交给 [`RankingGateway::fetch_ranking`]，
    /// 其中**空串表示总榜**（单期榜的周期就是空串）。宿主不做回退或补全
    /// —— 想要总榜就返回 `[""]`，不想抓就返回 `[]`。
    fn resolve_periods<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        periods_with_items: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<String>, RankingCallError>>;
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 一次插件调用失败（只用于 `Err` 侧）。空结果走 `Ok`，见 [`RankingGateway`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingCallError {
    /// 稳定的机读码（`extension_call_failed` / `extension_call_timeout` /
    /// `ranking_source_not_found` …）。**判断用 `code`，不要匹配 `message`。**
    pub code: &'static str,
    pub message: String,
}

impl RankingCallError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RankingCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// 单榜同步的统计。**键名与上游 `sync_board_period` 返回的 dict 逐字一致**
/// （`ranking_service.py:453-462`）—— 它会直接进 `SyncRankingBoardResponse`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BoardSyncStats {
    pub source_key: String,
    pub board_key: String,
    /// **规整后**的周期（`trim` + 小写，见
    /// [`RankingCatalogService::resolve_period`]）。
    pub period: String,
    /// 插件回的番号数。**不是**写入的条目数（两者在跳过时不等）。
    pub fetched_numbers: i64,
    /// 番号不在库里、由本次同步**新建**的影片数。
    ///
    /// ⚠️ **本仓恒为 0**：上游这里拉 JavDB 详情导入（`:408-424`），而宿主的
    /// JavDB 详情接口还没落地，那些番号走了上游的**失败分支**（见模块文档）。
    pub imported_movies: i64,
    /// 番号在库里已有影片、直接复用的条数。
    pub local_hit_movies: i64,
    /// 番号既不在库里、也导入不了的条数。
    pub skipped_movies: i64,
    /// **实际写入**的条目数（替换后该 scope 的行数）。
    pub stored_items: i64,
}

/// 全量同步的统计。**键名与上游 `sync_all_rankings` 返回的 dict 逐字一致**
/// （`ranking_service.py:507-516`）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AllRankingsStats {
    /// 本次收敛出的目标总数（一个目标 = 一个 榜单 × 周期）。
    pub total_targets: i64,
    /// 成功同步的目标数（对应 proto 的 `synced_count`）。
    pub success_targets: i64,
    /// 失败的目标数 —— 单个目标失败**不中断**整批。
    pub failed_targets: i64,
    pub fetched_numbers: i64,
    pub imported_movies: i64,
    pub local_hit_movies: i64,
    pub skipped_movies: i64,
    pub stored_items: i64,
}

/// 写侧：从插件拉榜单并**覆盖式**写回。
///
/// 上游 `ranking_service.py:322-565`。
///
/// # 依赖是注入的
///
/// `sources` 是组合根给的快照（榜单定义在插件注册载荷里），`gateway` 是组合根
/// 给的取数实现（连插件的 channel）。**网关可缺** —— 没注入时同步明确失败
/// （`ranking_gateway_missing`），而不是静默返回「同步成功、0 条」。
pub struct RankingSyncService {
    pool: PgPool,
    movies: MovieRepository,
    rankings: RankingItemRepository,
    sources: RankingSourceCatalog,
    gateway: Option<Arc<dyn RankingGateway>>,
}

impl RankingSyncService {
    /// 构造。`gateway` 稍后用 [`Self::with_gateway`] 注入。
    pub fn new(pool: PgPool, sources: RankingSourceCatalog) -> Self {
        Self {
            movies: MovieRepository::new(pool.clone()),
            rankings: RankingItemRepository::new(pool.clone()),
            pool,
            sources,
            gateway: None,
        }
    }

    /// 注入取数能力。组合根在插件注册表就绪后调用。
    pub fn with_gateway(mut self, gateway: Arc<dyn RankingGateway>) -> Self {
        self.gateway = Some(gateway);
        self
    }

    /// 注入过的源快照（只读）。
    pub fn sources(&self) -> &RankingSourceCatalog {
        &self.sources
    }

    fn gateway(&self) -> Result<&Arc<dyn RankingGateway>, ServiceError> {
        self.gateway.as_ref().ok_or_else(|| {
            ServiceError::unavailable(
                "ranking_gateway_missing",
                "排行同步不可用：宿主的插件取数能力还没注入",
            )
        })
    }

    /// ★ 同步一个「榜单 × 周期」。照抄上游 `sync_board_period`
    /// （`ranking_service.py:375-462`）。
    ///
    /// # 顺序不能换：先取数，**最后**才在一个事务里替换
    ///
    /// 取数失败（插件没起 / 超时）时绝不能碰库 —— 旧榜单要留着。上游也是这个
    /// 次序：`_get_rank_numbers` 在最前面，`_replace_scope_items` 在最后且自带
    /// `atomic()`。
    ///
    /// # 空榜会**清空**这个 scope
    ///
    /// 插件回 `[]` 是「这个榜此刻没有条目」，是**成功**：删空旧条目、一行不写
    /// （上游 `:370-371` 的 `if not rows: return 0`）。把空榜当错误，会让
    /// 「榜单下架」永远同步不掉。
    pub async fn sync_board_period(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
    ) -> Result<BoardSyncStats, ServiceError> {
        let (_, board) = self
            .sources
            .require_source_and_board(source_key, board_key)?;
        // 周期校验与读侧共用（上游 `_resolve_period` 也是同一个函数）——
        // 于是「同步能跑但接口查不到」不会发生。
        let period = RankingCatalogService::resolve_period(board, Some(period))?;
        let gateway = self.gateway()?;
        let numbers = gateway
            .fetch_ranking(source_key, board_key, &period)
            .await
            .map_err(|error| ServiceError::unavailable(error.code, error.message))?;

        // 批量查本地影片（上游 `:389-399`）：榜单上大多数番号都已入库，命中就
        // 直接复用 `Movie.id`，避免逐部去拉详情。
        let existing = self.movies.find_by_numbers(&numbers).await?;

        let mut local_hit_movies: i64 = 0;
        let mut skipped_movies: i64 = 0;
        let mut items: Vec<NewRankingItem> = Vec::with_capacity(numbers.len());
        for (index, number) in numbers.iter().enumerate() {
            // 上游 `enumerate(movie_numbers, start=1)` —— 名次从 1 开始。
            let rank = index as i32 + 1;
            let Some(movie) = existing.get(number) else {
                // ⚠️ 上游在这里拉 JavDB 详情导入（`:408-424`）。本仓没有详情接口，
                // 于是落进**上游的失败分支**：计 skipped + warn。补上详情接口后
                // 把这里换成 import_movie_if_missing。
                skipped_movies += 1;
                tracing::warn!(
                    source_key,
                    board_key,
                    period,
                    rank,
                    movie_number = %number,
                    "榜单条目跳过：番号不在库里，且 JavDB 详情导入尚未接通"
                );
                continue;
            };
            local_hit_movies += 1;
            items.push(NewRankingItem {
                source_key: source_key.to_owned(),
                board_key: board_key.to_owned(),
                period: period.clone(),
                rank,
                movie_number: number.clone(),
                movie_id: movie.id,
            });
        }

        let stored_items = self
            .replace_scope(source_key, board_key, &period, &items)
            .await?;
        Ok(BoardSyncStats {
            source_key: source_key.to_owned(),
            board_key: board_key.to_owned(),
            period,
            fetched_numbers: numbers.len() as i64,
            imported_movies: 0,
            local_hit_movies,
            skipped_movies,
            stored_items,
        })
    }

    /// ★ 替换一个 scope 的全部条目：**先删后插，同一个事务**（上游
    /// `_replace_scope_items`，`:357-373`）。
    ///
    /// # 为什么不是逐条 `upsert`
    ///
    /// `upsert` 按 `(source_key, board_key, period, rank)` 幂等，**删不掉
    /// 「这次没有的名次」**：榜单从 100 条缩到 80 条时，第 81..100 名会永远
    /// 留在库里，于是推荐打分读到一批幽灵条目。上游用替换正是为了这个。
    ///
    /// 两半必须在**同一个事务**里：中间失败会留下一个空 scope（先删成功了），
    /// 那正是「清空数据比不同步更糟」。
    async fn replace_scope(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
        items: &[NewRankingItem],
    ) -> Result<i64, ServiceError> {
        let mut tx = self.pool.begin().await?;
        let outcome = async {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            self.rankings
                .delete_board_in(&mut ctx, source_key, board_key, period)
                .await?;
            for item in items {
                self.rankings.upsert_in(&mut ctx, item).await?;
            }
            Ok::<i64, ServiceError>(items.len() as i64)
        }
        .await;
        commit_or_rollback(tx, outcome).await
    }

    /// ★ 同步**指定的**排行源（`None` = 全部）。照抄上游 `sync_all_rankings`
    /// （`ranking_service.py:501-565`）。
    ///
    /// # 为什么要 `source_keys` 过滤
    ///
    /// 插件只能同步**自己**声明的源：上游 `PluginContext.sync_ranking_sources`
    /// （`context.py:1476-1486`）先用 `RANKING_SOURCE_OWNERS` 过滤出归属自己的
    /// `source_keys` 再传进来。归属判断需要注册表，只有组合根有 —— 所以过滤在
    /// 调用方做，这里只按 key 收窄。
    ///
    /// # 两处与上游的形状差异（都是刻意的）
    ///
    /// **① 目标收敛挪到插件里**。上游 `_iter_sync_targets` 自己枚举
    /// `board_supported_periods(board)` 再按 `should_fetch` 过滤；本仓把
    /// 「哪些周期要抓」整段交给插件的 `ResolveRankingPeriods`（静态周期、
    /// 动态年份、账号未配，一次算完）。
    ///
    /// **② 收敛阶段失败 → 整批失败**（不吞）。上游那一步是纯本地计算，不可能
    /// 失败；这里是一次 rpc。吞掉它会让「插件挂了」表现成「同步成功、0 个目标」
    /// —— 那是最难查的一种故障。**单个目标同步失败仍然不中断整批**（照抄上游
    /// 的 per-target `try/except`）。
    pub async fn sync_all_rankings(
        &self,
        source_keys: Option<&[String]>,
    ) -> Result<AllRankingsStats, ServiceError> {
        let gateway = self.gateway()?;

        let mut targets: Vec<(String, String, String)> = Vec::new();
        for source in self.sources.definitions() {
            if let Some(only) = source_keys {
                if !only.iter().any(|key| key == &source.source_key) {
                    continue;
                }
            }
            for board in &source.boards {
                // 宿主知道的「这个榜这些周期已经有条目了」—— 递给插件，由它
                // 按自己的规则（账号配置 / 历史年份）裁掉一部分。
                let with_items = self
                    .rankings
                    .distinct_periods(&source.source_key, &board.board_key)
                    .await?;
                let periods = gateway
                    .resolve_periods(&source.source_key, &board.board_key, &with_items)
                    .await
                    .map_err(|error| ServiceError::unavailable(error.code, error.message))?;
                for period in periods {
                    targets.push((source.source_key.clone(), board.board_key.clone(), period));
                }
            }
        }

        let mut stats = AllRankingsStats {
            total_targets: targets.len() as i64,
            ..Default::default()
        };
        // 上游在开始处 emit 一次进度（`:519-525`）。这里没有进度通道
        // （`SyncRankingSourcesRequest` 是空的），改成一条 info ——「这次算出
        // 多少个目标」是运维判断「跑了个空转」的唯一线索。
        tracing::info!(total_targets = stats.total_targets, "排行榜同步开始");

        for (source_key, board_key, period) in &targets {
            match self.sync_board_period(source_key, board_key, period).await {
                Ok(board) => {
                    stats.success_targets += 1;
                    stats.fetched_numbers += board.fetched_numbers;
                    stats.imported_movies += board.imported_movies;
                    stats.local_hit_movies += board.local_hit_movies;
                    stats.skipped_movies += board.skipped_movies;
                    stats.stored_items += board.stored_items;
                }
                Err(error) => {
                    stats.failed_targets += 1;
                    // 上游 `logger.warning`（`:535-541`）。一个榜挂了（JavDB 抽风
                    // 之类）不该让后面 20 个榜都不跑，但必须留痕 —— 否则「同步
                    // 跑过了」会掩盖「有几个榜没更新」。
                    tracing::warn!(
                        source_key,
                        board_key,
                        period,
                        // `?` 是 `Debug`：`ServiceError` 没实现 `Display`。
                        detail = ?error,
                        "排行榜同步目标失败"
                    );
                }
            }
        }
        Ok(stats)
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

    /// 全部源定义（**完整**那份，不是给 API 的投影）。
    ///
    /// 排行同步要按它遍历 `source → boards` 收敛目标，所以需要带 `boards`
    /// 与周期字段的定义；[`Self::entries`] 是同一个列表的**响应投影**，
    /// 没有 `boards`。
    pub fn definitions(&self) -> &[RankingSourceDefinition] {
        &self.entries
    }

    /// 某个插件**声明**的源 key。插件的同步入口据此收窄范围
    /// （上游 `PluginContext.sync_ranking_sources`，`context.py:1476-1480`）。
    pub fn source_keys_owned_by(&self, plugin_id: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|definition| definition.owner_plugin_id == plugin_id)
            .map(|definition| definition.source_key.clone())
            .collect()
    }

    /// 全部源定义（响应投影：只有 key 与标题）。
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
