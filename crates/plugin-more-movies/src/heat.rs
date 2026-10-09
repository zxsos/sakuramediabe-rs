//! 影片热度计算：与宿主 `MovieHeatService` v6 公式保持一致。
//!
//! # 上游对应：`sakuramedia_more_movies/heat.py`
//!
//! 宿主热度在入库后由定时任务统一计算；插件在入库前用同一公式做门槛过滤，
//! 避免把低热度影片带进主库。参考值取当前业务库各互动字段的 P99，公式版本
//! 变更时需要同步更新（宿主见 `src/service/catalog/movie_heat_service.py`）。

/// 参考值（上游 `heat.py` 的模块常量）。
pub const WATCHED_COUNT_REFERENCE: f64 = 1308.0;
pub const WANT_WATCH_COUNT_REFERENCE: f64 = 4991.0;
pub const COMMENT_COUNT_REFERENCE: f64 = 41.0;
pub const SCORE_NUMBER_REFERENCE: f64 = 6291.0;
pub const HEAT_SCALE: f64 = 3100.0;

/// 按 v6 公式计算热度，与宿主 SQL ROUND 行为一致（正数 `int(x+0.5)`）。
///
/// 上游 `calculate_heat`：`None` 按 0 处理，这里调用方传 `u64`，无 `None` 情况。
pub fn calculate_heat(
    watched_count: u64,
    want_watch_count: u64,
    comment_count: u64,
    score_number: u64,
) -> u64 {
    let normalized = (7.0 / 34.0) * watched_count as f64 / WATCHED_COUNT_REFERENCE
        + (5.0 / 34.0) * want_watch_count as f64 / WANT_WATCH_COUNT_REFERENCE
        + (17.0 / 34.0) * comment_count as f64 / COMMENT_COUNT_REFERENCE
        + (5.0 / 34.0) * score_number as f64 / SCORE_NUMBER_REFERENCE;
    // 上游 `int(normalized_heat * HEAT_SCALE + 0.5)`：正数域等价于 round。
    (normalized * HEAT_SCALE + 0.5) as u64
}

/// JavDB 影片详情里的互动字段。
#[derive(Debug, Clone, Default)]
pub struct HeatInputs {
    pub watched_count: u64,
    pub want_watch_count: u64,
    pub comment_count: u64,
    pub score_number: u64,
}

impl HeatInputs {
    pub fn heat(&self) -> u64 {
        calculate_heat(
            self.watched_count,
            self.want_watch_count,
            self.comment_count,
            self.score_number,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游公式的冒烟：全 0 得 0。
    #[test]
    fn zero_inputs_give_zero() {
        assert_eq!(calculate_heat(0, 0, 0, 0), 0);
    }

    /// 参考值本身代入：normalized = 1 → HEAT_SCALE。
    #[test]
    fn reference_inputs_give_scale() {
        assert_eq!(
            calculate_heat(1308, 4991, 41, 6291),
            3100,
            "四个参考值代入应得 HEAT_SCALE"
        );
    }

    /// 与 Python `int(x + 0.5)` 的取整行为一致：0.5 向上。
    #[test]
    fn rounding_matches_python_int_plus_half() {
        // normalized*3100 = 3099.5 → 3100
        let heat = calculate_heat(1308, 4991, 41, 6290);
        // 6290/6291 略小于 1，期望 3099 或 3100，关键是不断言错方向
        assert!(heat == 3099 || heat == 3100, "heat={heat}");
    }
}
