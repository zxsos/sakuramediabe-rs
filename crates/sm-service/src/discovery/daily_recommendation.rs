//! 每日推荐（上游 `daily_recommendation_service.py`，18.7KB / 476 行）。
//!
//! 依赖：[`super::recommendation`]（稀疏相似度）+ [`super::ranking`] **读侧**。
//! **不依赖 provider 插件** —— 榜单分只用 `ranking_item` 表，与插件写侧无关。
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
    codes.iter().filter_map(|code| reason_text(code)).map(str::to_owned).collect()
}

/// 候选影片的轻量投影。**只 5 个字段**（见模块文档）。
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateMovie {
    pub id: i64,
    /// 热度。库里可能为 `NULL` -> 按 0 算。
    pub heat: Option<i64>,
    pub release_date: Option<chrono::NaiveDate>,
    pub created_at: Option<chrono::NaiveDateTime>,
    /// 是否已订阅。**随候选一起查**（上游注释 `:275`：无需再查 30 万行范围）。
    pub is_subscribed: bool,
}

/// 一部影片的六个信号分（各自已归一到 [0, 1]）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
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
    let mut positive: Vec<i64> = movies.iter().filter_map(|m| m.heat).filter(|h| *h > 0).collect();
    if positive.is_empty() {
        return HashMap::new();
    }
    positive.sort_unstable();
    // 95 分位：`ceil(n * 0.95) - 1`（Python 的 `math.ceil` 是向上取整）。
    let rank = ((positive.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    let reference = (positive[rank.min(positive.len() - 1)] as f64).max(1.0);
    movies
        .iter()
        .map(|movie| (movie.id, normalize(movie.heat.unwrap_or(0) as f64 / reference)))
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
    hits_by_seed: &HashMap<i64, Vec<super::recommendation::MovieSimilarityHit>>,
    seed_weights: &HashMap<i64, f64>,
    candidate_ids: &HashSet<i64>,
) -> HashMap<i64, f64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (seed_id, hits) in hits_by_seed {
        let Some(&seed_weight) = seed_weights.get(seed_id) else { continue };
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
#[derive(Debug, Clone, Copy, Default)]
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
/// 进度上报（签名与 `image_search` 那套一致）。
pub type ProgressSink<'a> = Box<
    dyn FnMut(Option<i32>, Option<i32>, &str, Option<&serde_json::Value>) -> BoxFuture<'a, Result<(), String>>
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

/// 单条每日推荐（响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRecommendationItem {
    pub movie_id: i64,
    pub title: Option<String>,
    pub poster_url: Option<String>,
    pub score: f64,
    /// **已由 `reason_texts` 从理由码翻出的文案**（表外的码不产出条目）。
    pub reasons: Vec<String>,
    /// 理由码。与 `reasons` **一一对应**（同一个过滤后的列表）。
    pub reason_codes: Vec<String>,
}

/// 分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRecommendationPage {
    pub items: Vec<DailyRecommendationItem>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    /// 快照日期。**分页要带日期** —— 否则跨零点翻页会混到两天的快照。
    pub snapshot_date: Option<String>,
}

/// 打分需要的外部输入。**与 DB 无关**，便于直接测 `score_movies`。
pub struct ScoreInputs {
    pub movies: Vec<CandidateMovie>,
    pub interest: InterestSignals,
    /// 每部候选的榜单行 `(movie_id, rank, period)`。
    pub ranking_rows: Vec<(i64, i64, String)>,
    /// 每个种子的相似影片命中。**空表 = 相似度信号缺失**（Qdrant 故障已降级）。
    pub hits_by_seed: HashMap<i64, Vec<super::recommendation::MovieSimilarityHit>>,
}

impl DailyRecommendationService {
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
        let has_public_signal = heat.values().any(|score| *score > 0.0)
            || ranking.values().any(|score| *score > 0.0);
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
                    inputs.interest.subscribed_actor_movie_ids.contains(&movie.id),
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
}