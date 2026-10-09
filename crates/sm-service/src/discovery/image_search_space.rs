//! 索引空间状态机 + 入口图片归一化。
//!
//! 上游 `image_search_index_space_service.py`（4.5KB）+ `image_search_input.py`（848B）。
//! **纯 PostgreSQL。**
//!
//! # 这个文件是整个 image_search 的**状态机**，不是查询服务
//!
//! 它回答一个问题：**当前索引能不能查**。四个状态（上游 `:45-99`）：
//!
//! ```text
//!   current_space_id 为空                      -> 未配置
//!   current_space_id 有，indexed_space_id 空   -> 需重建
//!   两者相等                                      -> 就绪
//!   两者不等                                      -> 需重建
//! ```
//!
//! # 与 `sm-db` 已实现那条不变量的关系
//!
//! `sm_db::discovery::image_search` 已有
//! `accepts_session(session_dim, expected_dim)`（`:162`）拒绝维度错配的会话，
//! 并有测试 `rejects_sessions_from_a_different_embedding_space` 钉着。
//! **本文件是那条不变量的上游对应物** —— 换 embedding 模型后老会话若不被拒，
//! 最坏结果是返回语义完全无关的图。`current_space_id` 就是
//! `ImageSearchIndexState` 注释里那个「Qdrant collection / space id」。
//!
//! # 刻意不做本地模型
//!
//! 判空间靠**外部推理服务的 `describe()`**（见 [`super::embedding`]）。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 索引空间状态。字段照抄上游 `:15-19`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchIndexSpaceStatus {
    /// 索引里记录的已索引空间。
    pub indexed_space_id: Option<String>,
    /// 推理服务当前声明的空间。
    pub current_space_id: Option<String>,
    /// 是否可查。
    pub searchable: bool,
    /// 供状态接口展示的空间维度。
    pub dimension: Option<u32>,
}

/// 索引需重建。
///
/// 上游 `ImageSearchIndexRebuildRequiredError`（`:21-39`）。它**不是**「查不到」
/// 而是「索引本身不可信」—— 所以 `details()`（`:29`）把两个 space_id 都带上，
/// 客户端才能自己判断差在哪。
#[derive(Debug, Clone)]
pub struct ImageSearchIndexRebuildRequired {
    pub status: ImageSearchIndexSpaceStatus,
}

impl std::fmt::Display for ImageSearchIndexRebuildRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("图片搜索索引需要重建")
    }
}

impl std::error::Error for ImageSearchIndexRebuildRequired {}

impl ImageSearchIndexRebuildRequired {
    /// 错误 details。两个 space_id 都带上 —— 只说「需重建」客户端无法行动。
    pub fn details(&self) -> Vec<(String, Option<String>)> {
        vec![
            ("indexed_space_id".to_owned(), self.status.indexed_space_id.clone()),
            ("current_space_id".to_owned(), self.status.current_space_id.clone()),
        ]
    }
}

/// 索引空间服务。
pub struct ImageSearchIndexSpaceService;

impl ImageSearchIndexSpaceService {
    /// 当前状态。上游 `get_status`（`:45`）。
    pub fn get_status(
        current_space_id: Option<&str>,
    ) -> Result<ImageSearchIndexSpaceStatus, ServiceError> {
        todo!("骨架：照上游 `:45-73` 实现（读 DB 的 indexed_space_id，与推理服务的 current 比对）")
    }

    /// 查询前的就绪闸门；不通过抛重建错误。上游 `ensure_search_ready`（`:75`）。
    pub fn ensure_search_ready(current_space_id: &str) -> Result<(), ServiceError> {
        todo!("骨架：照上游 `:75-79` 实现")
    }

    /// 索引前的闸门。上游 `prepare_for_indexing`（`:81`）。
    ///
    /// **与 `ensure_search_ready` 是两个不同的闸门，别合并**：查询要「已索引
    /// 且空间匹配」，索引只要「空间匹配」—— 重建过程中查询不通过但索引继续。
    pub fn prepare_for_indexing(current_space_id: &str) -> Result<(), ServiceError> {
        todo!("骨架：照上游 `:81-89` 实现")
    }

    /// 标记某空间已索引完成。上游 `set_indexed_space`（`:91`）。
    pub fn set_indexed_space(current_space_id: &str) -> Result<(), ServiceError> {
        todo!("骨架：照上游 `:91-99` 实现")
    }

    /// 是否已有完成的索引记录。上游 `_has_completed_index_records`（`:101`）。
    pub fn has_completed_index_records() -> Result<bool, ServiceError> {
        todo!("骨架：照上游 `:101+` 实现")
    }
}

/// 归一化检索用图片字节。
///
/// 上游 `normalize_image_search_query`（`image_search_input.py:7`）。**这是整个
/// image_search 的入口校验** —— 拿到的字节要转成推理服务能吃的格式。
///
/// 独立成函数（而不是塞进 `image_search.rs`）的理由：它被**两条**检索路径共用
/// （图搜与剧情图搜），放进任一个都会让另一条反向依赖。
pub fn normalize_image_search_query(image_bytes: &[u8]) -> Vec<u8> {
    todo!("骨架：照上游 image_search_input.py 全量实现（848B）")
}