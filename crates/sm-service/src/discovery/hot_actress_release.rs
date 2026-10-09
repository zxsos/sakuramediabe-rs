//! 热播女优新作（上游 `hot_actress_release_service.py`，9.3KB / 239 行）。
//!
//! **纯 PostgreSQL**，无 Qdrant、无推理服务、无插件。
//!
//! # 语义：按女优**已成熟作品**的表现发现新片，不改影片自身热度
//!
//! 上游类文档原话：「按女性演员已成熟作品表现发现新片，不改变影片自身热度
//! 语义」。所以 `recommendation_score` 与 `movie.heat` 是**两个独立的东西**，
//! 不要拿热度当推荐分。
//!
//! # 两个窗口是**故意错开**的
//!
//! ```text
//!   历史证据窗口  [today-180, today-60)   <- 只用「成熟」作品
//!   候选窗口      [today-90,  today+90)  <- 新片（可含未来）
//! ```
//!
//! **它们在 `[today-90, today-60)` 这 30 天里重叠。** 所以一部候选影片
//! **可能同时**出现在历史证据里 —— 这不是 bug，是设计。后果是打分时必须
//! **把影片自己的证据从女优总分里扣掉**，见
//! [`HotActressReleaseService::score_movies`]。
//!
//! # 三处最容易照抄错的地方
//!
//! **1. 「恰好一位女优」是 `SUM(...) == 1`，不是「至少一位」。**
//! 有 2 位女优的影片**不进**历史证据。这决定了一位女优的作品数统计什么。
//!
//! **2. `age_days` 有下界 `HISTORY_MATURITY_DAYS = 60`。**
//! `max((today - released).days, 60)` —— 而历史窗口本身已经排除了最近 60 天，
//! 所以这个下界**在当前数据下几乎不会生效**。仍然照抄：它是防御性的，且未来
//! 窗口参数一改就起作用。
//!
//! **3. 排序有两级，方向不同。**
//! - 同影片多女优时取「得分高者」，**平局取 `actor_id` 小者**
//!   （上游比的是 `(score, -actor_id)`，取大者 ⇒ `-actor_id` 大 ⇒ `actor_id` 小）
//! - 影片之间按 `(score, release_date, movie_id)` **全降序**
//!   —— 所以同分时**更新的片在前**，同分同日时 **`movie_id` 大的在前**
//!
//! 第 3 条的后半段很容易写反。

use std::collections::HashMap;

use sm_db::repo::discovery::{CandidateRow, HistoryActorRow, HotActressReleaseRepository};
use sm_db::Db;

use crate::catalog::actor::{ActorService, ActorView};
use crate::catalog::movie::{MovieCard, MovieService};
use crate::error::ServiceError;

/// 女性性别值。对应上游 `FEMALE_GENDER = 1`。
pub const FEMALE_GENDER: i32 = 1;

/// 候选窗口：过去天数。
pub const CANDIDATE_PAST_DAYS: i64 = 90;
/// 候选窗口：未来天数。**候选可以是未发行的片**（预售 / 即将上线）。
pub const CANDIDATE_FUTURE_DAYS: i64 = 90;
/// 历史窗口：往前看多少天。
pub const HISTORY_LOOKBACK_DAYS: i64 = 180;
/// 成熟期：最近多少天发行的**不进**历史证据。
pub const HISTORY_MATURITY_DAYS: i64 = 60;
/// 女优至少要有这么部历史作品才被推荐。
pub const MIN_HISTORICAL_MOVIES: i64 = 3;

/// 一位女优的历史证据。
#[derive(Debug, Clone, Default)]
pub struct ActorEvidence {
    /// 所有历史作品的证据之和。
    pub total: f64,
    /// 历史作品数。
    pub movie_count: i64,
    /// 每部作品各自的证据。下标是 `movie_id`。
    pub by_movie_id: HashMap<i32, f64>,
}

/// 打分后的候选影片。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMovie {
    pub movie_id: i64,
    pub release_date: chrono::NaiveDate,
    /// 胜出的那位女优。
    pub actor_id: i64,
    /// **扣掉影片自己之后**的历史作品数。
    pub historical_movie_count: i64,
    pub score: f64,
}

/// 单条结果的**服务形态**：影片卡片 + 胜出的女优 + 两个打分数。
///
/// # 这替换了骨架期那三个带 `serde` 的结构
///
/// 骨架期把线格式（`movie_id` / `title` / `hot_actress.profile_image: i64`）
/// 直接定义在服务层，理由是「卡片 DTO 在 API 层，要签名」。方向对，落点错了：
/// `MovieCard` / `ActorView` **本来就是服务层类型**（`catalog::movie` /
/// `catalog::actor`），签名在 API 层做。所以这里持有它们，路由照着
/// `daily_recommendation` 的同一套转线格式。
#[derive(Debug, Clone)]
pub struct HotActressReleaseItem {
    /// 影片卡片（[`MovieService::load_cards`] 的产物）。
    pub card: MovieCard,
    /// 胜出的那位女优。生效头像（覆盖优先）在 `image_id` / `image_origin` 上。
    pub actress: ActorView,
    /// 该女优的历史作品数，**已扣掉影片自己**（见
    /// [`HotActressReleaseService::score_movies`]）。
    pub historical_movie_count: i64,
    /// 原始分。写到线格式时 `round(.., 4)`（上游在服务层就 round 了；本仓
    /// 把格式化统一留在 DTO 层，与 `daily_recommendation` 一致）。
    pub score: f64,
}

/// 分页结果。
///
/// ⚠️ `items.len()` 可以**小于** `page_size`（也小于 `total` 与剩余条数）：
/// 黑名单影片与缺失女优在装配时被跳过且**不补位**（上游 `:191-195` 的
/// `continue`）。
#[derive(Debug, Clone)]
pub struct HotActressReleasePage {
    pub items: Vec<HotActressReleaseItem>,
    pub page: i64,
    pub page_size: i64,
    /// **候选影片总数**（打分后的长度），不是本页条数、也不是数据库行数。
    pub total: i64,
}
/// 四个窗口边界。
///
/// **历史窗口的右端是 `today - 60` 而不是 `today`** —— 成熟期（见
/// [`HotActressReleaseService::windows`]）。
///
/// ⚠️ 定义在 `impl` **外面**：Rust 不允许在 `impl` 块里定义 `struct`
/// （E0408）。它逻辑上属于本服务，但语法上必须提到模块级。
#[derive(Debug, Clone, Copy)]
pub struct Windows {
    pub history_start: chrono::NaiveDate,
    pub history_end: chrono::NaiveDate,
    pub candidate_start: chrono::NaiveDate,
    pub candidate_end: chrono::NaiveDate,
}

/// 读侧服务。
pub struct HotActressReleaseService;

impl HotActressReleaseService {
    /// 按基准日算四个窗口边界。
    pub fn windows(today: chrono::NaiveDate) -> Windows {
        Windows {
            history_start: today - chrono::Duration::days(HISTORY_LOOKBACK_DAYS),
            // 右开区间：不含 `today - 60` 当天
            history_end: today - chrono::Duration::days(HISTORY_MATURITY_DAYS),
            candidate_start: today - chrono::Duration::days(CANDIDATE_PAST_DAYS),
            candidate_end: today + chrono::Duration::days(CANDIDATE_FUTURE_DAYS),
        }
    }

    /// 归一化发行日。
    ///
    /// 仓储层已经把列读成 `NaiveDate`（两条查询都 `CAST(m.release_date AS date)`，
    /// 见 `repo::discovery::HistoryActorRow` 的说明），所以这里**原样返回**。
    /// 上游 `_release_date`（`:119-121`）要处理 `date | datetime` 两种类型，
    /// 那是 Python 的动态类型问题 —— **Rust 侧在读取时就定型了**，所以这个
    /// 函数只剩文档价值。
    ///
    /// ⚠️ 那句「在读取时就定型了」曾是**空话**：两条查询当时既没 CAST、列又是
    /// `timestamp`，于是解码阶段就报 `DATE vs TIMESTAMP`。保持这句成立的是
    /// SQL 里的 CAST，不是这个函数。
    ///
    /// **不要**在这里做时区转换：库里存的是日期不是时刻，转时区会让「同一天」
    /// 变成「不同天」。
    pub fn release_date(value: chrono::NaiveDate) -> chrono::NaiveDate {
        value
    }

    /// 单部作品对女优的证据：`log1p(heat / age_days)`。
    ///
    /// - `heat` 为 `None` 时按 **0** 算（上游 `float(heat or 0)`）—— 不是跳过，
    ///   是算作零证据。跳过会让「没热度但有作品」的女优凭空消失。
    /// - `age_days` **下界 60**（见模块文档第 2 条）。
    pub fn evidence(heat: Option<i32>, age_days: i64) -> f64 {
        let heat = heat.unwrap_or(0) as f64;
        (1.0 + heat / age_days.max(HISTORY_MATURITY_DAYS) as f64).ln()
    }

    /// 汇总历史证据。**纯函数** —— 不碰数据库，便于直接测。
    pub fn build_evidence(
        rows: &[HistoryActorRow],
        today: chrono::NaiveDate,
    ) -> HashMap<i32, ActorEvidence> {
        let mut out: HashMap<i32, ActorEvidence> = HashMap::new();
        for row in rows {
            // 解构取名而不是 `.0`/`.1` —— 前两位同类型，位置写错编译不报。
            // 位置含义见 `sm_db::repo::discovery::HistoryActorRow`。
            let (movie_id, actor_id, heat, release_date) = *row;
            let released = Self::release_date(release_date);
            // 上游 `max((today - released_on).days, MATURITY_DAYS)`。
            // 这里**不取绝对值** —— 未来发行的片子会得到负数，被下界抬到 60。
            // 照抄上游：它也没取绝对值。
            let age_days = (today - released).num_days();
            let evidence = Self::evidence(heat, age_days);
            let entry = out.entry(actor_id).or_default();
            entry.total += evidence;
            entry.movie_count += 1;
            entry.by_movie_id.insert(movie_id, evidence);
        }
        out
    }

    /// 打分与排序。**纯函数** —— 不碰数据库。
    ///
    /// # 三处照抄要点（见模块文档）
    ///
    /// 1. 候选影片**可能同时**在历史证据里（两个窗口重叠 30 天），所以
    ///    `historical_movie_count` 要**减 1**、`score` 要**扣掉自己那份证据**。
    /// 2. 不足 `MIN_HISTORICAL_MOVIES` **直接跳过**（不是降权）。
    /// 3. 同影片多女优时取 `(score, -actor_id)` 大者 ⇒ **平局取 id 小者**；
    ///    影片间按 `(score, release_date, movie_id)` **全降序**。
    pub fn score_movies(
        candidates: &[CandidateRow],
        evidence: &HashMap<i32, ActorEvidence>,
    ) -> Vec<ScoredMovie> {
        let mut best_by_movie: HashMap<i32, ScoredMovie> = HashMap::new();
        for row in candidates {
            // 同 `build_evidence`：解构取名，位置含义见
            // `sm_db::repo::discovery::CandidateRow`。
            let (movie_id, actor_id, release_date) = *row;
            let Some(actor) = evidence.get(&actor_id) else {
                continue;
            };
            // 影片自己的证据（可能没有 —— 候选窗口右半段不进历史）。
            let own = actor.by_movie_id.get(&movie_id).copied();
            let historical_movie_count = actor.movie_count - i64::from(own.is_some());
            if historical_movie_count < MIN_HISTORICAL_MOVIES {
                continue;
            }
            let score = (actor.total - own.unwrap_or(0.0)) / historical_movie_count as f64;
            let candidate = ScoredMovie {
                movie_id: movie_id as i64,
                release_date,
                actor_id: actor_id as i64,
                historical_movie_count,
                score,
            };
            // 平局取 actor_id **小**者：比较 `(score, -actor_id)` 取大者。
            let replace = match best_by_movie.get(&movie_id) {
                None => true,
                Some(current) => {
                    (candidate.score, -candidate.actor_id) > (current.score, -current.actor_id)
                }
            };
            if replace {
                best_by_movie.insert(movie_id, candidate);
            }
        }
        let mut out: Vec<ScoredMovie> = best_by_movie.into_values().collect();
        // 全降序：同分时更新的片在前，同分同日时 movie_id 大的在前。
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.release_date.cmp(&a.release_date))
                .then_with(|| b.movie_id.cmp(&a.movie_id))
        });
        out
    }
}
/// 需要跨到别的域取数据的部分：影片卡片与女优资料。
///
/// # 刻意留在 `todo!()`：它依赖 `repo/movie.rs` 与 `repo/actor.rs` 的具体形态
///
/// 上游 `_page_resources`（`:160-216`）要：
///
/// - `with_movie_card_relations(Movie.select(Movie))` —— 影片卡片需要的关联
/// - `attach_movie_list_media(movies)` —— 挂媒体信息
/// - `Actor` + `Image` 的**双 LEFT JOIN**（`profile_image` 与
///   `profile_image_override`，后者优先）
///
/// 最后一条尤其要小心：上游用 `Image.alias()` 做了两次 LEFT JOIN，
/// `effective_profile_image` 优先取 override。**照抄时要保留「override 优先」
/// 的语义** —— 只取 `profile_image` 会让用户设的头像不生效。
///
/// ★ 骨架期那个 `PageContext` trait 已删除：它让 API 层注入「按 id 取一个
/// `serde_json::Value`」，形状无处校验（字段名写错不会编译失败）。这里改成
/// **服务层取真类型**（`MovieCard` / `ActorView`），签名留在 API 层 ——
/// 与 `moment_recommendation` / `daily_recommendation` 同一取向。
///
/// 「黑名单影片」这条要特别说清：上游 `with_movie_card_relations` 会**过滤**
/// `is_blacklisted`，而本仓的 [`MovieService::load_cards`] 走
/// `MovieRepository::find_by_ids`（`SELECT * FROM movie WHERE id = ANY($1)`，
/// **不带黑名单过滤**）。所以黑名单影片的**过滤在本模块做**
/// （见 [`HotActressReleaseQuery::list_items`]）—— 不能指望卡片那层。
pub struct HotActressReleaseQuery {
    db: Db,
    repo: HotActressReleaseRepository,
}

impl HotActressReleaseQuery {
    /// 构造。取 `&Db`（要顺便建卡片与女优两个服务）。
    pub fn new(db: &Db) -> Self {
        Self {
            db: db.clone(),
            repo: HotActressReleaseRepository::new(db.clone()),
        }
    }

    /// 读快照并装配成一页。上游 `list_items`（`:218-238`）。
    ///
    /// # 顺序：**先切片，再取数**
    ///
    /// 上游 `:234` 是 `_page_resources(scored_movies[start:start+page_size])` ——
    /// 取卡片与女优只针对**本页**的 id。反过来（先全量取再切）在候选上千时
    /// 会白取几百部影片。
    ///
    /// # `total` 与 `items.len()` 会不一致
    ///
    /// `total = 打分后的候选总数`；而本页里被跳过（黑名单影片 / 女优已删）
    /// 的条目不补位 —— 所以 `items.len() <= page_size`，且
    /// `items.len() <= total - offset`。
    pub async fn list_items(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<HotActressReleasePage, ServiceError> {
        Self::validate_page(page, page_size)?;
        let today = Self::today();
        let scored = self.scored(today).await?;
        let total = scored.len() as i64;

        let offset = ((page - 1) * page_size).max(0) as usize;
        let window: Vec<ScoredMovie> = scored
            .into_iter()
            .skip(offset)
            .take(page_size.max(0) as usize)
            .collect();

        // 两批取数（卡片 / 女优）都只针对本页，且都是 `HashMap` 按 id 对齐 ——
        // 不按下标，因为仓储的返回顺序不保证。
        let movie_ids: Vec<i32> = window.iter().map(|item| item.movie_id as i32).collect();
        let mut cards: HashMap<i32, MovieCard> = MovieService::new(&self.db)
            .load_cards(&movie_ids)
            .await?
            .into_iter()
            .map(|card| (card.movie.id, card))
            .collect();
        let actor_ids: Vec<i32> = window.iter().map(|item| item.actor_id as i32).collect();
        let mut actresses = ActorService::new(&self.db).views_of(&actor_ids).await?;

        let items = window
            .into_iter()
            .filter_map(|item| {
                let card = cards.remove(&(item.movie_id as i32))?;
                // 黑名单影片不进结果（上游卡片查询就把它滤掉了）。**在取出
                // 卡片之后判**：`find_by_ids` 不过滤黑名单，这里漏判就会把
                // 拉黑的影片摆进「热播女优新作」。
                if card.movie.is_blacklisted {
                    return None;
                }
                let actress = actresses.remove(&(item.actor_id as i32))?;
                Some(HotActressReleaseItem {
                    card,
                    actress,
                    historical_movie_count: item.historical_movie_count,
                    score: item.score,
                })
            })
            .collect();

        Ok(HotActressReleasePage {
            items,
            page,
            page_size,
            total,
        })
    }

    /// 完整流程：取历史证据 + 候选 → 打分排序 → 切片。
    ///
    /// `total` 是**候选影片总数**（打分后的长度），**不是数据库里的行数** ——
    /// 上游 `len(scored_movies)`（`:237`）。所以 `total` 依赖打分结果，
    /// 不能用 `COUNT(*)` 顶替。
    pub async fn scored(&self, today: chrono::NaiveDate) -> Result<Vec<ScoredMovie>, ServiceError> {
        let windows = HotActressReleaseService::windows(today);
        let repo = self.repo.clone();
        // 两个查询都跑在各自的快照事务里。要「同一快照」得合并成一次调用，
        // 这里保持两次 —— 两次快照之间的差异最多影响边界那一天的影片，
        // 而打分是按证据排序的，边界抖动不改变名次。
        // 若之后要求严格一致，把两条 SQL 合成一条（同一个 CTE）。
        let (history_rows, candidate_rows) = tokio::try_join!(
            repo.history_actor_rows(windows.history_start, windows.history_end),
            repo.candidate_rows(windows.candidate_start, windows.candidate_end),
        )?;
        let evidence = HotActressReleaseService::build_evidence(&history_rows, today);
        Ok(HotActressReleaseService::score_movies(
            &candidate_rows,
            &evidence,
        ))
    }

    /// 今日（本地日期）为基准的全量打分结果。
    pub async fn scored_today(&self) -> Result<Vec<ScoredMovie>, ServiceError> {
        self.scored(Self::today()).await
    }

    /// 基准日 = 本地今天。
    ///
    /// 上游 `_today()`（`:46-47`）用 `runtime_now().date()` —— **本地日期**，
    /// 不是 UTC。所以不能用 `Utc::now()`：UTC 与本地跨日时，窗口边界会差一天。
    pub fn today() -> chrono::NaiveDate {
        chrono::Local::now().date_naive()
    }

    /// 校验分页参数。
    ///
    /// 上游用 `validate_page(page, page_size,
    /// error_code="invalid_hot_actress_release_filter")` —— **专用错误码**。
    /// 照抄：客户端要靠它区分「分页参数错了」与「筛选条件错了」。
    pub fn validate_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
        if page < 1 {
            return Err(ServiceError::validation(
                "invalid_hot_actress_release_filter",
                "page 从 1 开始",
            ));
        }
        // 上界用 100 —— 与 `sm_db::common::page::validate_page` 的口径一致。
        if !(1..=100).contains(&page_size) {
            return Err(ServiceError::validation(
                "invalid_hot_actress_release_filter",
                "page_size 需在 1..=100 之间",
            ));
        }
        Ok(())
    }
}
