//! 图片搜索索引重置（上游 `image_search_reset_service.py`，**980 字节**）。
//!
//! **纯 PostgreSQL + 任务队列**，无 Qdrant 直连、无推理服务、无插件。
//!
//! # 它**不删任何东西** —— 它是**任务入队**
//!
//! 这是本文件最容易搞错的地方，而我在骨架阶段**搞错了**：当时以为它「清掉索引
//! 状态 + 给在途任务发信号」，返回值是「各表清理条数」。
//!
//! 读完全文（30 行）后才知道它只做一件事：
//!
//! ```python
//! TaskQueueService.enqueue(
//!     task_key="image_search_index",
//!     trigger_type="manual",
//!     params={"reset": True},
//!     conflict="raise",
//! )
//! ```
//!
//! **真正的重置逻辑在后台任务 `image_search_index` 里**，而那个 handler
//! **还没写**（`sm-scheduler` 的 21 个 handler 只落地了 1 个）。所以这个端点
//! 现在能做的只是「排上队」，排上之后要等 handler 补齐。
//!
//! 返回值是 `{"task_run_id": <id>}` —— **一个 id**，不是清理条数。
//!
//! # 两道前置检查，顺序不能换
//!
//! | # | 检查 | 不满足时 | 出处 |
//! |---|---|---|---|
//! | 1 | `require_image_search()` | **409** 图搜未启用 | `:12` |
//! | 2 | `conflict = "raise"` | **409** 已有同 key 任务在跑/在队 | `:18-26` |
//!
//! 检查 1 在本文件之外 —— `sm_service::system::optional_services::require_image_search`
//! 的文档写着「与上游 `require_image_search` **逐字一致**」，所以直接调它。
//!
//! # `conflict = "raise"` 是这个端点的**关键选择**
//!
//! `ConflictPolicy` 有两个值：
//!
//! | | 行为 | 谁用 |
//! |---|---|---|
//! | [`Skip`](ConflictPolicy::Skip) | 撞上就跳过（**coalesce** 语义）| cron tick |
//! | [`Raise`](ConflictPolicy::Raise) | 撞上时报 409，带阻塞方行 id | **手动触发** |
//!
//! 手选用 `Raise` 是因为：用户点了「重置」却得到「已排队」而不知道，会以为
//! 已经有一次在跑了。**409 + `blocking_task_run_id` 让前端能显示「已有任务
//! 在跑（#123）」**，用户能自己决定要不要等。
//!
//! 照抄。改成 `Skip` 会让这个端点变成静默 no-op。
//!
//! # 409 的 details 有两个键
//!
//! 上游 `:25`：`{"task_key": ..., "blocking_task_run_id": ...}`。
//! 两个都要 —— 前者告诉用户是哪个任务，后者让他能去任务中心看进度。

use serde::{Deserialize, Serialize};
use serde_json::Map;

// ⚠️ 是 `crate::` 不是 `sm_service::` —— 本文件就在 `sm-service` 里，
// 用 crate 名引用自己会找不到模块。
use crate::error::ServiceError;
use crate::system::optional_services::require_image_search;
use crate::system::task_queue::{ConflictPolicy, EnqueueOutcome, TaskQueueService};

/// 被触发的任务键。**与 `optional_services::job_disabled_reason` 里那个字符串
/// 必须是同一个** —— 那里靠它把任务中心里的这一项置灰。
pub const RESET_TASK_KEY: &str = "image_search_index";

/// 重置结果。
///
/// # 只有一个字段
///
/// 上游 `{"task_run_id": int(task_run.id)}`。**不是清理条数** —— 清理由后台
/// 任务做，这个响应只表示「排上了」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchResetResult {
    /// 排上的任务运行 id。**拿它去轮询进度**。
    pub task_run_id: i64,
}

/// 重置服务。
pub struct ImageSearchResetService {
    queue: TaskQueueService,
}

impl ImageSearchResetService {
    /// 构造。
    pub fn new(queue: TaskQueueService) -> Self {
        Self { queue }
    }

    /// 入队参数：`{"reset": true}`。
    ///
    /// # 这个 `reset` 标志是**给 handler 的指令**
    ///
    /// 同一个 `image_search_index` 任务既可能被 cron 触发（补齐新缩略图），
    /// 也可能被这里手动触发（清空重建）。handler 靠 `params.reset` 区分。
    ///
    /// **别把字段名改成别的** —— 那是 handler 与本文件之间的契约。
    pub fn reset_params() -> serde_json::Value {
        serde_json::json!({ "reset": true })
    }

    /// 触发重置。
    ///
    /// # 幂等性：**刻意不幂等**
    ///
    /// 重复调用撞上 `Raise` 就报 409，**不排队第二次**。重置是破坏性的
    /// （清索引 + 重建），排两次会让第二次紧跟着第一次跑完再清一遍 ——
    /// 白做一轮全量索引。
    ///
    /// 「无待清理内容时返回空结果」这种幂等语义**不适用于这里**（我骨架阶段
    /// 写错了）。
    pub async fn reset(
        &self,
        config_values: &serde_json::Value,
    ) -> Result<ImageSearchResetResult, ServiceError> {
        // 检查 1：图搜未启用。`FeatureDisabled` 自己会转成 409。
        require_image_search(config_values)?;

        // 检查 2 + 入队。`Raise` 而非 `Skip`（见模块文档）。
        let outcome = self
            .queue
            .enqueue(
                RESET_TASK_KEY,
                "manual",
                // `task_name` 缺省回落到 `task_key`（enqueue 内部处理）。
                None,
                Some(Self::reset_params()),
                ConflictPolicy::Raise,
            )
            .await?;

        match outcome {
            EnqueueOutcome::Enqueued(run) => Ok(ImageSearchResetResult {
                task_run_id: run.id as i64,
            }),
            EnqueueOutcome::Skipped {
                blocking_task_run_id,
            } => {
                // 用了 `Raise` 还走到这里，说明互斥键被占。
                // 两个 details 键都带上（见模块文档）。
                Err(ServiceError::conflict(
                    "image_search_reset_conflict",
                    "图片搜索索引正在运行或已排队",
                    Some(details(RESET_TASK_KEY, blocking_task_run_id)),
                ))
            }
        }
    }
}

/// 409 的 details。两个键。
fn details(task_key: &str, blocking_task_run_id: Option<i32>) -> Map<String, serde_json::Value> {
    Map::from_iter([
        ("task_key".to_owned(), serde_json::json!(task_key)),
        (
            "blocking_task_run_id".to_owned(),
            match blocking_task_run_id {
                Some(id) => serde_json::json!(id),
                // 上游 `exc.blocking_task_run_id` 可能为 None。
                // **不省略这个键** —— 客户端要靠它判断「有阻塞方但 id 未知」
                // 与「根本没查到阻塞方」。
                None => serde_json::Value::Null,
            },
        ),
    ])
}
