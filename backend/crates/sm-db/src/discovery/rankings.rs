//! `discovery` 域的推荐榜单模型（3 张表）。
//!
//! 对应 `src/model/discovery/rankings.py`、`daily_recommendations.py`、
//! `moment_recommendations.py`。
//!
//! # `rank` 的唯一索引是「表内全局」的，不是「按快照分组」
//!
//! `daily_recommendation_item.rank` 与 `moment_recommendation.rank` 都是
//! `unique=True`，而 `daily_recommendation_item` 同时有 `snapshot_date`。
//!
//! 两者结合意味着：**第二天生成的 rank=1 会与第一天的 rank=1 冲突**。
//! 所以这张表不是「累积历史」，而是**每次生成前清空重写**。
//! 迁移 service 层时必须确认这个清空动作，否则第二批写入就会撞唯一约束。
//! `snapshot_date` 记录的是「这一批是哪天的」，不是历史维度。
//!
//! 如果实际行为是累积保留，那 schema 与 service 就不一致，需要先查清。

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::FromRow;

/// `ranking_item` 表：来自外部榜单的排名条目。
#[derive(Debug, Clone, FromRow)]
pub struct RankingItem {
    pub id: i32,
    /// 数据源标识（如 `javdb`）。
    pub source_key: String,
    /// 榜单标识。
    pub board_key: String,
    /// 周期。**默认空串，不是 NULL** —— 空串代表不限定周期（如总榜）。
    pub period: String,
    /// 名次，从 1 开始。
    pub rank: i32,
    /// 影片番号。
    ///
    /// 与 `movie_id` 冗余并存：查询番号用这一列，走索引且不必 join。
    pub movie_number: String,
    /// 指向 `Movie`（JAV 影片）的 `id`。**NOT NULL。**
    ///
    /// 上游是
    /// `movie = ForeignKeyField(Movie, backref="ranking_items", on_delete="CASCADE")`，
    /// 没有 `null=True`，所以这一列不可空。
    ///
    /// 此前这里是 `Option<i32>`，注释写着「榜单数据先于刮削入库是常态，
    /// 影片未入库时为空，入库后回填」——**那个设计在数据库层面不成立**：
    /// 列是 NOT NULL，第一次插入就必须带上已存在的 `movie_id`，没有
    /// 「先插行、以后再回填」这种中间状态。按注释实现会直接撞约束。
    ///
    /// 榜单数据确实可能先于刮削到达，但那时正确的做法是**先落 movie 行**
    /// （哪怕是只有番号的占位行），再插 ranking_item —— 冗余的
    /// `movie_number` 让这个占位成本很低。
    pub movie_id: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl RankingItem {
    /// 榜单身份（不含名次）。
    ///
    /// 复合唯一索引是 `(source_key, board_key, period, rank)`，
    /// 而 `(source_key, board_key, period)` 另有普通索引服务列表查询 ——
    /// 两者都在，`board_identity` 正好对应后者的前缀。
    pub fn board_identity(&self) -> (&str, &str, &str) {
        (&self.source_key, &self.board_key, &self.period)
    }

    /// 是否为不限定周期的总榜。
    pub fn is_all_time(&self) -> bool {
        self.period.is_empty()
    }
}

/// `daily_recommendation_item` 表：每日推荐。
///
/// **每天一批，生成前清空** —— 见模块文档关于 `rank` 唯一索引的说明。
#[derive(Debug, Clone, FromRow)]
pub struct DailyRecommendationItem {
    pub id: i32,
    /// 快照日期。**`DateField` 而非 `DateTimeField`**，无时间部分。
    pub snapshot_date: NaiveDate,
    /// 指向 `Movie`（JAV 影片）。**唯一** —— 一部电影在表里至多一条。
    ///
    /// 注意这个唯一性是全表的，不按 `snapshot_date` 分组。
    pub movie_id: i32,
    /// 名次，**全表唯一**（见模块文档）。
    pub rank: i32,
    /// 综合得分。
    pub score: f64,
    /// 理由代码数组，JSON 文本。`JsonTextField`，默认 `[]`。
    pub reason_codes: Option<String>,
    /// 理由文案数组，JSON 文本。默认 `[]`。
    pub reason_texts: Option<String>,
    /// 各信号分量得分，JSON 对象。默认 `{}`。
    pub signal_scores: Option<String>,
    /// 生成时刻。
    pub generated_at: NaiveDateTime,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl DailyRecommendationItem {
    /// 解析 `reason_codes` 数组。
    pub fn parsed_reason_codes(&self) -> Option<Vec<String>> {
        parse_json_array(self.reason_codes.as_deref())
    }

    /// 解析 `reason_texts` 数组。
    pub fn parsed_reason_texts(&self) -> Option<Vec<String>> {
        parse_json_array(self.reason_texts.as_deref())
    }

    /// 解析 `signal_scores` 对象。
    pub fn parsed_signal_scores(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let raw = self.signal_scores.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }

    /// 理由代码与文案是否数量一致。
    ///
    /// 两者都是数组但无外键约束，长度不一致说明生成侧有 bug ——
    /// 客户端按下标配对展示，数量不符会导致部分文案缺失。
    pub fn reason_arity_mismatch(&self) -> bool {
        match (self.parsed_reason_codes(), self.parsed_reason_texts()) {
            (Some(codes), Some(texts)) => codes.len() != texts.len(),
            _ => false,
        }
    }
}

/// 解析 `JsonTextField` 的数组内容。空串与非法 JSON 都视为 `None`。
fn parse_json_array(raw: Option<&str>) -> Option<Vec<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str(raw).ok()
}

/// 时刻推荐的策略标识。
pub mod moment_strategy {
    pub const ALL: [&str; 0] = [];
}

/// `moment_recommendation` 表：时刻（片段级）推荐。
///
/// 与 `DailyRecommendationItem` 的影片级推荐不同，这里推荐的是
/// **某个影片的某个时刻**，因此额外携带 `media` / `thumbnail` /
/// `offset_seconds`。
///
/// 三个 `seed_*` / `source_movie` 字段是「推荐依据」，删除时一律 SET NULL：
/// 依据没了推荐仍然成立，只是失去可解释性。
#[derive(Debug, Clone, FromRow)]
pub struct MomentRecommendation {
    pub id: i32,
    /// 名次，**全表唯一**（与 `daily_recommendation_item.rank` 同理）。
    pub rank: i32,
    /// 综合得分。`f64` 对应 DDL 的 `double precision`；用 f32 会读不出来。
    pub score: f64,
    /// 推荐策略标识，有索引。
    pub strategy: String,
    /// 可读的推荐理由。
    pub reason: String,
    /// 目标影片。
    pub movie_id: i32,
    /// 目标媒体文件。
    pub media_id: i32,
    /// 目标缩略图（**唯一** —— 一个缩略图至多被推荐一次）。
    pub thumbnail_id: i32,
    /// 距片头的秒数。
    pub offset_seconds: i32,
    /// 种子时刻点，即「从这个时刻出发找相似内容」。
    pub seed_point_id: Option<i32>,
    /// 种子缩略图（视觉检索路径）。
    pub seed_thumbnail_id: Option<i32>,
    /// 种子来源影片（影片相似度路径）。
    pub source_movie_id: Option<i32>,
    /// 视觉相似度分量得分。无视觉依据时为空。
    pub visual_score: Option<f64>,
    /// 影片相似度分量得分。无影片依据时为空。
    pub movie_similarity_score: Option<f64>,
    pub generated_at: NaiveDateTime,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl MomentRecommendation {
    /// 该推荐是否有可解释依据。
    ///
    /// 三个 seed 字段全空意味着无法回答「为什么推荐这个」，
    /// 但推荐本身仍然有效，不应被丢弃。
    pub fn has_seed(&self) -> bool {
        self.seed_point_id.is_some()
            || self.seed_thumbnail_id.is_some()
            || self.source_movie_id.is_some()
    }

    /// 推荐所依据的检索路径。
    pub fn seed_kind(&self) -> MomentSeedKind {
        match (
            self.seed_thumbnail_id.is_some(),
            self.source_movie_id.is_some(),
        ) {
            (true, true) => MomentSeedKind::Both,
            (true, false) => MomentSeedKind::Thumbnail,
            (false, true) => MomentSeedKind::Movie,
            (false, false) => MomentSeedKind::None,
        }
    }

    /// 相似度分量是否齐备。
    pub fn has_both_scores(&self) -> bool {
        self.visual_score.is_some() && self.movie_similarity_score.is_some()
    }
}

/// 时刻推荐的种子来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentSeedKind {
    /// 无种子 —— 推荐不可解释。
    None,
    /// 仅缩略图种子（视觉检索）。
    Thumbnail,
    /// 仅影片种子（影片相似度）。
    Movie,
    /// 两种种子都有。
    Both,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ranking(period: &str, movie_id: i32) -> RankingItem {
        RankingItem {
            id: 1,
            source_key: "javdb".to_owned(),
            board_key: "popular".to_owned(),
            period: period.to_owned(),
            rank: 1,
            movie_number: "ABC-001".to_owned(),
            movie_id,
            created_at: None,
            updated_at: None,
        }
    }

    fn daily(rank: i32, codes: &str, texts: &str) -> DailyRecommendationItem {
        DailyRecommendationItem {
            id: 1,
            snapshot_date: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            movie_id: 1,
            rank,
            score: 0.9,
            reason_codes: Some(codes.to_owned()),
            reason_texts: Some(texts.to_owned()),
            signal_scores: Some(r#"{"popularity":0.7}"#.to_owned()),
            generated_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
                .unwrap()
                .and_hms_opt(3, 0, 0)
                .unwrap(),
            created_at: None,
            updated_at: None,
        }
    }

    fn moment(seed_thumb: Option<i32>, seed_movie: Option<i32>) -> MomentRecommendation {
        MomentRecommendation {
            id: 1,
            rank: 1,
            score: 0.8,
            strategy: "visual".to_owned(),
            reason: "similar".to_owned(),
            movie_id: 1,
            media_id: 1,
            thumbnail_id: 9,
            offset_seconds: 120,
            seed_point_id: None,
            seed_thumbnail_id: seed_thumb,
            source_movie_id: seed_movie,
            visual_score: Some(0.7),
            movie_similarity_score: seed_movie.map(|_| 0.5),
            generated_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
                .unwrap()
                .and_hms_opt(3, 0, 0)
                .unwrap(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn period_defaults_to_empty_string_not_null() {
        // period 是 CharField(default="")，空串代表不限定周期。
        assert!(ranking("", 1).is_all_time());
        assert!(!ranking("2026-10", 1).is_all_time());
    }

    #[test]
    fn board_identity_serves_the_list_query_index() {
        // 曾经存在一个 `is_linked()`，断言「movie_id 可以先空着、
        // 影片入库后再回填」。那个前提是错的：`movie` 是
        // `ForeignKeyField(Movie, on_delete="CASCADE")`，没有 `null=True`，
        // 列是 NOT NULL，不存在「先插行、以后回填」的中间状态。
        //
        // 榜单先于刮削到达时，正确做法是先落 movie 占位行再插
        // ranking_item —— 冗余的 movie_number 让这个成本很低。
        let item = ranking("2026-10", 7);
        assert_eq!(item.movie_id, 7, "外键必带值");
        assert_eq!(item.movie_number, "ABC-001", "冗余番号仍用于免 join 查询");
        assert_eq!(
            item.board_identity(),
            ("javdb", "popular", "2026-10"),
            "board_identity 对应 (source_key, board_key, period) 那个普通索引"
        );
    }

    #[test]
    fn daily_reason_arrays_should_match_arity() {
        let ok = daily(1, r#"["a","b"]"#, r#"["甲","乙"]"#);
        assert_eq!(ok.parsed_reason_codes().unwrap().len(), 2);
        assert_eq!(ok.parsed_reason_texts().unwrap().len(), 2);
        assert!(!ok.reason_arity_mismatch());

        let bad = daily(2, r#"["a","b","c"]"#, r#"["甲"]"#);
        assert!(
            bad.reason_arity_mismatch(),
            "客户端按下标配对展示，长度不符会导致文案缺失"
        );
    }

    #[test]
    fn daily_parses_signal_scores_object() {
        let d = daily(1, "[]", "[]");
        let sig = d.parsed_signal_scores().unwrap();
        assert_eq!(sig["popularity"], 0.7);
    }

    #[test]
    fn moment_seed_kind_reflects_retrieval_path() {
        assert_eq!(moment(None, None).seed_kind(), MomentSeedKind::None);
        assert!(!moment(None, None).has_seed());
        assert_eq!(moment(Some(3), None).seed_kind(), MomentSeedKind::Thumbnail);
        assert_eq!(moment(None, Some(4)).seed_kind(), MomentSeedKind::Movie);
        assert_eq!(moment(Some(3), Some(4)).seed_kind(), MomentSeedKind::Both);

        // 无 seed 时推荐仍成立，只是不可解释 —— 不应据此丢弃。
        let bare = moment(None, None);
        assert!(!bare.has_both_scores());
    }
}
