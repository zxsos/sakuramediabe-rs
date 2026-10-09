//! 提交下载请求（上游 `downloads/request_service.py`，145 行）。
//!
//! # 与「搜索候选」是**两件事**
//!
//! [`super::download_search`] 负责「找到种子」，本文件负责「把种子交给下载器」。
//! 分开是因为前者只依赖 Torznab 索引器（纯 HTTP），后者要**下载器 provider
//! 插件** —— 混在一起会让已实现的搜索被插件依赖污染。
//!
//! # 校验顺序有讲究：黑名单**在提交之前**
//!
//! ```text
//! 番号非空 -> 候选字段非空 -> 客户端已绑定索引器 -> 算资源哈希 -> 比黑名单 -> 提交
//! ```
//!
//! 算哈希在提交之前是**硬要求**：提交后下载器就开始下载了，那时再拦已经
//! 晚了（见 [`super::download_resource_hash`] 的模块文档）。
//!
//! # `200` 而不是 `201`
//!
//! 上游这个端点返回 **200**（不是 201、不是 202）。响应体是「已登记的候选 +
//! client id」，**不是**任务句柄 —— 真正的任务要另外查 `GET /download-tasks`。
//! 照抄，别「修正」成 201：那会让客户端以为响应体里有新资源。

use serde::{Deserialize, Serialize};

use super::download_common::{DownloadClientRow, IndexerRow};
use crate::error::ServiceError;

/// 候选的最小信息。**只有这三项会落进 `download_resource`。**
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadRequestCandidate {
    /// 种子地址（magnet 或 .torrent URL）。
    pub source_uri: String,
    pub title: String,
    /// 种子大小（字节）。用于展示与体积过滤。
    pub size_bytes: Option<i64>,
}

/// `POST /download-requests` 的请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadRequestCreateRequest {
    pub movie_number: String,
    /// 索引器名。**必填** —— 宿主不猜用哪个索引器。
    pub indexer_name: String,
    pub candidate: DownloadRequestCandidate,
}

/// 响应体。**200**。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadRequestCreateResponse {
    pub movie_number: String,
    /// 实际提交到的下载器客户端名。
    pub client_name: String,
    /// provider 侧的任务标识。**没有它就没法删任务**。
    pub remote_task_id: String,
    /// 落在台账里的本地任务 id（**可为空** —— provider 可能不返回可跟踪的 id）。
    pub task_id: Option<i64>,
}

/// ★ 前置校验。**纯函数**（不碰 IO）—— 所以能直接测，且顺序照上游。
///
/// 顺序：**番号 → 候选的两个字段**。先番号是因为它是定位键，空番号后面查什么
/// 都是白查。
///
/// 客户端绑定与黑名单**不在这里** —— 它们要 IO，分别在 `create_request` 的
/// 后续步骤里。把需要 IO 的部分塞进校验函数会让它没法单独测。
pub fn validate_request(payload: &DownloadRequestCreateRequest) -> Result<(), ServiceError> {
    if payload.movie_number.trim().is_empty() {
        return Err(ServiceError::validation(
            "invalid_download_request_movie_number",
            "番号不能为空",
        ));
    }
    if payload.candidate.source_uri.trim().is_empty() {
        return Err(ServiceError::validation(
            "invalid_download_request_candidate",
            "候选的 source_uri 不能为空",
        ));
    }
    if payload.candidate.title.trim().is_empty() {
        return Err(ServiceError::validation(
            "invalid_download_request_candidate",
            "候选的标题不能为空",
        ));
    }
    Ok(())
}

/// 提交服务。
// `inner` 尚未被方法体引用（`create_request` 还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct DownloadRequestService {
    /// 可注入的搜索/提交依赖，测试时替身注入。
    inner: Option<Box<dyn RequestDeps>>,
}

impl Default for DownloadRequestService {
    fn default() -> Self {
        Self::new()
    }
}

/// 可注入依赖面。**存在是为了测试**，生产路径用 `None` 走真实实现。
pub trait RequestDeps: Send + Sync {
    /// 取索引器。
    fn indexer(&self, name: &str) -> Result<IndexerRow, ServiceError>;
    /// 取与索引器绑定的客户端（空列表 → 422）。
    fn bound_clients(&self, indexer: &IndexerRow) -> Result<Vec<DownloadClientRow>, ServiceError>;
    /// 该资源是否被主机黑名单拉黑。
    fn is_blacklisted(&self, info_hash: &str) -> Result<bool, ServiceError>;
    /// 向下载器提交。**返回 provider 侧任务标识**。
    fn submit(
        &self,
        client: &DownloadClientRow,
        candidate: &DownloadRequestCandidate,
    ) -> Result<String, ServiceError>;
}

impl DownloadRequestService {
    /// 构造（真实依赖）。
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// 构造（注入替身，测试用）。
    pub fn with_deps(deps: Box<dyn RequestDeps>) -> Self {
        Self { inner: Some(deps) }
    }

    /// ★ 提交一个候选。上游 `create_request`。
    ///
    /// 错误码（照上游）：
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 番号为空 | `422 invalid_download_request_movie_number` |
    /// | 候选的 `source_uri` 或 `title` 为空 | `422 invalid_download_request_candidate` |
    /// | 客户端没绑定这个索引器 | `422 download_request_client_not_bound_to_indexer` |
    /// | 资源在黑名单里 | `422 download_source_blacklisted` |
    /// | provider 报错 | `provider_{code}`（见 [`super::download_common::provider_error`]） |
    ///
    /// **黑名单在提交之前**（见模块文档）。
    pub async fn create_request(
        &self,
        payload: DownloadRequestCreateRequest,
    ) -> Result<DownloadRequestCreateResponse, ServiceError> {
        // 1. 校验：番号 -> 候选字段（纯函数，不碰 IO）
        validate_request(&payload)?;

        // 2. 取依赖（测试注入或真实实现）
        let deps = self.inner.as_ref().ok_or_else(|| {
            ServiceError::unavailable(
                "download_request_no_deps",
                "真实依赖尚未实现（需要 provider seam）",
            )
        })?;

        // 3. 解析索引器
        let indexer = deps.indexer(&payload.indexer_name)?;

        // 4. 解析绑定的客户端（空列表 -> 422，由 deps 实现负责）
        let clients = deps.bound_clients(&indexer)?;
        let client = clients.into_iter().next().ok_or_else(|| {
            ServiceError::validation(
                "download_request_client_not_bound_to_indexer",
                "没有可用的下载客户端",
            )
        })?;

        // 5. 算资源哈希
        let info_hash = super::download_resource_hash::resolve_resource_hash(
            &payload.candidate.source_uri,
        )
        .await?;

        // 6. 查黑名单（提交之前是硬要求）
        if deps.is_blacklisted(&info_hash)? {
            return Err(ServiceError::validation(
                "download_source_blacklisted",
                "该资源已被拉黑",
            ));
        }

        // 7. 提交到下载器（provider 调用走 trait）
        let remote_task_id = deps.submit(&client, &payload.candidate)?;

        // 8. 返回响应
        Ok(DownloadRequestCreateResponse {
            movie_number: payload.movie_number,
            client_name: client.name.clone(),
            remote_task_id,
            task_id: None, // 台账落库待 provider seam 就绪后补
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 番号为空 → 422，且错误码是**这个端点专用**的。
    ///
    /// 不是通用的 `validation_error` —— 客户端要靠它区分「番号写错了」与
    /// 「下载器出问题了」。
    #[test]
    fn a_blank_movie_number_is_rejected_with_the_dedicated_code() {
        let request = DownloadRequestCreateRequest {
            movie_number: "   ".to_owned(),
            indexer_name: "nyaa".to_owned(),
            candidate: DownloadRequestCandidate {
                source_uri: "magnet:?xt=urn:btih:abc".to_owned(),
                title: "t".to_owned(),
                size_bytes: None,
            },
        };
        let service = DownloadRequestService::new();
        let error = validate_request(&request).expect_err("空番号应被拒");
        assert_eq!(error.code(), "invalid_download_request_movie_number");
        assert!(service.inner.is_none());
    }

    /// 候选的 `source_uri` 与 `title` **都要**非空 —— 少一个都不行。
    ///
    /// `source_uri` 空则没法提交；`title` 空则任务列表里是一条无名任务，
    /// 用户无法辨认。
    #[test]
    fn both_candidate_fields_are_required() {
        let base = DownloadRequestCreateRequest {
            movie_number: "ABC-123".to_owned(),
            indexer_name: "nyaa".to_owned(),
            candidate: DownloadRequestCandidate {
                source_uri: "magnet:?xt=urn:btih:abc".to_owned(),
                title: "t".to_owned(),
                size_bytes: None,
            },
        };
        let mut no_uri = base.clone();
        no_uri.candidate.source_uri = "  ".to_owned();
        assert_eq!(
            validate_request(&no_uri).expect_err("空 uri").code(),
            "invalid_download_request_candidate"
        );
        let mut no_title = base;
        no_title.candidate.title = String::new();
        assert_eq!(
            validate_request(&no_title).expect_err("空标题").code(),
            "invalid_download_request_candidate"
        );
    }

    /// 前置校验本身**不碰 IO** —— 它是纯函数，所以上面三条测试不需要任何替身。
    #[test]
    fn validation_is_pure_and_needs_no_dependencies() {
        let ok = DownloadRequestCreateRequest {
            movie_number: "ABC-123".to_owned(),
            indexer_name: "nyaa".to_owned(),
            candidate: DownloadRequestCandidate {
                source_uri: "magnet:?xt=urn:btih:abc".to_owned(),
                title: "t".to_owned(),
                size_bytes: None,
            },
        };
        assert!(validate_request(&ok).is_ok());
    }
}
