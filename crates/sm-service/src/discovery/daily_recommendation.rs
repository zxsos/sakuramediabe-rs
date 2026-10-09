//! 每日推荐（上游 `daily_recommendation_service.py`，18.7KB / 476 行）。
//!
//! 依赖：[`super::recommendation`]（稀疏相似度）+ [`super::ranking`] **读侧**。
//! **不依赖 provider 插件** —— 榜单分只用 `ranking_item` 表，与插件写侧无关。
//!
//! # 读侧与生成侧都已落地
//!
//! - **读侧** [`DailyRecommendationService::list_items`] —— `GET /daily-recommendations`；
//! - **生成侧** [`DailyRecommendationService::generate_latest_snapshot`] —— 调度任务
//!   `daily_recommendation_generate`（cron `daily_recommendation_generate_cron`，
//!   默认 `0 5 * * *`）。
//!
//! 生成是**整体替换**语义（`rank` / `movie_id` 全表唯一），所以读侧看到的永远是
//! 「当前这一批」，而不是历史累积 —— 库里还没有快照时读端点返回**空页**，
//! 那是正常状态（与上游一致），不是 500。
//!
//! # 三种打分制度：不是一套权重走到底
//!
//! ```text
//!   有兴趣信号（最近播放过 / 订阅演员 / 订阅影片）
//!     -> REGULAR_WEIGHTS   相似度 8/19 · 订阅演员 3/19 · 订阅影片 2/19
//!                           热度 4/19 · 榜单 2/19
//!   无兴趣信号 且 无公共信号（极冷启动）
//!     -> 只用 freshness
//!   无兴趣信号 但有公共信号（冷启动）
//!     -> COLD_START_WEIGHTS  热度 11/18 · 榜单 5/18 · 新鲜度 2/18
//! ```
//!
//! **为什么要分三种**：新用户没有任何交互记录，相似度与订阅信号全为 0。此时若用
//! `REGULAR_WEIGHTS`，那 13/19 的权重会乘在 0 上 —— **所有影片得分都只剩
//! 热度与榜单的 6/19**，排序退化。所以要么退到 `COLD_START_WEIGHTS`（重新分配
//! 热度/榜单的权重），要么在完全没有公共信号时只用新鲜度。
//!
//! # 候选只投影 5 列，不加载完整模型
//!
//! 上游注释（`:132`）：「全库候选只投影打分所需列，避免 30 万完整 Movie 模型
//! 驻留内存（**实测峰值 3.9GB**）」。
//!
//! `is_collection` 只用于 `WHERE` 过滤，**不进入投影**。
//!
//! # 理由码 → 文案：**未知码被丢弃**
//!
//! 上游 `:259` 是 `REASON_TEXTS[code] for code in codes if code in REASON_TEXTS`
//! —— **不在表里的码直接不产出文案**。
//!
//! ⚠️ 我早先在骨架注释里写的是「未知码降级为原样返回，不丢」—— **那是错的**。
//! 差别在于「多一条看不懂的文案」与「少一条文案」：上游选后者。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use sm_db::collections::PLAYLIST_KIND_RECENTLY_PLAYED;
use sm_db::common::now_utc;
use sm_db::common::page::{Page, PageRequest};
use sm_db::repo::{
    DailyRecommendationItemRepository, MovieActorRepository, MovieRepository,
    NewDailyRecommendation, PlaylistMovieRepository, PlaylistRepository, RankingItemRepository,
};
use sm_db::{DailyRecommendationItem, Db};

use super::qdrant::similarity::MovieSimilarityStore;
use crate::catalog::movie::{MovieCard, MovieService};
use crate::error::ServiceError;

/// 相似度权重（`REGULAR_WEIGHTS`）。
pub const W_SIMILARITY: f64 = 8.0 / 19.0;
/// 订阅演员权重。
pub const W_SUBSCRIBED_ACTOR: f64 = 3.0 / 19.0;
/// 订阅影片权重。
pub const W_SUBSCRIBED_MOVIE: f64 = 2.0 / 19.0;
/// 热度权重（常规制度）。
pub const W_HEAT_REGULAR: f64 = 4.0 / 19.0;
/// 榜单权重（常规制度）。
pub const W_RANKING_REGULAR: f64 = 2.0 / 19.0;

/// 冷启动制度下的热度权重。
pub const W_HEAT_COLD: f64 = 11.0 / 18.0;
/// 冷启动制度下的榜单权重。
pub const W_RANKING_COLD: f64 = 5.0 / 18.0;
/// 冷启动制度下的新鲜度权重。
pub const W_FRESHNESS_COLD: f64 = 2.0 / 18.0;

/// 快照容量。上游 `DAILY_RECOMMENDATION_LIMIT = 200`。
///
/// 全库候选（可能 30 万）打折分后**只留前 200 条**进快照表：客户端一次
/// 只翻几页，而多存的每条都要付 `daily_recommendation_item` 唯一约束的
/// 插入代价。
pub const DAILY_RECOMMENDATION_LIMIT: i64 = 200;
/// 榜单名次衰减窗口。名次 > 100 得 0 分。
pub const RANK_DECAY_WINDOW: i64 = 100;
/// 最近播放种子影片数上限。
pub const RECENT_SEED_LIMIT: i64 = 30;
/// 每个种子的相似影片取几条。
pub const SIMILARITY_PER_SEED_LIMIT: i64 = 50;

/// 理由码。
pub mod reason {
    pub const SIMILAR_RECENT_PLAY: &str = "similar_recent_play";
    pub const SUBSCRIBED_ACTOR: &str = "subscribed_actor";
    pub const SUBSCRIBED_MOVIE: &str = "subscribed_movie";
    pub const POPULAR_MOVIE: &str = "popular_movie";
    pub const RANKING_TRENDING: &str = "ranking_trending";
    pub const NEW_RELEASE: &str = "new_release";
}

/// 理由码 → 展示文案。**表外的码不产出文案**（照上游 `:259`）。
///
/// 只有这 6 个。**新增理由码必须同时加文案**，否则界面上什么都不显示。
pub fn reason_text(code: &str) -> Option<&'static str> {
    Some(match code {
        reason::SIMILAR_RECENT_PLAY => "与你最近播放的影片相似",
        reason::SUBSCRIBED_ACTOR => "包含已订阅演员",
        reason::SUBSCRIBED_MOVIE => "你已订阅这部影片",
        reason::POPULAR_MOVIE => "近期热度较高",
        reason::RANKING_TRENDING => "来自近期榜单",
        reason::NEW_RELEASE => "较新发布或近期入库",
        _ => return None,
    })
}

/// 理由码列表 → 文案列表。**丢弃表外的码**。
pub fn reason_texts(codes: &[String]) -> Vec<String> {
    codes
        .iter()
        .filter_map(|code| reason_text(code))
        .map(str::to_owned)
        .collect()
}

/// 候选影片的轻量投影。**只 5 个字段**（见模块文档）。
///
/// # 两个字段的类型**必须**与 `movie` 表的列一致
///
/// | 字段 | 列 | 类型 |
/// |---|---|---|
/// | `heat` | `movie.heat` | `integer NOT NULL` |
/// | `release_date` | `movie.release_date` | `timestamp NULL`（上游 `DateTimeField(null=True)`）|
///
/// **`release_date` 不是 `date`**：这里曾经写成 `Option<NaiveDate>` —— 那在
/// 从 SQL 解码时就会 `ColumnDecode` 失败（列是 `timestamp`），而且即便转成功
/// 也会丢掉时分秒，让 `freshness` 在同一天内的排序失去区分度。上游
/// `_release_sort_value`（`:122-128`）对 `datetime` 是**原样返回**，只有拿到
/// `date` 才补 `time.min` —— 说明它拿到的其实是 `datetime`。
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateMovie {
    pub id: i64,
    /// 热度。列为 `NOT NULL`；用 `i64` 是为了与 `heat_scores` 的整数算术同型
    /// （上游 `int(row.heat or 0)`）。
    pub heat: i64,
    pub release_date: Option<chrono::NaiveDateTime>,
    pub created_at: Option<chrono::NaiveDateTime>,
    /// 是否已订阅。**随候选一起查**（上游注释 `:275`：无需再查 30 万行范围）。
    pub is_subscribed: bool,
}

/// 一部影片的六个信号分（各自已归一到 [0, 1]）。
///
/// # 字段名就是 `signal_scores` 列的 JSON 键
///
/// 上游存的是 `{"similarity": …, "subscribed_actor": …, …}`（`:289-296`），
/// 这里靠 `derive(Serialize)` + 同名字段直接产出同一个形状。**字段改名 =
/// 改客户端读得到的键**，而它不会报错，只会让某一块显示成空白。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalScores {
    pub similarity: f64,
    pub subscribed_actor: f64,
    pub subscribed_movie: f64,
    pub heat: f64,
    pub ranking: f64,
    pub freshness: f64,
}

/// 打分结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredRecommendation {
    pub movie: CandidateMovie,
    pub score: f64,
    /// 理由码。**可能为空**（所有信号都是 0）。
    pub reason_codes: Vec<String>,
    pub signals: SignalScores,
}

/// 打分统计（会进任务摘要）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreStats {
    /// 无兴趣信号（走了冷启动制度）。
    pub cold_start: bool,
    /// 极冷启动（无兴趣**且**无公共信号，只用新鲜度）。
    pub extreme_cold_start: bool,
    pub recent_seed_movies: i64,
    pub candidate_movies: i64,
}
/// 归一到 `[0, 1]`。上游 `_normalize`（`:112`）。
pub fn normalize(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// 榜单名次衰减。
///
/// # `rank > 100` 直接 0 分，不是线性衰减到负数再 clamp
///
/// 上游 `:117-119`：
///
/// ```python
/// if rank <= 0 or rank > RANK_DECAY_WINDOW: return 0.0
/// return max(0.0, (1.0 - ((rank - 1) / RANK_DECAY_WINDOW)) * weight)
/// ```
///
/// **名次 0 或负数也得 0** —— 那是脏数据，不能让它拿满分。
pub fn rank_decay(rank: i64, weight: f64) -> f64 {
    if rank <= 0 || rank > RANK_DECAY_WINDOW {
        return 0.0;
    }
    (1.0 - ((rank - 1) as f64 / RANK_DECAY_WINDOW as f64)) * weight
}

/// 榜单周期权重。**未知周期 0.5**（上游 `:229` 的 `.get(..., 0.5)`）。
///
/// `""`（总榜）是 0.7 —— 与 weekly 同权重。上游 `period_weights` 里显式列了
/// `"": 0.7`，不是走默认。
pub fn period_weight(period: &str) -> f64 {
    match period.to_ascii_lowercase().as_str() {
        "daily" => 1.0,
        "weekly" => 0.7,
        "monthly" => 0.4,
        "" => 0.7,
        _ => 0.5,
    }
}

/// 种子权重：按种子在列表中的**位置线性衰减**。
///
/// 上游 `:187-189`：`1.0 - (index / max(len(seed_ids), 1))`。
/// **第一个种子权重接近 1，最后一个接近 0**（不完全为 0）。
pub fn seed_weight(index: usize, total: usize) -> f64 {
    1.0 - (index as f64 / total.max(1) as f64)
}

/// 热度分。**用 95 分位做参考值，不是最大值**。
///
/// 上游 `:211-217`：
///
/// ```python
/// positive_heats = sorted(heat for heat in heats if heat > 0)
/// index = max(0, ceil(len * 0.95) - 1)
/// heat_ref = max(float(positive_heats[index]), 1.0)
/// ```
///
/// **为什么用分位不用最大**：一部爆款影片的 heat 可能是普通影片的百倍，用它
/// 做分母会让**其余所有影片的得分挤在 0.00~0.05 之间** —— 排序完全失去区分度。
/// 95 分位意味着「绝大多数影片的满分线」，爆款会被截到 1.0。
///
/// `max(..., 1.0)` 是下界：全部 heat 都很小时不要放大噪声。
///
/// # `heat <= 0` 的影片得 0 分
///
/// `normalize(0 / heat_ref)` = 0。不排除它们 —— 它们仍在候选里，只是没热度分。
pub fn heat_scores(movies: &[CandidateMovie]) -> HashMap<i64, f64> {
    let mut positive: Vec<i64> = movies.iter().map(|m| m.heat).filter(|h| *h > 0).collect();
    if positive.is_empty() {
        return HashMap::new();
    }
    positive.sort_unstable();
    // 95 分位：`ceil(n * 0.95) - 1`（Python 的 `math.ceil` 是向上取整）。
    let rank = ((positive.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    let reference = (positive[rank.min(positive.len() - 1)] as f64).max(1.0);
    movies
        .iter()
        .map(|movie| (movie.id, normalize(movie.heat as f64 / reference)))
        .collect()
}

/// 新鲜度分。
///
/// 上游 `:237-255`：按 `(发行日, 入库时间, id)` **降序**排，然后
/// `1 - index / (n - 1)`。
///
/// **只有一部影片时给 1.0**（`:249-250`）—— 分母会退化成 0，所以特判。
/// 那个特判不是可有可无的：一部影片时它是唯一候选，给 0 分会让它排到后面。
pub fn freshness_scores(movies: &[CandidateMovie]) -> HashMap<i64, f64> {
    if movies.is_empty() {
        return HashMap::new();
    }
    if movies.len() == 1 {
        return HashMap::from([(movies[0].id, 1.0)]);
    }
    let mut ordered: Vec<&CandidateMovie> = movies.iter().collect();
    ordered.sort_by(|a, b| {
        b.release_date
            .cmp(&a.release_date)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.id.cmp(&a.id))
    });
    let denominator = (ordered.len() - 1).max(1) as f64;
    ordered
        .iter()
        .enumerate()
        .map(|(index, movie)| (movie.id, normalize(1.0 - index as f64 / denominator)))
        .collect()
}

/// 榜单分。同一影片取**所有周期里的最大值**，不是累加。
///
/// 上游 `:230-232` 用 `max`。用累加的话「四个榜都进前 50」会碾压
/// 「日榜第 1」—— 而后者是更强的信号。
pub fn ranking_scores(rows: &[(i64, i64, String)]) -> HashMap<i64, f64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (movie_id, rank, period) in rows {
        let decayed = rank_decay(*rank, period_weight(period));
        let entry = scores.entry(*movie_id).or_insert(0.0);
        if decayed > *entry {
            *entry = decayed;
        }
    }
    scores
}

/// 相似度分。**Qdrant 故障时返回空表**（跳整路信号）。
///
/// 上游 `:196-199` 捕获 `MovieSimilarityIndexError` 后 `return {}` ——
/// 「每日推荐还有热度、榜单等信号，Qdrant 故障时只跳过相似度信号」。
///
/// **不是抛错**。这与 `recommendation::search_similar_movies` 的降级是同一
/// 层意思，但**这里更进一步**：连 `NotReady` 也降级（上游捕获的是基类
/// `MovieSimilarityIndexError`，两个子类都包含）。
///
/// 所以**新用户与索引未建好的用户看到的推荐是一样的**（都是冷启动推荐）。
pub fn similarity_scores(
    hits_by_seed: &HashMap<i64, Vec<super::MovieSimilarityHit>>,
    seed_weights: &HashMap<i64, f64>,
    candidate_ids: &HashSet<i64>,
) -> HashMap<i64, f64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (seed_id, hits) in hits_by_seed {
        let Some(&seed_weight) = seed_weights.get(seed_id) else {
            continue;
        };
        for hit in hits {
            if !candidate_ids.contains(&hit.movie_id) {
                continue;
            }
            let score = normalize(hit.score as f64 * seed_weight);
            let entry = scores.entry(hit.movie_id).or_insert(0.0);
            if score > *entry {
                *entry = score;
            }
        }
    }
    scores
}
/// 是否有「兴趣信号」（决定走哪套权重）。
///
/// 上游 `:280`：`bool(recent_seed_ids or subscribed_actor_movie_ids or
/// subscribed_movie_ids)`。
// ⚠️ **不能** derive `Copy`：`Vec` / `HashSet` 都带堆分配。
#[derive(Debug, Clone, Default)]
pub struct InterestSignals {
    pub recent_seed_ids: Vec<i64>,
    pub subscribed_actor_movie_ids: HashSet<i64>,
    pub subscribed_movie_ids: HashSet<i64>,
}

impl InterestSignals {
    /// 是否有任何兴趣信号。
    pub fn any(&self) -> bool {
        !self.recent_seed_ids.is_empty()
            || !self.subscribed_actor_movie_ids.is_empty()
            || !self.subscribed_movie_ids.is_empty()
    }
}

/// 选出打分制度。
///
/// # 三种制度，边界是「有没有兴趣信号」×「有没有公共信号」
///
/// 上游 `:277-284`：
///
/// ```python
/// has_interest_signal = bool(seeds or subscribed_actors or subscribed_movies)
/// has_public_signal = any(s > 0 for s in (heat, ranking) for v in s.values())
/// extreme_cold_start = not has_interest_signal and not has_public_signal
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreRegime {
    /// 常规：有兴趣信号 -> `REGULAR_WEIGHTS`
    Regular,
    /// 冷启动：无兴趣信号但有公共信号 -> `COLD_START_WEIGHTS`
    ColdStart,
    /// 极冷启动：两者都无 -> **只用新鲜度**
    ExtremeColdStart,
}

/// 判定制度。**纯函数**。
pub fn select_regime(interest: &InterestSignals, has_public_signal: bool) -> ScoreRegime {
    if interest.any() {
        return ScoreRegime::Regular;
    }
    if has_public_signal {
        ScoreRegime::ColdStart
    } else {
        ScoreRegime::ExtremeColdStart
    }
}

/// 按制度给一部影片打分。**纯函数**。
///
/// # 极冷启动时**只有新鲜度参与**
///
/// 上游 `:299-300`：`score = signal_scores["freshness"]`。**不是**用
/// `COLD_START_WEIGHTS`（那一支是给「有公共信号但无兴趣信号」用的）。
///
/// # 为什么还要 `.max(0.0)`
///
/// 六个信号都已归一到 `[0, 1]`，权重都是正数，所以加权和**理论上非负**。
/// 但浮点误差下 `heat * 11/18 + ranking * 5/18 + freshness * 2/18` 在全 0 时
/// 可能得到 `-0.0` —— 而 `-0.0 < 0.0` 为假，所以不会被 clamp 改动，只是会在
/// 序列化时输出 `-0`。**显式 `max(0.0)` 消掉这个边界。**
pub fn score_one(signals: &SignalScores, regime: ScoreRegime) -> f64 {
    let raw = match regime {
        ScoreRegime::Regular => {
            signals.similarity * W_SIMILARITY
                + signals.subscribed_actor * W_SUBSCRIBED_ACTOR
                + signals.subscribed_movie * W_SUBSCRIBED_MOVIE
                + signals.heat * W_HEAT_REGULAR
                + signals.ranking * W_RANKING_REGULAR
        }
        ScoreRegime::ColdStart => {
            signals.heat * W_HEAT_COLD
                + signals.ranking * W_RANKING_COLD
                + signals.freshness * W_FRESHNESS_COLD
        }
        ScoreRegime::ExtremeColdStart => signals.freshness,
    };
    raw.max(0.0)
}

/// 分配理由码。**纯函数**。
///
/// 上游 `:304-316`。`new_release` 的条件**不是**「新鲜度 > 0」：
///
/// ```python
/// if extreme_cold_start or (not has_interest_signal and signal_scores["freshness"] > 0):
///     reason_codes.append("new_release")
/// ```
///
/// **即「无兴趣信号时且有新鲜度」**（极冷启动是它的子集）。有用户兴趣时
/// **不给**「较新发布」这个理由 —— 因为那不是用户选择它的原因。
pub fn assign_reasons(
    signals: &SignalScores,
    regime: ScoreRegime,
    has_interest_signal: bool,
) -> Vec<String> {
    let mut codes = Vec::with_capacity(6);
    if signals.similarity > 0.0 {
        codes.push(reason::SIMILAR_RECENT_PLAY.to_owned());
    }
    if signals.subscribed_actor > 0.0 {
        codes.push(reason::SUBSCRIBED_ACTOR.to_owned());
    }
    if signals.subscribed_movie > 0.0 {
        codes.push(reason::SUBSCRIBED_MOVIE.to_owned());
    }
    if signals.heat > 0.0 {
        codes.push(reason::POPULAR_MOVIE.to_owned());
    }
    if signals.ranking > 0.0 {
        codes.push(reason::RANKING_TRENDING.to_owned());
    }
    // 注意是 `regime == Extreme || (!has_interest && freshness > 0)`，
    // 不是单纯 `freshness > 0`。
    if regime == ScoreRegime::ExtremeColdStart || (!has_interest_signal && signals.freshness > 0.0)
    {
        codes.push(reason::NEW_RELEASE.to_owned());
    }
    codes
}

/// 排序。**纯函数** —— `scored` 按 `(score, 发行日, 入库时间, id)` **全降序**。
///
/// 上游 `:330-338` 用 `reverse=True`，所以**同分时更新的片在前，同分同日时
/// `id` 大的在前**。后半段容易写反。
pub fn sort_scored(scored: &mut [ScoredRecommendation]) {
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.movie.release_date.cmp(&a.movie.release_date))
            .then_with(|| b.movie.created_at.cmp(&a.movie.created_at))
            .then_with(|| b.movie.id.cmp(&a.movie.id))
    });
}
/// 进度上报（签名与 `image_search` / `recommendation` 那套一致）。
///
/// # `+ Send` 是必需的，不是装饰
///
/// 它被 move 进 worker 的 handler future，而 `TaskHandler` 要求
/// `Pin<Box<dyn Future + Send>>`。少了 `Send`，`sm-scheduler` 注册
/// `daily_recommendation_generate` 时才会发现它装不进去。
pub type ProgressSink<'a> = Box<
    dyn FnMut(
            Option<i32>,
            Option<i32>,
            &str,
            Option<&serde_json::Value>,
        ) -> BoxFuture<'a, Result<(), String>>
        + Send
        + 'a,
>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 快照日期：未指定 = 今天（**本地日期**）。
///
/// 上游 `_snapshot_date`（`:108-109`）用 `runtime_now().date()`。**不是 UTC** ——
/// 用 UTC 会在跨日时让「今天的推荐」算成昨天的。
pub fn snapshot_date(target: Option<chrono::NaiveDate>) -> chrono::NaiveDate {
    target.unwrap_or_else(|| chrono::Local::now().date_naive())
}

/// 快照生成的结果摘要。**键名与上游 stats dict 逐字一致**
/// （`daily_recommendation_service.py:375-382`）。
///
/// 它会进 `background_task_run.result_summary`，客户端按这些键读数字 ——
/// 改名不会报错，只会让某一块显示成空白。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotStats {
    /// 快照日期，`YYYY-MM-DD`（**本地日期**，见 [`snapshot_date`]）。
    ///
    /// 序列化成字符串而不是 `NaiveDate`：上游 `snapshot_date.isoformat()`，
    /// 客户端按同样的字符串读。
    pub snapshot_date: String,
    /// 参与打分的候选数（**全库非集合、未拉黑**，不是入选数）。
    pub candidate_movies: i64,
    /// 实际写进快照表的条数（`min(候选数, limit)`）。
    pub stored_items: i64,
    /// 无兴趣信号（走了冷启动制度）。见 [`ScoreStats::cold_start`]。
    pub cold_start: bool,
    /// 极冷启动（无兴趣**且**无公共信号，只用新鲜度）。
    pub extreme_cold_start: bool,
    /// 本轮取到的「最近播放」种子数。
    pub recent_seed_movies: i64,
}

/// 每日推荐分页参数非法时的错误码。
///
/// 上游 `list_items`（`daily_recommendation_service.py:422`）传
/// `error_code="invalid_daily_recommendation_filter"`。
pub const INVALID_DAILY_RECOMMENDATION_FILTER: &str = "invalid_daily_recommendation_filter";

/// 校验分页参数。违规 → 422 [`INVALID_DAILY_RECOMMENDATION_FILTER`]。
///
/// `details` 的形状由 `sm_core::pagination::PageError` 决定
/// （`{"page": 0}` / `{"page_size": 101}`），与上游 `validate_page` 一致。
fn validate_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
    sm_core::pagination::validate_page(page, page_size).map_err(|error| {
        let details = match error.details() {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("page".to_owned(), other);
                map
            }
        };
        ServiceError::validation_with(
            INVALID_DAILY_RECOMMENDATION_FILTER,
            error.message(),
            details,
        )
    })
}

/// 一条每日推荐的读侧装配结果：影片卡片 + 快照行 + 陈旧标记。
///
/// 路由层拿它组装 `DailyRecommendationMovieResource`（完整卡片 + 8 个推荐
/// 字段）—— **卡片才是主体**，不是「几个缩过的字段 + 片名」。
///
/// `reason_codes` / `reason_texts` / `signal_scores` 一律取
/// [`DailyRecommendationItem`]（库里**存的**值），**不重算**：生成那一刻的
/// 文案就是权威，后来的理由码表变动不该改写历史快照。上游 `row.* or []` /
/// `or {}` 同样是读存值（`daily_recommendation_service.py:464-466`）。
#[derive(Debug, Clone)]
pub struct DailyRecommendationCard {
    /// 影片卡片（[`MovieService::load_cards`] 的产物）。
    pub card: MovieCard,
    /// 快照行本身。
    pub item: DailyRecommendationItem,
    /// 快照日期是否早于**今天**（本地日期）。上游 `row.snapshot_date < today`
    /// （`daily_recommendation_service.py:467`）。
    pub is_stale: bool,
}

/// 打分需要的外部输入。**与 DB 无关**，便于直接测 `score_movies`。
pub struct ScoreInputs {
    pub movies: Vec<CandidateMovie>,
    pub interest: InterestSignals,
    /// 每部候选的榜单行 `(movie_id, rank, period)`。
    pub ranking_rows: Vec<(i64, i64, String)>,
    /// 每个种子的相似影片命中。**空表 = 相似度信号缺失**（Qdrant 故障已降级）。
    pub hits_by_seed: HashMap<i64, Vec<super::MovieSimilarityHit>>,
}

/// 每日推荐服务。
///
/// 读侧 [`Self::list_items`] 持 `Db`（构造照
/// [`crate::transfers::download_task::DownloadTaskService::new`]）；打分主体
/// [`Self::score_movies`] 刻意是**关联函数而非方法**：它不碰 `self`，输入全部
/// 由调用方取好。这样评分逻辑（六路信号 × 三种制度）能被完整单测，不需要
/// mock 数据库。
///
/// 骨架期这里 `impl` 了一个并不存在的 `DailyRecommendationService` ——
/// 结构体忘了写。现在补上，并只填读侧。
#[derive(Debug, Clone)]
pub struct DailyRecommendationService {
    db: Db,
}

impl DailyRecommendationService {
    /// 构造。取 `&Db` 并克隆（与 `DownloadTaskService::new` 同形），
    /// 调用方写 `new(state.db())`。
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    /// `GET /daily-recommendations`：读**当前**快照的一页，按 `rank`。
    ///
    /// 对应上游 `list_items`（`daily_recommendation_service.py:414-476`）：
    ///
    /// 1. `validate_page`（专用错误码 [`INVALID_DAILY_RECOMMENDATION_FILTER`]）；
    /// 2. `DailyRecommendationItem ⋈ Movie(is_blacklisted=False)` 分页，
    ///    `ORDER BY rank`（[`DailyRecommendationItemRepository::list_visible_page`]）；
    /// 3. **一次**批量装配这批影片的卡片（[`MovieService::load_cards`]）；
    /// 4. `is_stale = snapshot_date < 今天`（**元素级**，不是页级）。
    ///
    /// # 顺序由 `rank` 决定
    ///
    /// `load_cards` **只查不排**（按入参 id 顺序返回），这里把 `movie_ids`
    /// 按 `rank` 序传进去，再按 `rows` 的顺序组装 —— 与上游 `for row in rows`
    /// 一致。
    ///
    /// # 并发删除时**跳过**而不是报错
    ///
    /// 分页与卡片装配之间有窗口：影片若在这中间被删，卡片取不到。上游
    /// `if movie is None: continue` 同样是跳过（`daily_recommendation_item`
    /// 的 FK 是 `ON DELETE CASCADE`，正常路径下这一行也已被连带删除）。
    pub async fn list_items(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<Page<DailyRecommendationCard>, ServiceError> {
        validate_page(page, page_size)?;
        // 分页参数已按上游口径校验过；这里的映射不会失败。
        let request = PageRequest::new(page, page_size).map_err(ServiceError::from)?;

        let rows = DailyRecommendationItemRepository::new(self.db.clone())
            .list_visible_page(request)
            .await?;
        let total = rows.total;
        if rows.items.is_empty() {
            return Ok(Page::new(Vec::new(), total));
        }

        let movie_ids: Vec<i32> = rows.items.iter().map(|row| row.movie_id).collect();
        let cards = MovieService::new(&self.db).load_cards(&movie_ids).await?;
        let mut cards_by_id: HashMap<i32, MovieCard> = cards
            .into_iter()
            .map(|card| (card.movie.id, card))
            .collect();

        let today = chrono::Local::now().date_naive();
        let items = rows
            .items
            .into_iter()
            .filter_map(|item| {
                // 影片在分页与装配之间被删（并发）时卡片缺失 —— 跳过。
                let card = cards_by_id.remove(&item.movie_id)?;
                let is_stale = item.snapshot_date < today;
                Some(DailyRecommendationCard {
                    card,
                    item,
                    is_stale,
                })
            })
            .collect();
        Ok(Page::new(items, total))
    }

    /// ★ 打分主体。**纯函数**（输入已由调用方取好，无 IO）。
    ///
    /// 刻意做成同步的：**把 IO 留在外面，评分逻辑就能被完整测试**。
    /// 六路信号（相似度 / 订阅演员 / 订阅影片 / 热度 / 榜单 / 新鲜度）全部是
    /// 已归一的小数，三种制度的选择与理由码分配都不碰数据库。
    pub fn score_movies(inputs: ScoreInputs) -> (Vec<ScoredRecommendation>, ScoreStats) {
        let candidate_ids: HashSet<i64> = inputs.movies.iter().map(|m| m.id).collect();
        let heat = heat_scores(&inputs.movies);
        let ranking = ranking_scores(&inputs.ranking_rows);
        let freshness = freshness_scores(&inputs.movies);

        // 公共信号 = 热度或榜单里**有任何一个 > 0**（上游 `:281-284`）。
        let has_public_signal =
            heat.values().any(|score| *score > 0.0) || ranking.values().any(|score| *score > 0.0);
        let has_interest_signal = inputs.interest.any();
        let regime = select_regime(&inputs.interest, has_public_signal);

        // 种子权重：按列表位置线性衰减。
        let seed_total = inputs.interest.recent_seed_ids.len();
        let seed_weights: HashMap<i64, f64> = inputs
            .interest
            .recent_seed_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, seed_weight(index, seed_total)))
            .collect();
        // 相似度分。**命中表为空就当没有这一路信号**（Qdrant 故障已在上游降级）。
        let similarity = if inputs.hits_by_seed.is_empty() {
            HashMap::new()
        } else {
            similarity_scores(&inputs.hits_by_seed, &seed_weights, &candidate_ids)
        };

        let mut scored: Vec<ScoredRecommendation> = Vec::with_capacity(inputs.movies.len());
        for movie in &inputs.movies {
            let signals = SignalScores {
                similarity: normalize(similarity.get(&movie.id).copied().unwrap_or(0.0)),
                subscribed_actor: f64::from(
                    inputs
                        .interest
                        .subscribed_actor_movie_ids
                        .contains(&movie.id),
                ),
                subscribed_movie: f64::from(movie.is_subscribed),
                heat: normalize(heat.get(&movie.id).copied().unwrap_or(0.0)),
                ranking: normalize(ranking.get(&movie.id).copied().unwrap_or(0.0)),
                freshness: normalize(freshness.get(&movie.id).copied().unwrap_or(0.0)),
            };
            let reason_codes = assign_reasons(&signals, regime, has_interest_signal);
            scored.push(ScoredRecommendation {
                movie: movie.clone(),
                score: score_one(&signals, regime),
                reason_codes,
                signals,
            });
        }
        sort_scored(&mut scored);
        let stats = ScoreStats {
            cold_start: !has_interest_signal,
            extreme_cold_start: regime == ScoreRegime::ExtremeColdStart,
            recent_seed_movies: seed_total as i64,
            candidate_movies: inputs.movies.len() as i64,
        };
        (scored, stats)
    }

    /// 生成并**整体替换**当前快照。上游 `generate_latest_snapshot`
    /// （`daily_recommendation_service.py:347-412`），调度任务
    /// `daily_recommendation_generate`。
    ///
    /// 1. 全库候选投影（[`MovieRepository::list_daily_candidates`]）；
    /// 2. 四路兴趣 / 公共信号 + Qdrant 相似度（**失败只跳过那一路**）；
    /// 3. [`Self::score_movies`] 打分排序，取前 `limit` 条；
    /// 4. [`DailyRecommendationItemRepository::replace_all`] —— 先清全表再按序
    ///    插入（**同一个事务**）。
    ///
    /// # 第 4 步为什么必须是「整体替换」
    ///
    /// `daily_recommendation_item` 的 `rank` 与 `movie_id` 都是**全表**唯一
    /// （不按 `snapshot_date` 分组）。逐条 upsert 会让昨天那批的残留与新一批
    /// 混在一起。详见该仓储的类型文档。
    ///
    /// # 相似度是 `Option<&MovieSimilarityStore>`，不是必需参数
    ///
    /// Qdrant 未启用时传 `None`，相似度信号全为 0、其余四路照常 —— 这与
    /// 「Qdrant 故障」走的是**同一条降级路径**（见 `Self::load_similarity_hits`），
    /// 所以不需要两套代码。
    ///
    /// # 进度上报比上游**粗**
    ///
    /// 上游在评分循环里每 `len/10` 条报一次（`:319-328`）。这里只报三个节点 ——
    /// [`Self::score_movies`] 是纯函数、不接受回调（那正是它能被完整测试的
    /// 原因），而 30 万次进度事件对客户端也没有价值。**节点与文案取自上游**，
    /// 只是少了中间那些「正在评分 N/M」。
    pub async fn generate_latest_snapshot(
        &self,
        target_date: Option<chrono::NaiveDate>,
        limit: i64,
        similarity: Option<&MovieSimilarityStore>,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<SnapshotStats, ServiceError> {
        let snapshot_date = snapshot_date(target_date);
        // 上游 `safe_limit = max(int(limit), 0)`：负数当成 0（一部都不存），
        // 而不是回落到默认值 —— 「显式要求 0 条」与「没给」是两件事。
        let safe_limit = limit.max(0);

        emit(
            &mut progress,
            Some(0),
            Some(0),
            "每日推荐快照生成 · 正在读取候选影片",
            None,
        )
        .await;
        let movies = self.load_candidate_movies().await?;
        tracing::info!(
            candidate_movies = movies.len(),
            snapshot_date = %snapshot_date,
            "每日推荐快照生成开始"
        );
        // `as i32` 安全：候选数上界是 `movie` 表行数，而主键就是 integer。
        let total = movies.len() as i32;
        let scoring_text = format!("每日推荐快照生成 · 候选 {} 部 · 正在评分", movies.len());
        let scoring_patch = serde_json::json!({ "candidate_movies": movies.len() });
        emit(
            &mut progress,
            Some(0),
            Some(total),
            &scoring_text,
            Some(&scoring_patch),
        )
        .await;

        let inputs = self.load_score_inputs(&movies, similarity).await?;
        let (scored, score_stats) = Self::score_movies(inputs);
        let ranked: Vec<ScoredRecommendation> =
            scored.into_iter().take(safe_limit as usize).collect();

        let stats = SnapshotStats {
            snapshot_date: snapshot_date.format("%Y-%m-%d").to_string(),
            candidate_movies: score_stats.candidate_movies,
            stored_items: ranked.len() as i64,
            cold_start: score_stats.cold_start,
            extreme_cold_start: score_stats.extreme_cold_start,
            recent_seed_movies: score_stats.recent_seed_movies,
        };
        let writing_text = format!("每日推荐快照生成 · 正在写入快照 · 入选 {} 部", ranked.len());
        let writing_patch = serde_json::to_value(&stats).ok();
        emit(
            &mut progress,
            Some(total),
            Some(total),
            &writing_text,
            writing_patch.as_ref(),
        )
        .await;

        let generated_at = now_utc();
        let rows: Vec<NewDailyRecommendation> = ranked
            .iter()
            .enumerate()
            .map(|(index, item)| NewDailyRecommendation {
                snapshot_date,
                // 候选 id 就来自 `movie.id`（integer），回写是同一类型。
                movie_id: item.movie.id as i32,
                // `rank` 从 1 开始，且**全表唯一** —— 见类型文档。
                rank: index as i32 + 1,
                score: item.score,
                reason_codes: Some(json_array(&item.reason_codes)),
                reason_texts: Some(json_array(&reason_texts(&item.reason_codes))),
                signal_scores: serde_json::to_string(&item.signals).ok(),
                generated_at,
            })
            .collect();

        DailyRecommendationItemRepository::new(self.db.clone())
            .replace_all(&rows)
            .await?;
        Ok(stats)
    }

    /// 全库候选投影 → 服务层 [`CandidateMovie`]。
    ///
    /// 只做**列名到字段名**的搬运，不含任何过滤 —— 「非集合、未拉黑」在 SQL 里。
    async fn load_candidate_movies(&self) -> Result<Vec<CandidateMovie>, ServiceError> {
        let rows = MovieRepository::new(self.db.clone())
            .list_daily_candidates()
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, heat, release_date, created_at, is_subscribed)| CandidateMovie {
                    id: i64::from(id),
                    heat: i64::from(heat),
                    release_date,
                    created_at,
                    is_subscribed,
                },
            )
            .collect())
    }

    /// 四路信号 + 相似度命中 → [`ScoreInputs`]。
    ///
    /// # 五条查询串行，与上游一致
    ///
    /// 上游 `_score_movies`（`:261-276`）也是串行的。并发化会把「哪一路先失败」
    /// 变成不确定，而这里两类失败的性质完全不同（DB 错误该整体失败；Qdrant
    /// 故障该降级）—— 混在一起只会更难判断。
    async fn load_score_inputs(
        &self,
        movies: &[CandidateMovie],
        similarity: Option<&MovieSimilarityStore>,
    ) -> Result<ScoreInputs, ServiceError> {
        // 候选 id 回不到 `i64` 之外的类型：它们是从 `movie.id`（integer）来的。
        let candidate_ids: Vec<i32> = movies.iter().map(|movie| movie.id as i32).collect();

        let recent_seed_ids = self.load_recent_seed_ids(&candidate_ids).await?;
        let subscribed_actor_movie_ids: HashSet<i64> = MovieActorRepository::new(self.db.clone())
            .list_with_subscribed_actor_in(&candidate_ids)
            .await?
            .into_iter()
            .map(i64::from)
            .collect();
        let ranking_rows: Vec<(i64, i64, String)> = RankingItemRepository::new(self.db.clone())
            .list_rank_rows_for_movies(&candidate_ids)
            .await?
            .into_iter()
            .map(|(movie_id, rank, period)| (i64::from(movie_id), i64::from(rank), period))
            .collect();
        // 订阅影片标记**已随候选投影取回**（`Movie.is_subscribed`）——
        // 不再查一次 30 万行范围（上游注释 `:271`）。
        let subscribed_movie_ids: HashSet<i64> = movies
            .iter()
            .filter(|movie| movie.is_subscribed)
            .map(|movie| movie.id)
            .collect();
        let hits_by_seed = self
            .load_similarity_hits(&recent_seed_ids, similarity)
            .await;

        Ok(ScoreInputs {
            movies: movies.to_vec(),
            interest: InterestSignals {
                recent_seed_ids,
                subscribed_actor_movie_ids,
                subscribed_movie_ids,
            },
            ranking_rows,
            hits_by_seed,
        })
    }

    /// 「最近播放」种子（最多 [`RECENT_SEED_LIMIT`] 个），按最近播放倒序。
    ///
    /// # 列表不存在时返回空表，**不创建**
    ///
    /// 上游 `Playlist.get_or_none(...)` 拿到 `None` 就 `return []`。新装实例
    /// 还没有这个系统列表，那是正常状态 —— 在定时任务的读路径上顺手建一个
    /// 会把「读路径带写」扩散到没人预期的地方。
    async fn load_recent_seed_ids(&self, candidate_ids: &[i32]) -> Result<Vec<i64>, ServiceError> {
        let Some(playlist) = PlaylistRepository::new(self.db.clone())
            .find_by_system_kind(PLAYLIST_KIND_RECENTLY_PLAYED)
            .await?
        else {
            return Ok(Vec::new());
        };
        let ids = PlaylistMovieRepository::new(self.db.clone())
            .list_recent_played_in(playlist.id, candidate_ids, RECENT_SEED_LIMIT)
            .await?;
        Ok(ids.into_iter().map(i64::from).collect())
    }

    /// Qdrant 相似度命中。**两类错误都降级成空表**（跳整路信号）。
    ///
    /// 上游捕获的是基类 `MovieSimilarityIndexError`（`:196-199`），它的两个
    /// 子类 ——「索引未就绪」与「服务不可用」—— 都落进 `return {}`。
    ///
    /// **与 `recommendation::search_similar_movies` 的差别是刻意的**：那边
    /// `NotReady` 要返 `Err`（影片详情页 503「重试有意义」），而每日推荐是一次
    /// **离线快照生成** —— 没有「让用户重试」这个选项，索引没建好时用冷启动
    /// 推荐是更好的结果。
    async fn load_similarity_hits(
        &self,
        seed_ids: &[i64],
        similarity: Option<&MovieSimilarityStore>,
    ) -> HashMap<i64, Vec<super::MovieSimilarityHit>> {
        let Some(store) = similarity else {
            return HashMap::new();
        };
        if seed_ids.is_empty() {
            return HashMap::new();
        }
        match store.search_many(seed_ids, SIMILARITY_PER_SEED_LIMIT).await {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(%error, "每日推荐跳过影片相似度信号");
                HashMap::new()
            }
        }
    }
}

/// 进度上报：`None` 就是「没人听」，直接跳过。
///
/// 与 `image_search_index::emit` 同形 —— 那边也是「有 sink 才上报，且
/// **上报失败不打断任务**」（进度是尽力而为的，它的失败不该让快照生成失败）。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: Option<i32>,
    total: Option<i32>,
    text: &str,
    patch: Option<&serde_json::Value>,
) {
    if let Some(sink) = progress {
        let _ = sink(current, total, text, patch).await;
    }
}

/// `Vec<String>` → JSON 数组文本。
///
/// **序列化不会失败**（`String` 没有非法状态），兜底只是为了避免 `unwrap`。
fn json_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一部候选。时间戳用 `base + 秒`，便于构造确定的排序关系。
    fn movie(id: i64, heat: i64, release_seconds: i64, created_seconds: i64) -> CandidateMovie {
        let base = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .expect("合法日期")
            .and_hms_opt(0, 0, 0)
            .expect("合法时刻");
        CandidateMovie {
            id,
            heat,
            release_date: Some(base + chrono::Duration::seconds(release_seconds)),
            created_at: Some(base + chrono::Duration::seconds(created_seconds)),
            is_subscribed: false,
        }
    }

    fn signals(freshness: f64) -> SignalScores {
        SignalScores {
            freshness,
            ..SignalScores::default()
        }
    }

    #[test]
    fn normalize_clamps_both_ends() {
        assert_eq!(normalize(-0.5), 0.0);
        assert_eq!(normalize(0.25), 0.25);
        assert_eq!(normalize(1.5), 1.0);
    }

    #[test]
    fn rank_decay_zeroes_out_of_window_and_dirty_ranks() {
        // 名次 0 / 负数 / 超出窗口都是脏数据 —— 必须得 0，不能拿满分。
        assert_eq!(rank_decay(0, 1.0), 0.0);
        assert_eq!(rank_decay(-3, 1.0), 0.0);
        assert_eq!(rank_decay(101, 1.0), 0.0);
        assert_eq!(rank_decay(1, 1.0), 1.0, "第 1 名拿满权重");
        // 第 51 名：(1 - 50/100) * 1.0
        assert!((rank_decay(51, 1.0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn period_weight_covers_every_upstream_key() {
        assert_eq!(period_weight("daily"), 1.0);
        assert_eq!(period_weight("weekly"), 0.7);
        assert_eq!(period_weight("monthly"), 0.4);
        // 空串是「总榜」，上游显式列了 0.7 —— 不是走默认分支。
        assert_eq!(period_weight(""), 0.7);
        // 大小写不敏感，未知周期 0.5。
        assert_eq!(period_weight("DAILY"), 1.0);
        assert_eq!(period_weight("yearly"), 0.5);
    }

    #[test]
    fn seed_weight_decays_linearly_but_never_reaches_zero() {
        assert_eq!(seed_weight(0, 3), 1.0);
        assert!((seed_weight(2, 3) - (1.0 - 2.0 / 3.0)).abs() < f64::EPSILON);
        // 最后一个不是 0：`index / max(total, 1)` 恒小于 1。
        assert!(seed_weight(2, 3) > 0.0);
    }

    #[test]
    fn heat_scores_use_the_95th_percentile_as_reference() {
        // 99 部热度 1、1 部热度 1000：参考值取 95 分位（= 1），不取最大值。
        let mut movies: Vec<CandidateMovie> = (1..=99).map(|id| movie(id, 1, 0, 0)).collect();
        movies.push(movie(100, 1000, 0, 0));
        let scores = heat_scores(&movies);
        assert_eq!(scores[&100], 1.0, "爆款被截到 1.0");
        assert_eq!(scores[&1], 1.0, "分位参考值就是 1");
    }

    #[test]
    fn heat_scores_are_empty_when_nobody_has_heat() {
        let movies = vec![movie(1, 0, 0, 0), movie(2, -5, 0, 0)];
        assert!(heat_scores(&movies).is_empty());
    }

    #[test]
    fn freshness_gives_a_lone_movie_full_marks() {
        // 只有一部时分母会退化成 0 —— 不特判它就排不到前面。
        let scores = freshness_scores(&[movie(7, 0, 0, 0)]);
        assert_eq!(scores[&7], 1.0);
    }

    #[test]
    fn freshness_orders_by_release_then_created_then_id() {
        // 发行日最新者 1.0；同日比入库时间；再同则比 id（大的在前）。
        let movies = vec![
            movie(1, 0, 300, 0),
            movie(2, 0, 200, 100),
            movie(3, 0, 200, 100),
        ];
        let scores = freshness_scores(&movies);
        assert_eq!(scores[&1], 1.0);
        // 2 与 3 的发行日、入库时间都相同 -> id 大的在前，freshness 更高。
        assert!(scores[&3] > scores[&2]);
    }

    #[test]
    fn ranking_scores_take_the_max_across_periods() {
        // 同一影片挂两个榜：取衰减后的最大值，**不是累加**。
        let rows = vec![
            (1_i64, 1_i64, "daily".to_owned()),
            (1, 1, "monthly".to_owned()),
            (2, 101, "daily".to_owned()),
        ];
        let scores = ranking_scores(&rows);
        assert_eq!(scores[&1], 1.0, "daily 第 1 名胜过 monthly 第 1 名");
        assert_eq!(scores[&2], 0.0, "超出衰减窗口得 0");
    }

    #[test]
    fn similarity_keeps_only_candidates_and_scales_by_seed_weight() {
        let hits = HashMap::from([(
            10_i64,
            vec![
                crate::discovery::MovieSimilarityHit {
                    movie_id: 20,
                    score: 0.8,
                },
                // 不在候选集里 —— 必须被丢掉。
                crate::discovery::MovieSimilarityHit {
                    movie_id: 99,
                    score: 1.0,
                },
            ],
        )]);
        let weights = HashMap::from([(10_i64, 0.5_f64)]);
        let candidates: HashSet<i64> = [20_i64].into_iter().collect();

        let scores = similarity_scores(&hits, &weights, &candidates);

        assert_eq!(scores.len(), 1, "非候选的命中被丢弃");
        // 容差放宽到 1e-6：`hit.score` 是 `f32`（Qdrant 侧的相似度），
        // 转 `f64` 会带上单精度尾数噪声。
        assert!((scores[&20] - 0.4).abs() < 1e-6, "0.8 × 0.5");
    }

    #[test]
    fn select_regime_prefers_regular_then_cold_then_extreme() {
        let interest = InterestSignals {
            recent_seed_ids: vec![1],
            ..InterestSignals::default()
        };
        assert_eq!(select_regime(&interest, false), ScoreRegime::Regular);

        let none = InterestSignals::default();
        assert_eq!(select_regime(&none, true), ScoreRegime::ColdStart);
        assert_eq!(select_regime(&none, false), ScoreRegime::ExtremeColdStart);
    }

    #[test]
    fn extreme_cold_start_scores_on_freshness_alone() {
        // 极冷启动**不走** COLD_START_WEIGHTS —— 只用 freshness。
        assert_eq!(score_one(&signals(0.5), ScoreRegime::ExtremeColdStart), 0.5);
    }

    #[test]
    fn cold_start_weights_sum_to_one() {
        let all = SignalScores {
            heat: 1.0,
            ranking: 1.0,
            freshness: 1.0,
            ..SignalScores::default()
        };
        let score = score_one(&all, ScoreRegime::ColdStart);
        assert!((score - 1.0).abs() < 1e-9, "11/18 + 5/18 + 2/18 = 1");
    }

    #[test]
    fn new_release_needs_no_interest_signal() {
        // 有用户兴趣时**不给** new_release —— 那不是用户选择它的原因。
        let with_interest = assign_reasons(&signals(0.9), ScoreRegime::Regular, true);
        assert!(!with_interest.iter().any(|code| code == reason::NEW_RELEASE));

        // 无兴趣信号且有新鲜度 -> 给。
        let cold = assign_reasons(&signals(0.9), ScoreRegime::ColdStart, false);
        assert!(cold.iter().any(|code| code == reason::NEW_RELEASE));
    }

    #[test]
    fn reason_texts_drop_unknown_codes() {
        let texts = reason_texts(&[
            reason::POPULAR_MOVIE.to_owned(),
            "not_a_real_code".to_owned(),
        ]);
        assert_eq!(texts, vec!["近期热度较高".to_owned()]);
    }

    #[test]
    fn sorting_is_total_and_descending_on_every_key() {
        let scored = |id: i64, score: f64, release: i64, created: i64| ScoredRecommendation {
            movie: movie(id, 0, release, created),
            score,
            reason_codes: Vec::new(),
            signals: SignalScores::default(),
        };
        let mut items = vec![
            scored(1, 0.5, 0, 0),
            scored(2, 0.9, 0, 0),
            // 与 id=2 同分同发行日，但入库更晚 -> 排在前面。
            scored(3, 0.9, 0, 100),
            scored(4, 0.9, 100, 0),
        ];
        sort_scored(&mut items);
        assert_eq!(
            items.iter().map(|item| item.movie.id).collect::<Vec<_>>(),
            vec![4, 3, 2, 1],
            "分数 -> 发行日 -> 入库时间 -> id，全降序"
        );
    }

    #[test]
    fn snapshot_date_passes_through_and_defaults_to_local_today() {
        let given = chrono::NaiveDate::from_ymd_opt(2020, 6, 15).expect("合法日期");
        assert_eq!(snapshot_date(Some(given)), given);
        assert_eq!(
            snapshot_date(None),
            chrono::Local::now().date_naive(),
            "缺省取**本地**今天，不是 UTC"
        );
    }
}
