//! 图片搜索索引重置（上游 `image_search_reset_service.py`，980B）。
//!
//! **纯 PostgreSQL，不碰 Qdrant。**
//!
//! 上一轮我曾说「三个 status 端点卡 Qdrant」，其中把
//! `POST /image-search/reset` 算进去了 —— **那是错的**。这个文件只 import
//! `optional_services` 与 `task_queue_service`，做的是「清掉索引状态 + 给在途
//! 的索引任务发信号」。
//!
//! # 为什么它小到不像一个 service
//!
//! 980 字节、只有一个 `reset`。真正的删除动作在别处（Qdrant 集合由
//! `qdrant::dense::DenseStore::clear` 负责），这里只改 DB 里的状态并让在途任务
//! 知道「要重置了」。
//!
//! # 幂等性
//!
//! 重复调用必须安全 —— 运维很可能在排查时连点两次。上游 `:11` 的返回值是
//! `dict[str, int]`（各表清理条数），**空字典就是「本来就干净」**，不是错误。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 重置结果：各表清理条数。
///
/// 用 `HashMap` 而不是具名 struct —— 上游返回 dict，键随版本增减。具名 struct
/// 会在上游加一个键时逼着改这里。
pub type ImageSearchResetResult = std::collections::HashMap<String, i64>;

/// 重置服务。
pub struct ImageSearchResetService;

impl ImageSearchResetService {
    /// 重置图片搜索索引状态。上游 `reset`（`:11`）。
    ///
    /// **幂等**：无待清理内容时返回空结果，不是错误。
    pub fn reset() -> Result<ImageSearchResetResult, ServiceError> {
        todo!("骨架：照上游 `:11-30` 实现")
    }
}

/// 供 `sm-api` 路由用的响应包装。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchResetResponse {
    /// 扁平化后的清理条数，直接进 JSON body。
    pub result: ImageSearchResetResult,
}