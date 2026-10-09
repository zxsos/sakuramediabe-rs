//! 缩略图子模块的契约类型（上游 `playback/thumbnails/contracts.py`，23 行）。
//!
//! # 只有一个类型，但它防的是「无限 pending」
//!
//! [`ThumbnailDeferred`]：媒体源**暂时**没准备好（盘没挂载、文件还在下载、
//! 115 那边在扫盘）。上游 docstring：「必须带有限次退避策略，不能无限 pending。」
//!
//! # 为什么这个错误不能被当成普通失败重试
//!
//! 普通失败（`failed`）有 `MAX_FAILURE_ATTEMPTS = 2` 次退避，之后进终态。
//! 而「源没就绪」可能是**持续几小时**的（大盘归档）。若按普通失败处理，
//! 两分钟后就进终态 —— 那部媒体**再也不会生成缩略图**。
//!
//! 延迟态有自己的两个参数：
//!
//! | 参数 | 上游值 | 作用 |
//! |---|---|---|
//! | `max_deferred_attempts` | 3 | 延迟 3 次仍不行才转终态 |
//! | `deferred_backoff_base_seconds` | 15 分钟 | 退避基数（指数增长） |
//!
//! # 两个参数都**必须为正**，否则抛 `ValueError`
//!
//! 上游显式校验。`max_deferred_attempts = 0` 会让第一次延迟就终态
//! （等于没有延迟机制）；`backoff_base = 0` 会让 3 次重试在**同一秒**内发生
//! —— 那不是退避，是立即重试。

/// 媒体源暂未就绪。**不是**失败，是延迟。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailDeferred {
    pub message: String,
    /// 默认 `thumbnail_source_deferred`。**会落进 `thumbnail_last_error_code`**，
    /// 任务中心据此显示「稍后重试」而不是「失败」。
    pub error_code: String,
    /// 最多延迟几次。**必须 > 0**。
    pub max_deferred_attempts: u32,
    /// 退避基数（秒）。**必须 > 0**。
    pub deferred_backoff_base_seconds: i64,
}

/// 默认错误码。
pub const THUMBNAIL_SOURCE_DEFERRED: &str = "thumbnail_source_deferred";

impl ThumbnailDeferred {
    /// 构造。**校验两个参数为正**（见模块文档）。
    pub fn new(
        message: impl Into<String>,
        max_deferred_attempts: u32,
        deferred_backoff_base_seconds: i64,
    ) -> Result<Self, String> {
        if max_deferred_attempts == 0 {
            return Err("max_deferred_attempts_must_be_positive".to_owned());
        }
        if deferred_backoff_base_seconds <= 0 {
            return Err("deferred_backoff_base_seconds_must_be_positive".to_owned());
        }
        Ok(Self {
            message: message.into(),
            error_code: THUMBNAIL_SOURCE_DEFERRED.to_owned(),
            max_deferred_attempts,
            deferred_backoff_base_seconds,
        })
    }

    /// 第 `attempt` 次延迟该等多久（指数退避）。`attempt` 从 1 起。
    pub fn backoff_seconds(&self, attempt: u32) -> i64 {
        // saturating_mul / saturating_pow 防溢出：第 30 次延迟时
        // 900 * 2^29 远超 i64。不封顶的话一个卡住的媒体会算出负数退避，
        // 变成「立即重试」。
        self.deferred_backoff_base_seconds
            .saturating_mul(2_i64.saturating_pow(attempt.saturating_sub(1).min(40)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 两个参数为 0 时**必须报错**，否则退避机制失效。
    #[test]
    fn non_positive_parameters_are_rejected() {
        assert_eq!(
            ThumbnailDeferred::new("x", 0, 900).expect_err("次数为 0 应被拒"),
            "max_deferred_attempts_must_be_positive"
        );
        assert_eq!(
            ThumbnailDeferred::new("x", 3, 0).expect_err("基数为 0 应被拒"),
            "deferred_backoff_base_seconds_must_be_positive"
        );
        assert!(ThumbnailDeferred::new("x", 3, 900).is_ok());
    }

    /// 退避是**指数增长**：900 -> 1800 -> 3600。
    #[test]
    fn backoff_grows_exponentially_from_the_base() {
        let deferred = ThumbnailDeferred::new("x", 3, 900).expect("参数合法");
        assert_eq!(deferred.backoff_seconds(1), 900);
        assert_eq!(deferred.backoff_seconds(2), 1800);
        assert_eq!(deferred.backoff_seconds(3), 3600);
    }

    /// ★ 极大次数下退避**封顶为正数**，不溢出成负数。
    ///
    /// 第 30 次延迟时 `900 * 2^29` 远超 i64。不封顶会算出负数退避 ——
    /// 那等于立即重试，比不延迟更糟。
    #[test]
    fn backoff_never_overflows_into_a_negative_delay() {
        let deferred = ThumbnailDeferred::new("x", 1000, 900).expect("参数合法");
        for attempt in [1u32, 10, 30, 100, 1000] {
            assert!(
                deferred.backoff_seconds(attempt) > 0,
                "第 {attempt} 次的退避变成了非正数"
            );
        }
    }

    /// 默认错误码是**稳定字符串** —— 它落进 `thumbnail_last_error_code`。
    #[test]
    fn the_error_code_is_pinned() {
        let deferred = ThumbnailDeferred::new("x", 3, 900).expect("参数合法");
        assert_eq!(deferred.error_code, "thumbnail_source_deferred");
    }
}
