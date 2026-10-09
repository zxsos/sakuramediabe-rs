//! 运行遥测（上游 `system/telemetry_service.py`，188 行）。
//!
//! # 回答「这次跑得对不对」，不回答「跑得怎么样」
//!
//! 遥测是**任务中心**那些数字的来源：本次处理了多少、成功多少、失败多少、
//! 耗时多少。
//!
//! ⚠️ 它**不**做性能剖析、不采集指标到 Prometheus、不上报到外部 —— 那些
//! 是运维侧的事。这个文件只服务「用户看自己这次任务跑得怎样」。
//!
//! # 三个来源，**不要混算**
//!
//! | 来源 | 含义 |
//! |---|---|
//! | `task_runs` | 通用任务台账（有 `trigger_type`、`summary`） |
//! | `media_point` 派生 | 时刻相关 |
//! | Qdrant 集合状态 | 图搜索引是否就绪 |
//!
//! 混算会得出「任务成功率 95%」这种**没有意义**的数 —— 分母里混进了
//! 「不需要成功的项」。
//!
//! # 图搜就绪状态来自**数据库**而非 Qdrant
//!
//! 理由同 `discovery::image_search_space`：状态是 `image_search_index_state`
//! 这个**单例表**（id 恒为 1）。读它而不是问 Qdrant —— 问 Qdrant 是一次
//! 网络往返，而 `/status` 是高频轮询接口。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 任务统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTelemetry {
    /// 总运行数。
    pub total: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// 仍在跑（有 `started_at` 无 `finished_at`）。
    pub running: i64,
    /// 成功率 [0, 1]。`total = 0` 时为 `None`，**不是 0** ——
    /// 「没跑过」与「全部失败」必须能区分。
    pub success_rate: Option<f64>,
}

impl TaskTelemetry {
    /// 由计数构造。**纯函数** —— 分母为 0 时不给 0%。
    pub fn from_counts(total: i64, succeeded: i64, failed: i64, running: i64) -> Self {
        let success_rate = if total > 0 {
            Some(succeeded as f64 / total as f64)
        } else {
            None
        };
        Self {
            total,
            succeeded,
            failed,
            running,
            success_rate,
        }
    }
}

/// 图搜索引状态（读**单例表**，不问 Qdrant —— 见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchIndexTelemetry {
    /// 索引记录的嵌入空间 id。与 `discovery::image_search_space` 里
    /// `indexed_space_id` 同义。
    pub indexed_space_id: Option<String>,
    /// 是否可检索。`indexed_space_id` 为 `None` = 还没建过索引。
    pub searchable: bool,
}

/// 磁盘空间（三列）。上游 `status_service` 在等 `media_library` 的
/// `storage_space_usages` —— 见 `playback::media_library`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsageTelemetry {
    /// 无 N 条列 = 尚未接入（`media_library` 未落地时恒为 `None`）。
    pub total_bytes: Option<i64>,
    pub used_bytes: Option<i64>,
    pub free_bytes: Option<i64>,
}

/// 完整遥测。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    /// 按 `task_key` 分组的任务统计。
    pub tasks: Vec<TaskTelemetryByKey>,
    pub image_search: ImageSearchIndexTelemetry,
    pub disk: DiskUsageTelemetry,
}

/// 按任务键的统计。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskTelemetryByKey {
    pub task_key: String,
    /// 展示名。
    pub display_name: String,
    pub stats: TaskTelemetry,
    /// 最近一次运行时间。`None` = 从未跑过。
    pub last_run_at: Option<String>,
}

/// 遥测服务。
pub struct TelemetryService;

impl TelemetryService {
    /// 取一次完整快照。
    ///
    /// 三个来源**分别查**（见模块文档），不要合成一个数字。
    pub async fn snapshot() -> Result<TelemetrySnapshot, ServiceError> {
        todo!("骨架：查 task_runs 分组统计 -> 读 image_search_index_state 单例 -> 汇总各库空间占用")
    }

    /// 任务统计。**按 `task_key` 分组**，不跨任务聚合。
    pub async fn task_stats() -> Result<Vec<TaskTelemetryByKey>, ServiceError> {
        todo!("骨架：GROUP BY task_key；running = started_at 非空且 finished_at 为空")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 分母为 0 时 `success_rate` 是 `None`，**不是 0**。
    ///
    /// 「没跑过」与「全部失败」必须能区分 —— 前者不是问题，后者是。
    #[test]
    fn no_runs_is_none_not_zero_percent() {
        let stats = TaskTelemetry::from_counts(0, 0, 0, 0);
        assert_eq!(stats.success_rate, None, "没跑过不等于 0% 成功率");
    }

    /// 有成功时成功率在 [0, 1]。
    #[test]
    fn the_success_rate_is_a_ratio() {
        let stats = TaskTelemetry::from_counts(10, 9, 1, 0);
        assert_eq!(stats.success_rate, Some(0.9));
        let half = TaskTelemetry::from_counts(4, 2, 2, 0);
        assert_eq!(half.success_rate, Some(0.5));
    }

    /// 「仍在跑」**不计入**成功率的分母争议 —— 它单列。
    #[test]
    fn running_runs_are_counted_separately() {
        let stats = TaskTelemetry::from_counts(10, 9, 1, 5);
        assert_eq!(stats.running, 5);
        assert_eq!(stats.total, 10);
    }

    /// 图搜「没建过索引」与「已建可检索」是**不同**状态。
    #[test]
    fn a_missing_index_is_not_a_searchable_one() {
        let none = ImageSearchIndexTelemetry {
            indexed_space_id: None,
            searchable: false,
        };
        let ready = ImageSearchIndexTelemetry {
            indexed_space_id: Some("siglip2-v3".to_owned()),
            searchable: true,
        };
        assert!(!none.searchable);
        assert!(ready.searchable);
    }
}

