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
//!
//! # 入队参数是**番号**，执行时再查一次
//!
//! 上游 `params={"movie_number": movie.movie_number}`（`:23`），
//! `execute_movie_heat` 再 `require_movie_by_normalized_number` 一次（`:42`）。
//!
//! 看着绕，但那是**故意的**：任务会排队一段时间才被领取，而这段时间影片可能
//! 被合并（`movie.id` 会指向另一行），`movie_number` 才是稳定键。
//!
//! ⚠️ 骨架期的注释把这一点写反了（「执行时不该再按番号查，番号可能被改」）——
//! 上游恰恰是**按番号**查的。

use serde_json::{json, Value};

use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::system::jobs::ManualJobTriggerResponse;
use crate::system::{resolve_task_name, ConflictPolicy, EnqueueOutcome, TaskQueueService};

use super::movie::MovieService;
use super::movie_heat::{MovieHeatService, FORMULA_VERSION};

/// 任务键。与 `cron_spec::builtin_jobs` 的 `movie_heat_update` **必须一致**。
pub const MOVIE_HEAT_TASK_KEY: &str = "movie_heat_update";

/// 执行体参数。上游 `MovieHeatRecomputeParams`
/// （`schema/catalog/movies.py:199-208`）。
///
/// 校验是 `min_length=1` **加上 strip 后非空** —— 上游那个 `field_validator`
/// 返回的是**去空白后的值**，所以这里也要 trim 之后再定位影片（否则
/// `" ABC-123 "` 会查不到，而用户看不出为什么）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MovieHeatRecomputeParams {
    pub movie_number: String,
}

/// 影片任务服务。
#[derive(Debug, Clone)]
pub struct MovieTaskService {
    db: Db,
}

impl MovieTaskService {
    /// 构造。取 `&Db` 并克隆（与本 crate 全部 service 同形）。
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    /// ★ 手动触发某部影片的热度重算。上游 `recompute_movie_heat`（`:14-37`）。
    ///
    /// 顺序：按番号定位影片（**404 `movie_not_found`**）→ 以 `trigger_type =
    /// manual` 入队（[`ConflictPolicy::Raise`]）→ 撞上在跑的同类任务就 **409**。
    ///
    /// # 409 带 `blocking_task_run_id`
    ///
    /// 客户端要靠它告诉用户「正在跑的是那一条」—— 只给一句「正在执行」，
    /// 用户只能干等，而实际上他可以去看那一条的进度。
    ///
    /// # 为什么不在这里检查 `movie_heat_update` 是否被禁用
    ///
    /// 上游是查了（`submit_manual_job` 里的 `require_job_enabled`），
    /// 但那是**HTTP 层**的统一入口（`/system/jobs/{task_key}/run` 也在用），
    /// 本仓由组合根那侧处理。这里多查一次会让「同一个任务，两条入口两种行为」。
    pub async fn recompute_movie_heat(
        &self,
        movie_number: &str,
    ) -> Result<ManualJobTriggerResponse, ServiceError> {
        // 404 的细节（`{"movie_number": ...}`）由 `require_by_normalized_number`
        // 给出 —— 与「按番号查影片」的其它入口一致。
        let (movie, _canonical) = MovieService::new(&self.db)
            .require_by_normalized_number(movie_number)
            .await?;

        let task_name = resolve_task_name(MOVIE_HEAT_TASK_KEY, None);
        let params = json!({ "movie_number": movie.movie_number });
        let outcome = TaskQueueService::new(&self.db)
            .enqueue(
                MOVIE_HEAT_TASK_KEY,
                "manual",
                Some(&task_name),
                Some(params),
                ConflictPolicy::Raise,
            )
            .await?;

        match outcome {
            EnqueueOutcome::Enqueued(run) => Ok(ManualJobTriggerResponse {
                task_run_id: run.id,
                task_key: run.task_key,
                state: run.state,
            }),
            EnqueueOutcome::Skipped {
                blocking_task_run_id,
            } => Err(ServiceError::conflict(
                "movie_heat_recompute_conflict",
                "影片热度任务正在执行",
                // 上游：`{"blocking_task_run_id": blocking.id if blocking else None}`
                // —— 取不到时是 **JSON null**，不是省略这个键。
                Some(details_of("blocking_task_run_id", blocking_task_run_id)),
            )),
        }
    }

    /// 执行体。worker 调用。上游 `execute_movie_heat`（`:39-48`）。
    ///
    /// 返回的四个键与上游逐字一致（进 `result_summary`）：
    /// `movie_id` / `movie_number` / `updated_count` / `formula_version`。
    ///
    /// # `movie_number` 缺了或空白的 `params` 是 422
    ///
    /// 上游在 pydantic 那一层就拒了（`min_length=1` + strip 非空）。
    /// 这里同样在进查询之前拒 —— 否则会走到 `require_by_normalized_number`
    /// 拿一个 404，而真正的问题是**任务的参数被写坏了**。
    pub async fn execute_movie_heat(&self, params: &Value) -> Result<Value, ServiceError> {
        let payload: MovieHeatRecomputeParams =
            serde_json::from_value(params.clone()).map_err(|error| {
                ServiceError::validation("validation_error", format!("任务参数不合法：{error}"))
            })?;
        let movie_number = payload.movie_number.trim();
        if movie_number.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "movie_number cannot be blank",
            ));
        }

        let (movie, _canonical) = MovieService::new(&self.db)
            .require_by_normalized_number(movie_number)
            .await?;
        // `u64` -> `i64`：与 `movie_heat` 那条任务摘要同款（`rows_affected` 是
        // u64，而 JSON 里只放得下 i64；上界是「一次 UPDATE 命中的行数」）。
        let updated_count = MovieHeatService::new(&self.db)
            .update_single_movie_heat(movie.id)
            .await?;

        Ok(json!({
            "movie_id": movie.id,
            "movie_number": movie.movie_number,
            "updated_count": updated_count as i64,
            "formula_version": FORMULA_VERSION,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 任务键与 `cron_spec` 里的**必须一致** —— 它是调度器定位任务的唯一依据。
    #[test]
    fn the_task_key_matches_the_cron_registry() {
        assert_eq!(MOVIE_HEAT_TASK_KEY, "movie_heat_update");
        // 目录表里也要有：没有显示名的任务在任务中心会显示英文键。
        assert_eq!(resolve_task_name(MOVIE_HEAT_TASK_KEY, None), "影片热度更新");
    }

    /// 参数形状是 `{ movie_number }`，**不是** `{ movie_id }`。
    ///
    /// 这条盯着一个曾经写反的地方：骨架期的文档说「执行时带的是 movie_id，
    /// 不该再按番号查」。上游恰好相反 —— 传番号、执行时再查一次，因为合并会
    /// 让 id 换行而番号仍然稳定。
    #[test]
    fn the_params_carry_the_movie_number_not_an_id() {
        let params: MovieHeatRecomputeParams =
            serde_json::from_value(json!({ "movie_number": "ABC-123" })).expect("可解析");
        assert_eq!(params.movie_number, "ABC-123");

        // 只给 id 的形状必须解不出来（否则「传错参数」会静默变成「查不到」）。
        assert!(
            serde_json::from_value::<MovieHeatRecomputeParams>(json!({ "movie_id": 7 })).is_err()
        );
        assert!(serde_json::from_value::<MovieHeatRecomputeParams>(json!({})).is_err());
    }
}
