//! 缩略图生成任务（上游 `playback/thumbnails/task_service.py`，509 行）。
//!
//! # 任务键 `media_thumbnail_generation`，cron **每 30 分钟**
//!
//! 缩略图生成要调 provider（可能跑 ffprobe、抽帧），慢且重。所以每半小时一轮，
//! 每次只处理「到期的」那些。
//!
//! # 三条独立的重试轨道
//!
//! ```text
//!   pending ──生成──> 成功
//!     │
//!     ├─ 源没就绪 ─> deferred（最多 3 次，15 分钟起指数退避）─> 超限 -> 终态
//!     ├─ 真失败 ───> failed（最多 2 次，15 分钟起退避）──────> 超限 -> 终态
//!     └─ 成功但数量不够 ─> 仍算成功（见下方「数量不足」）
//! ```
//!
//! 三个计数器**互不干扰**：`thumbnail_deferred_count` 与
//! `thumbnail_attempt_count` 是 `media` 表上两个独立列。
//!
//! ⚠️ 混用会导致「延迟 3 次后被当成失败 3 次而终态」—— 一个只是盘没挂载的
//! 媒体会因此永远拿不到缩略图。
//!
//! # 「数量不足」是**成功**，不是失败
//!
//! [`Self::minimum_acceptable_count`]：provider 返回 3 张、我们期望 5 张，
//! 只要 ≥ 下限就算成功。
//!
//! 为什么：短片可能只有 2 个有效抽帧点，硬要求「等于期望数」会让它永远失败。
//! 下限是 `expected * 0.6` 一类（见该函数）。

use super::contracts::ThumbnailDeferred;
use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `media_thumbnail_generation` 一致。
pub const TASK_KEY: &str = "media_thumbnail_generation";

/// 普通失败的最大重试次数。
pub const MAX_FAILURE_ATTEMPTS: u32 = 2;
/// 源未就绪的最大延迟次数。
pub const MAX_DEFERRED_ATTEMPTS: u32 = 3;
/// 失败退避基数（秒）。
pub const FAILURE_RETRY_BACKOFF_BASE_SECONDS: i64 = 15 * 60;
/// 失败退避上限（秒）。**24 小时** —— 再久就等下一天了。
pub const FAILURE_RETRY_BACKOFF_MAX_SECONDS: i64 = 24 * 3600;

/// 终态错误码集合。这些**不再重试**。
///
/// 进终态意味着「人工介入才可能解决」。多一个进来少一次无效重试。
pub const TERMINAL_ERROR_CODES: [&str; 7] = [
    "thumbnail_artifact_invalid",
    "thumbnail_artifact_not_webp",
    "thumbnail_artifact_empty",
    "thumbnail_artifact_path_invalid",
    "thumbnail_offset_invalid",
    "provider_not_installed",
    "media_not_found",
];

/// 一次生成的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThumbnailGenerationOutcome {
    /// `success` / `deferred` / `failed`。
    pub state: String,
    pub generated_count: u32,
    /// 失败或延迟时的原因码。`state = "success"` 时为 `None`。
    pub error_code: Option<String>,
}

/// 下限：期望条数的 60%（**向上取整**，且至少 1）。
///
/// 纯函数。`expected_count = 0` 时返回 **1** 而不是 0 —— 0 意味着「任何数量都
/// 不够」，那会让「期望 0 张」的媒体永远无法成功。
pub fn minimum_acceptable_count(expected_count: u32) -> u32 {
    if expected_count == 0 {
        return 1;
    }
    (expected_count * 6 + 9) / 10
}

/// 失败退避秒数：指数增长，**封顶** [`FAILURE_RETRY_BACKOFF_MAX_SECONDS`]。
pub fn failure_backoff_seconds(attempt: u32) -> i64 {
    let base = FAILURE_RETRY_BACKOFF_BASE_SECONDS;
    let grown = base.saturating_mul(1 << attempt.min(20));
    grown.min(FAILURE_RETRY_BACKOFF_MAX_SECONDS)
}

/// 缩略图任务服务。
pub struct MediaThumbnailTaskService;

impl MediaThumbnailTaskService {
    /// 待生成的数量（`state = pending` 且到期）。
    pub async fn count_pending_media() -> Result<i64, ServiceError> {
        todo!("骨架：查 media 缩略图状态为待处理且 next_retry_at 已到期")
    }

    /// 在退避等待中的数量。
    pub async fn count_retry_wait_media() -> Result<i64, ServiceError> {
        todo!("骨架：查 state 为退避等待且 next_retry_at 未到")
    }

    /// 已进终态的数量。
    pub async fn count_terminal_failed_media() -> Result<i64, ServiceError> {
        todo!("骨架：查 state 为终态失败")
    }

    /// 把指定媒体从终态**放回**待处理。返回受影响行数。
    ///
    /// 供「人工重试」用 —— 终态意味着自动重试已放弃，但用户换了个网络环境
    /// 之后可能就想重试了。
    pub async fn reset_terminal_media(media_ids: &[i64]) -> Result<u64, ServiceError> {
        let _ = media_ids;
        todo!("骨架：终态 -> pending，计数清零")
    }

    /// ★ 生成一轮。任务执行体。
    ///
    /// 逐个到期媒体：调 provider 生成 -> 校验产物 -> 落盘登记。
    ///
    /// 单个媒体失败/延迟**不中断**整批 —— 一个坏媒体不该让整轮 500 个白跑。
    pub async fn generate_pending_thumbnails(&self) -> Result<serde_json::Value, ServiceError> {
        todo!("骨架：逐媒体 -> provider 生成 -> validate_artifact -> persist；deferred 与 failed 分开计数")
    }
}

/// 判定一次生成的结果该落成什么状态。**纯函数** —— 三条轨道的分流都在这里。
///
/// ```text
/// generated_count >= minimum_acceptable_count(expected) -> success
/// 源未就绪 且 deferred_count < MAX_DEFERRED_ATTEMPTS        -> deferred
/// 源未就绪 且 超限                                        -> 终态
/// 真失败 且 error_code 在 TERMINAL_ERROR_CODES 里          -> 终态
/// 真失败 且 attempt_count < MAX_FAILURE_ATTEMPTS          -> 失败待重试
/// 真失败 且 超限                                          -> 终态
/// ```
pub fn classify(
    generated_count: u32,
    expected_count: u32,
    deferred: Option<&ThumbnailDeferred>,
    attempt_count: u32,
    deferred_count: u32,
    error_code: Option<&str>,
) -> ThumbnailGenerationOutcome {
    if generated_count >= minimum_acceptable_count(expected_count) {
        return ThumbnailGenerationOutcome {
            state: "success".to_owned(),
            generated_count,
            error_code: None,
        };
    }
    // 数量不足时也要看有没有错误码 —— 「生成 0 张且报 provider 错」不是成功。
    let terminal = error_code.is_some_and(|code| TERMINAL_ERROR_CODES.contains(&code));
    if terminal {
        return ThumbnailGenerationOutcome {
            state: "terminal".to_owned(),
            generated_count,
            error_code: error_code.map(str::to_owned),
        };
    }
    if let Some(deferred_err) = deferred {
        let state = if deferred_count + 1 > deferred_err.max_deferred_attempts {
            "terminal"
        } else {
            "deferred"
        };
        return ThumbnailGenerationOutcome {
            state: state.to_owned(),
            generated_count,
            error_code: Some(deferred_err.error_code.clone()),
        };
    }
    let state = if attempt_count + 1 > MAX_FAILURE_ATTEMPTS {
        "terminal"
    } else {
        "failed"
    };
    ThumbnailGenerationOutcome {
        state: state.to_owned(),
        generated_count,
        error_code: error_code.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 三条轨道的上限互不相同，别弄混。
    #[test]
    fn the_three_tracks_have_different_limits() {
        assert_eq!(MAX_DEFERRED_ATTEMPTS, 3);
        assert_eq!(MAX_FAILURE_ATTEMPTS, 2);
        // 两个计数列是独立的 —— 混用会让「盘没挂载」变成「失败两次后终态」。
        assert_ne!(MAX_DEFERRED_ATTEMPTS, MAX_FAILURE_ATTEMPTS);
    }

    /// 数量不足时仍算**成功**（下限是 60%）。
    #[test]
    fn a_partial_result_still_counts_as_success() {
        let outcome = classify(3, 5, None, 0, 0, None);
        assert_eq!(outcome.state, "success");
        assert_eq!(outcome.generated_count, 3);
    }

    /// 期望 0 张时下限是 **1** 而不是 0 —— 否则永远无法成功。
    #[test]
    fn zero_expected_still_requires_one() {
        assert_eq!(minimum_acceptable_count(0), 1);
    }

    /// 延迟轨与失败轨**互不干扰**：延迟 2 次后仍是 deferred，不是 terminal。
    #[test]
    fn deferred_attempts_do_not_consume_the_failure_budget() {
        let deferred = ThumbnailDeferred::new("盘没挂载", 3, 900).expect("参数合法");
        for deferred_count in 0..3 {
            let outcome = classify(0, 5, Some(&deferred), /*attempt_count=*/ 99, deferred_count, None);
            assert_eq!(
                outcome.state, "deferred",
                "延迟 {deferred_count} 次后不该变终态"
            );
        }
        // 第 4 次才终态。
        assert_eq!(
            classify(0, 5, Some(&deferred), 99, 3, None).state,
            "terminal"
        );
    }

    /// 失败轨超限才终态。
    #[test]
    fn failure_reaches_terminal_only_after_the_limit() {
        assert_eq!(classify(0, 5, None, 0, 0, Some("boom")).state, "failed");
        assert_eq!(classify(0, 5, None, 2, 0, Some("boom")).state, "terminal");
    }

    /// 终态错误码**立刻**终态，不等重试次数。
    #[test]
    fn terminal_error_codes_skip_retry_entirely() {
        for code in TERMINAL_ERROR_CODES {
            let outcome = classify(0, 5, None, 0, 0, Some(code));
            assert_eq!(outcome.state, "terminal", "{code} 应直接终态");
        }
    }

    /// 失败退避**封顶**在 24 小时。
    #[test]
    fn failure_backoff_is_capped_at_one_day() {
        assert_eq!(failure_backoff_seconds(0), 900);
        assert_eq!(failure_backoff_seconds(1), 1800);
        assert_eq!(failure_backoff_seconds(20), FAILURE_RETRY_BACKOFF_MAX_SECONDS);
        assert_eq!(failure_backoff_seconds(63), FAILURE_RETRY_BACKOFF_MAX_SECONDS);
    }
}

