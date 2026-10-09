//! 影片相关异步任务的聚合入口（上游 `catalog/movie_task_service.py`，48 行）。
//!
//! # 只有 48 行：它只是「手动触发」的薄封装
//!
//! 真正的执行体是 `movie_heat_update` 任务（见 [`super::movie_heat`]）。本文件
//! 只做两件事：按番号定位影片、往调度器提交一个 `trigger_type = "manual"`
//! 的任务运行。
//!
//! # 为什么要走调度器而不是直接调 service
//!
//! 重算热度要扫全表（30 万行）。在 HTTP 请求里同步做会占住连接几十秒。
//!
//! # 409 而非排队
//!
//! 撞上已在跑的同名任务时返回 `409 movie_heat_recompute_conflict`。用户此时
//! 多半是「再点一次看看有没有生效」，排队会让第二次点击无声无息。

use crate::error::ServiceError;

/// 手动触发任务的响应。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManualJobTriggerResponse {
    /// 任务运行 id。**轮询它**看进度。
    pub task_run_id: i64,
    /// 展示名。与 `cron_spec::builtin_jobs` 的 `display_name` 一致。
    pub task_name: String,
    /// 触发类型。手动触发恒为 `manual`。
    pub trigger_type: String,
}

/// 任务键。与 `cron_spec::builtin_jobs` 的 `movie_heat_update` **必须一致**。
pub const MOVIE_HEAT_TASK_KEY: &str = "movie_heat_update";

/// 影片任务服务。
pub struct MovieTaskService;

impl MovieTaskService {
    /// ★ 手动触发某部影片的热度重算。上游 `recompute_movie_heat(cls, movie_number)`。
    ///
    /// **按番号**定位（`require_movie_by_normalized_number`）—— 用户手里通常
    /// 只有番号。
    ///
    /// 错误码：影片不存在 → `404 movie_not_found`；已有同名任务在跑 →
    /// `409 movie_heat_recompute_conflict`。
    pub async fn recompute_movie_heat(
        movie_number: &str,
    ) -> Result<ManualJobTriggerResponse, ServiceError> {
        let _ = movie_number;
        todo!("骨架：按番号定位影片(404) -> 查在跑的同名任务(409) -> 提交 manual 任务运行")
    }

    /// 执行体。worker 调用。上游 `execute_movie_heat(_reporter, params)`。
    ///
    /// `params` 里带的是 `movie_id`（**不是番号**）—— 执行时不该再按番号查，
    /// 番号可能被改。
    pub async fn execute_movie_heat(
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = params;
        todo!("骨架：取 params.movie_id -> 调 movie_heat::update_single_movie_heat -> 回摘要")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 任务键与 `cron_spec` 里的**必须一致** —— 它是调度器定位任务的唯一依据。
    #[test]
    fn the_task_key_matches_the_cron_registry() {
        assert_eq!(MOVIE_HEAT_TASK_KEY, "movie_heat_update");
    }

    /// 响应**必须带**任务运行 id 与触发类型：前者是轮询依据，后者让任务中心
    /// 能区分「我手动点的」与「cron 到的」。
    #[test]
    fn the_trigger_response_carries_the_run_id_and_trigger_type() {
        let response = ManualJobTriggerResponse {
            task_run_id: 42,
            task_name: "执行一次影片热度重算".to_owned(),
            trigger_type: "manual".to_owned(),
        };
        assert_eq!(response.task_run_id, 42);
        assert_eq!(response.trigger_type, "manual");
        assert!(!response.task_name.is_empty());
    }
}
