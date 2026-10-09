//! 影片热度重算（上游 `catalog/movie_heat_service.py`，70 行）。
//!
//! # 公式：固定 P99 参考值的**线性累计**
//!
//! ```text
//!   heat = ROUND(HEAT_SCALE * ( 7/34 * watched/WATCHED_REF
//!                             + 5/34 * want_watch/WANT_REF
//!                             +17/34 * comment/COMMENT_REF
//!                             + 5/34 * score_number/SCORE_REF ))
//! ```
//!
//! # ⚠️ **不设上限、不 clamp** —— 这是刻意的
//!
//! 上游注释（`:20`、`:27`）：
//!
//! > 按固定 P99 参考值做线性累计，**保留头部原始计数差异，不设置热度上限**。
//! > `HEAT_SCALE` 只是 P99 附近的**展示基准**，参考值以上继续线性增长。
//!
//! 我第一版骨架在这里写了 `clamp(0, 1)` 与「截到满值」—— **那是错的**，
//! 而且错得隐蔽：clamp 会让上百部爆款影片的热度**完全相同**，
//! 「保留头部差异」这个设计意图被抹掉，而头部差异正是热度排序的主要信息。
//!
//! 负数输入会算出负热度（上游也不防）—— `heat` 列没有 `CHECK (heat >= 0)`。
//! 照抄，别加 clamp。
//!
//! # 参考值是 **P99**，写死不再变
//!
//! 上游注释（`:11`）：「参考值取当前业务库各互动字段的 P99，**固定后**避免
//! 全库分布变化导致**历史热度漂移**」。
//!
//! 这与 `discovery::daily_recommendation` 里的 95 分位**不同**（那里每次现算
//! 分位）。两套做法别混用。
//!
//! # 更新是**增量**的：`WHERE heat != computed`
//!
//! 上游 `:33` / `:40` / `:48` 三处都有。所以：
//!
//! | 返回值 | 含义 |
//! |---|---|
//! | `candidate_count` | 热度与公式**不一致**的影片数（不是全库行数） |
//! | `updated_count` | 实际被 UPDATE 的行数 |
//!
//! 少了这个条件，每次全表重算都会更新 30 万行、全部刷新 `updated_at`。
//! 两个计数**语义不同**：前者在事务内先数（用于日志），后者是执行结果，
//! 并发写入时两者会差。

use crate::error::ServiceError;

/// 公式版本。**改公式必须改它**，否则无法区分新旧公式算出的热度。
pub const FORMULA_VERSION: &str = "v6";

/// 观看数 P99 参考值。
pub const WATCHED_COUNT_REFERENCE: f64 = 1308.0;
/// 想看数 P99 参考值。
pub const WANT_WATCH_COUNT_REFERENCE: f64 = 4991.0;
/// 评论数 P99 参考值。
pub const COMMENT_COUNT_REFERENCE: f64 = 41.0;
/// 评分人数 P99 参考值。
pub const SCORE_NUMBER_REFERENCE: f64 = 6291.0;
/// 展示基准刻度。**不是上限**（见模块文档）。
pub const HEAT_SCALE: f64 = 3100.0;

/// 四项权重。分母刻意用 **34**（不归一到 1.0）。
///
/// 分母 34 而分子之和 34 的「差一点」是刻意的：四项**全部达到 P99** 时的和
/// 略大于 1，正好对应「P99 附近即满刻度」。
pub mod weights {
    /// 观看数权重 `7/34` —— 四项里最低。
    pub const WATCHED: f64 = 7.0 / 34.0;
    /// 想看数权重 `5/34`。
    pub const WANT_WATCH: f64 = 5.0 / 34.0;
    /// 评论数权重 `17/34` —— **绝对主导**（占一半）。
    pub const COMMENT: f64 = 17.0 / 34.0;
    /// 评分人数权重 `5/34`。
    pub const SCORE_NUMBER: f64 = 5.0 / 34.0;
}

/// 一部影片的重算输入。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HeatInput {
    pub watched_count: i64,
    pub want_watch_count: i64,
    pub comment_count: i64,
    pub score_number: i64,
}

/// ★ 单部影片的热度。**纯函数** —— 必须与 SQL 的 `build_heat_expression`
/// 给出**逐行相同**的值，否则「单部重算」与「全表重算」会让热度在两个入口
/// 之间跳变，且 `WHERE heat != computed` 会永远判定它们不一致。
///
/// `fn.ROUND` 是 PostgreSQL 的四舍五入（**远离零**），Rust 的 `.round()`
/// 语义相同。用 `.floor()` 会变成向零取整，与 SQL 不一致。
pub fn heat_of(input: HeatInput) -> i64 {
    use weights::{COMMENT, SCORE_NUMBER, WANT_WATCH, WATCHED};
    let normalized = WATCHED * (input.watched_count as f64 / WATCHED_COUNT_REFERENCE)
        + WANT_WATCH * (input.want_watch_count as f64 / WANT_WATCH_COUNT_REFERENCE)
        + COMMENT * (input.comment_count as f64 / COMMENT_COUNT_REFERENCE)
        + SCORE_NUMBER * (input.score_number as f64 / SCORE_NUMBER_REFERENCE);
    (normalized * HEAT_SCALE).round() as i64
}

/// 全表重算结果。**键名与上游逐字一致**（会进任务摘要）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MovieHeatUpdateResult {
    /// 热度与公式不一致的影片数（事务内先数）。
    pub candidate_count: i64,
    /// 实际更新的行数。
    pub updated_count: i64,
    /// 公式版本。
    pub formula_version: &'static str,
}

/// 热度服务。
pub struct MovieHeatService;

impl MovieHeatService {
    /// ★ 全表重算。
    ///
    /// 上游在 `database.atomic()` 里**先数后更**。保持这个顺序：反过来会得到
    /// `candidate_count = 0`（因为已经全都一致了）。
    pub async fn update_movie_heat() -> Result<MovieHeatUpdateResult, ServiceError> {
        todo!("骨架：事务内先 SELECT COUNT(*) WHERE heat != <公式>，再 UPDATE 同一条件")
    }

    /// 单部影片重算。返回**实际更新行数**（`0` = 影片不存在**或**热度已对）。
    ///
    /// 两种情况都返回 0，上游**不区分** —— 照抄，别加「不存在就 404」。
    /// 手动重算的语义是「确保它是对的」，已经对时返回 0 是正确结果。
    pub async fn update_single_movie_heat(movie_id: i64) -> Result<u64, ServiceError> {
        let _ = movie_id;
        todo!("骨架：UPDATE movie SET heat = <公式> WHERE id = $1 AND heat != <公式>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全零 → 0。
    #[test]
    fn a_movie_with_no_signals_has_no_heat() {
        assert_eq!(heat_of(HeatInput::default()), 0);
    }

    /// ★ 超出 P99 的信号**必须继续线性增长**，不得截断。
    ///
    /// 这是本文件最重要的一条：clamp 会让所有爆款影片热度相同，
    /// 「保留头部原始计数差异」的设计意图就没了。
    #[test]
    fn signals_beyond_p99_keep_growing() {
        let at_p99 = HeatInput {
            watched_count: 1308,
            want_watch_count: 4991,
            comment_count: 41,
            score_number: 6291,
        };
        let ten_x = HeatInput {
            watched_count: 13080,
            want_watch_count: 49910,
            comment_count: 410,
            score_number: 62910,
        };
        let base = heat_of(at_p99);
        let scaled = heat_of(ten_x);
        assert!(scaled > base, "10 倍信号应有更高热度：{base} -> {scaled}");
        assert!((scaled - base * 10).abs() <= 2, "应约为 10 倍");
    }

    /// ★ 评论数是**主导项**（17/34，占一半）。
    ///
    /// 搞反权重会完全改变热度排序，而排序直接决定所有列表的呈现。
    ///
    /// `assertions_on_constants` 在这里是**误报**：两侧都是 `const`，编译期
    /// 就能定值 —— 而这正是本条用例的目的（把权重比例钉死，谁改动谁红）。
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn comments_dominate() {
        use weights::*;
        assert!((COMMENT - 0.5).abs() < 1e-9, "评论权重就是 0.5");
        assert!(COMMENT > WATCHED && COMMENT > WANT_WATCH && COMMENT > SCORE_NUMBER);
    }

    /// 观看数权重最低 —— 权重顺序照抄 7 < 5 < 17 < 5 里的相对关系。
    ///
    /// 理由同 [`comments_dominate`]：断言常量是刻意的。
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn watched_count_has_the_smallest_weight() {
        use weights::*;
        assert!(WATCHED < WANT_WATCH);
        assert!(WATCHED < SCORE_NUMBER);
        // 想看与评分人数**同权重**（都是 5/34）。
        assert!((WANT_WATCH - SCORE_NUMBER).abs() < 1e-12);
    }

    /// 单一信号的热度可预测：只有评论数时约为 `0.5 * HEAT_SCALE`。
    #[test]
    fn a_single_signal_scales_by_its_weight() {
        let only_comments = HeatInput {
            comment_count: 41,
            ..HeatInput::default()
        };
        let expected = (0.5 * HEAT_SCALE).round() as i64;
        assert_eq!(heat_of(only_comments), expected);
    }

    /// 四项全 P99 时落在 `HEAT_SCALE` 附近。
    #[test]
    fn all_signals_at_p99_land_near_the_scale() {
        let at_p99 = HeatInput {
            watched_count: 1308,
            want_watch_count: 4991,
            comment_count: 41,
            score_number: 6291,
        };
        let heat = heat_of(at_p99);
        // 权重和 34/34 = 1 -> 恰好 HEAT_SCALE。
        assert_eq!(heat, HEAT_SCALE as i64);
    }

    /// 负数输入**不**被夹到 0（上游也不防，照抄）。
    ///
    /// 这里断言的是「不要偷偷加 clamp」—— 一旦有人为了「数据更干净」加上
    /// clamp，这条测试会失败并提醒他那是契约变更。
    #[test]
    fn negative_inputs_are_not_clamped_even_though_it_looks_dirty() {
        let dirty = HeatInput {
            watched_count: -100,
            ..HeatInput::default()
        };
        assert!(heat_of(dirty) < 0, "上游不防负数，别加 clamp");
    }

    /// 公式版本是**非空常量**。
    #[test]
    fn the_formula_version_is_pinned() {
        assert_eq!(FORMULA_VERSION, "v6");
    }
}
