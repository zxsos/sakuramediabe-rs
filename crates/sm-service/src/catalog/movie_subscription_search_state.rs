//! 影片订阅搜索的状态与重试预算（上游 `catalog/movie_subscription_search_state_service.py`，136 行）。
//!
//! # 「订阅一部影片」是**长周期可重试**的过程
//!
//! 新片常常几天后才被收录，所以订阅不是一次查询，而是一个会被打断、会重试、
//! 会最终放弃的状态机。
//!
//! ```text
//!   pending ──begin_attempt──> running ──┬─ mark_succeeded ─> succeeded（终态）
//!      ↑                                 ├─ 可重试失败 ─> failed_retryable
//!      └──── reset / 预算耗尽 ───────────┴─ 预算耗尽 ─> exhausted（终态）
//! ```
//!
//! # 支点：`consumes_budget`
//!
//! | 失败原因 | 消耗预算 | 为什么 |
//! |---|---|---|
//! | `no_candidate_found` | **否** | **正常结果** —— 新片可能还没被收录 |
//! | 索引器搜索失败 | 是 | 服务端问题，该退避重试 |
//! | 提交下载失败 | 是 | 同上 |
//!
//! **把「没找到」算成失败是最容易写错的地方**：刚订阅的新片因「还没人发片」
//! 被判失败 → 几次后预算耗尽 → 那部影片**永久不再搜索**，用户却毫不知情。
//!
//! # `failed_retryable` 与 `exhausted` 必须分开
//!
//! 「还在等」与「已放弃」合成一个状态，会让放弃看起来像还在等 —— 任务中心
//! 里永远转圈。
//!
//! # 刻意**不用**通用任务台账
//!
//! 它的粒度是**影片**，而任务台账是 `task_key`。借用会让「2000 部影片各搜
//! 一次」在任务中心显示成 2000 条任务。
//!
//! # `running` 会因进程崩溃卡死
//!
//! 所以启动时要跑 [`MovieSubscriptionSearchStateService::recover_interrupted_running`]，
//! 否则那些影片永远停在「正在搜索」而不再被领取。

use crate::error::ServiceError;

/// 待处理（初始状态，也是重试落点）。
pub const STATE_PENDING: &str = "pending";
/// 正在搜索。
pub const STATE_RUNNING: &str = "running";
/// 成功（终态）。
pub const STATE_SUCCEEDED: &str = "succeeded";
/// 失败但可重试。
pub const STATE_FAILED_RETRYABLE: &str = "failed_retryable";
/// 预算耗尽（终态，已放弃）。
pub const STATE_EXHAUSTED: &str = "exhausted";

/// 「没找到候选」的错误码。
pub const ERROR_CODE_NO_CANDIDATE: &str = "no_candidate_found";
/// 崩溃中断的固定文案（落库并显示给用户）。
pub const INTERRUPTED_ERROR_MESSAGE: &str = "搜索被中断，已重新排队";

/// 搜索错误。见模块文档的 `consumes_budget` 表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSearchError {
    pub code: String,
    pub message: String,
    /// 是否扣减重试预算。**「没找到」必须是 `false`。**
    pub consumes_budget: bool,
}

impl SubscriptionSearchError {
    /// 「没找到候选」—— **不**消耗预算。
    pub fn no_candidate() -> Self {
        Self {
            code: ERROR_CODE_NO_CANDIDATE.to_owned(),
            message: "未找到符合条件的资源".to_owned(),
            consumes_budget: false,
        }
    }

    /// 索引器搜索失败。消耗预算。
    pub fn indexer_failed(detail: &str) -> Self {
        Self {
            code: "indexer_search_failed".to_owned(),
            message: detail.to_owned(),
            consumes_budget: true,
        }
    }
}

/// 预算耗尽的文案。
pub fn exhausted_message(stale_attempt_limit: i64) -> String {
    format!("已尝试 {stale_attempt_limit} 次仍未找到资源，停止重试")
}

/// 候选筛选条件。上游 `candidate_condition(cls, *, now)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateCondition {
    pub now: chrono::NaiveDateTime,
    /// **只含两个可搜状态**：`succeeded` 与 `exhausted` 都不再进候选。
    pub states: [String; 2],
}

/// 构造候选条件（纯函数，SQL 由仓储层拼）。
pub fn candidate_condition(now: chrono::NaiveDateTime) -> CandidateCondition {
    CandidateCondition {
        now,
        states: [STATE_PENDING.to_owned(), STATE_FAILED_RETRYABLE.to_owned()],
    }
}

/// 订阅搜索状态服务。
pub struct MovieSubscriptionSearchStateService;

impl MovieSubscriptionSearchStateService {
    /// 失败多少次后放弃。上游从 `settings.downloads.*` 读，**不写死**。
    pub fn stale_attempt_limit() -> i64 {
        todo!("骨架：从 config 的 downloads.* 读；写死会让运维失去调整能力")
    }

    /// 搜索记录是否**新鲜**（不需要再搜）。上游 `is_fresh(movie, *, now)`。
    pub fn is_fresh(last_attempt_at: Option<chrono::NaiveDateTime>, state: &str) -> bool {
        let _ = (last_attempt_at, state);
        todo!("骨架：按 state 与最近尝试时间判断；间隔从 config 读")
    }

    /// 领取一次搜索：`pending` → `running` 并计数。
    ///
    /// 上游 `begin_attempt(movie_id) -> Movie | None`。**`None` = 没抢到**。
    ///
    /// 必须是**条件更新**（`WHERE state = 'pending'`）而不是「先查再改」——
    /// 后者会让两个 worker 同时领到同一部影片。
    pub async fn begin_attempt(movie_id: i64) -> Result<Option<i64>, ServiceError> {
        let _ = movie_id;
        todo!("骨架：条件 UPDATE ... WHERE id = $1 AND state = 'pending' RETURNING movie_id")
    }

    /// 标记成功。**终态**，清空失败计数。
    pub async fn mark_succeeded(movie_id: i64) -> Result<(), ServiceError> {
        let _ = movie_id;
        todo!("骨架：state = 'succeeded'，计数清零")
    }

    /// 标记失败。**按 `consumes_budget` 决定落哪个状态**，返回落定的状态。
    ///
    /// ```text
    /// consumes_budget = false -> failed_retryable（计数**不涨**）
    /// consumes_budget = true  -> 计数 +1；超限则 exhausted
    /// ```
    ///
    /// 返回状态是必要的：调用方要写进摘要，而「这次是否变成 exhausted」
    /// 只有这里知道。
    pub async fn mark_failed(
        movie_id: i64,
        error: &SubscriptionSearchError,
    ) -> Result<String, ServiceError> {
        let _ = (movie_id, error);
        todo!("骨架：false -> failed_retryable(计数不变)；true -> 计数+1，超限 -> exhausted")
    }

    /// 重置搜索状态。`movie_ids = None` = 重置**全部**。
    pub async fn reset(movie_ids: Option<&[i64]>) -> Result<u64, ServiceError> {
        let _ = movie_ids;
        todo!("骨架：回到 pending、计数清零；None 时全表")
    }

    /// ★ 启动时把卡在 `running` 的改回 `pending`，返回修复数量。
    ///
    /// 不做这一步，那些影片永远显示「正在搜索」而不再被领取。
    pub async fn recover_interrupted_running() -> Result<u64, ServiceError> {
        todo!("骨架：UPDATE ... WHERE state = 'running' -> pending，记 INTERRUPTED_ERROR_MESSAGE")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★「没找到候选」**不**消耗预算。
    ///
    /// 写成 true 会让刚订阅的新片因「还没被收录」被判失败，几次后永久耗尽
    /// 预算，那部影片再也搜不到。
    #[test]
    fn having_no_candidate_is_not_a_failure() {
        assert!(!SubscriptionSearchError::no_candidate().consumes_budget);
        assert!(SubscriptionSearchError::indexer_failed("超时").consumes_budget);
    }

    /// 候选只含 `pending` 与 `failed_retryable`。
    ///
    /// 尤其 `exhausted` 不能再进候选 —— 放它回去会导致「放弃 → 又搜 → 又放弃」
    /// 的无限循环。
    #[test]
    fn only_retryable_states_are_candidates() {
        let condition = candidate_condition(chrono::NaiveDateTime::UNIX_EPOCH);
        assert_eq!(condition.states, [STATE_PENDING, STATE_FAILED_RETRYABLE]);
        assert!(!condition.states.contains(&STATE_SUCCEEDED.to_owned()));
        assert!(!condition.states.contains(&STATE_EXHAUSTED.to_owned()));
    }

    /// 五个状态**互不相同** —— 拼错状态名会让影片卡在无人认领的中间态。
    #[test]
    fn the_five_states_are_distinct() {
        let states = [
            STATE_PENDING,
            STATE_RUNNING,
            STATE_SUCCEEDED,
            STATE_FAILED_RETRYABLE,
            STATE_EXHAUSTED,
        ];
        let mut sorted = states.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5);
    }

    /// 「没找到」的错误码是**稳定契约**（会落库并进国际化映射）。
    #[test]
    fn the_no_candidate_code_is_pinned() {
        assert_eq!(ERROR_CODE_NO_CANDIDATE, "no_candidate_found");
        assert_eq!(
            SubscriptionSearchError::no_candidate().code,
            "no_candidate_found"
        );
    }

    /// 耗尽文案要带上次数 —— 用户需要知道系统试了几次。
    #[test]
    fn the_exhausted_message_reports_the_attempt_count() {
        let message = exhausted_message(5);
        assert!(message.contains('5'), "文案应含尝试次数：{message}");
    }
}
