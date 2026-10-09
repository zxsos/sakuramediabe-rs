//! 两个扩展点的**调用面**：真的把 rpc 发出去。
//!
//! # 上游对应
//!
//! `MetadataSourceExtensionService.FetchMovie` 与
//! `RankingSourceExtensionService.FetchRanking`。声明的收集与校验在
//! [`crate::extensions`]，这里只管「发一次调用、把结果收敛成宿主能用的形状」。
//!
//! # 通道复用控制面那条
//!
//! 两个扩展点服务与 `PluginControl` 由**同一个插件进程**提供（proto 里它们
//! 各是一个 service，但没有任何字段声明另一个端口 —— 只有数据面才有
//! `data_plane_endpoint`）。所以客户端用 [`crate::loader::connect`] 拿到的
//! 那条 `Channel` 建即可：`Client::new(channel)`。
//!
//! # 「没收录」不是错误
//!
//! proto 写在 `FetchMovieResponse.found` 上：「未收录时返回空响应，宿主会尝试
//! 下一个来源」。所以 `found = false` 是**正常结果**，与「插件调用失败」是
//! 两回事 —— 前者继续试下一个来源，后者记一条失败。这里用 [`MovieLookup`]
//! 把两者分开，调用方没法把空响应当成功。
//!
//! # 交付校验不在这里
//!
//! `FetchMovieResponse` 里的图片路径是**插件填的**，用之前必须确认它们落在宿主
//! 给的 `delivery_dir` 内 —— 那是 [`crate::movie_delivery::validate_movie_delivery`]
//! 的事，单独的模块（判据多、要碰文件系统，混进调用面会让这里变重）。
//!
//! # 超时由 `grpc-timeout` 表达
//!
//! 与 [`crate::runner::run_job`] 的 deadline 同一个取向：宿主不等一个卡住的
//! 插件。`deadline` 为 `None` 时不设上限（上游 Python 侧也是无上限的同步调用）。

use std::time::Duration;

use sm_plugin_api::v1::metadata_source_extension_service_client::MetadataSourceExtensionServiceClient;
use sm_plugin_api::v1::ranking_source_extension_service_client::RankingSourceExtensionServiceClient;
use sm_plugin_api::v1::{
    FetchMovieRequest, FetchMovieResponse, FetchRankingRequest, FetchRankingResponse,
};
use tonic::transport::Channel;

/// 调用一个扩展点时的失败。**区别于「插件说没收录」** —— 那是正常结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionCallError {
    /// gRPC 调用失败（含插件在取数时崩了）。
    Call(String),
    /// 宿主设的时限到了。与「插件不支持」是两回事：值得重试或换来源。
    Timeout,
}

impl ExtensionCallError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Call(_) => "extension_call_failed",
            Self::Timeout => "extension_call_timeout",
        }
    }
}

/// `FetchMovie` 的结果收敛。
#[derive(Debug, Clone, PartialEq)]
pub enum MovieLookup {
    /// 插件收录了这部片子，载荷即 `FetchMovieResponse`。
    ///
    /// 装箱：`FetchMovieResponse` 有十几个 `String` 字段，直接内联会让整个
    /// 枚举大到每次返回都搬一堆字节（clippy 的 `result_large_err`）。
    Found(Box<FetchMovieResponse>),
    /// 插件明确没有（`found = false`）—— **不是失败**，宿主据此试下一个来源。
    NotFound,
}

/// 把 `FetchMovie` 的响应收敛成「收录了 / 没有」。
///
/// 单独做成纯函数是因为这条判定决定了兜底链路往哪走，必须能单测；而发 rpc
/// 需要真的服务端。
pub fn interpret_fetch_movie(response: FetchMovieResponse) -> MovieLookup {
    if response.found {
        MovieLookup::Found(Box::new(response))
    } else {
        MovieLookup::NotFound
    }
}

/// 向一个插件取一次元数据。
pub async fn fetch_movie(
    client: &mut MetadataSourceExtensionServiceClient<Channel>,
    request: FetchMovieRequest,
    deadline: Option<Duration>,
) -> Result<FetchMovieResponse, ExtensionCallError> {
    let mut request = tonic::Request::new(request);
    if let Some(limit) = deadline {
        request.set_timeout(limit);
    }
    client
        .fetch_movie(request)
        .await
        .map(|response| response.into_inner())
        .map_err(classify)
}

/// 向一个插件取一次榜单。返回的番号列表**顺序即排名**。
pub async fn fetch_ranking(
    client: &mut RankingSourceExtensionServiceClient<Channel>,
    request: FetchRankingRequest,
    deadline: Option<Duration>,
) -> Result<FetchRankingResponse, ExtensionCallError> {
    let mut request = tonic::Request::new(request);
    if let Some(limit) = deadline {
        request.set_timeout(limit);
    }
    client
        .fetch_ranking(request)
        .await
        .map(|response| response.into_inner())
        .map_err(classify)
}

/// 把 gRPC 状态归成宿主的错误。
///
/// 只把 `DeadlineExceeded` 单独拎出来：它与「插件内部出错」的处置不同 ——
/// 前者可以换来源或稍后重试，后者重试多半还是错。
fn classify(status: tonic::Status) -> ExtensionCallError {
    if status.code() == tonic::Code::DeadlineExceeded {
        ExtensionCallError::Timeout
    } else {
        ExtensionCallError::Call(status.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_is_a_timeout_not_a_plain_failure() {
        assert_eq!(
            classify(tonic::Status::new(tonic::Code::DeadlineExceeded, "超时")),
            ExtensionCallError::Timeout
        );
        assert_eq!(ExtensionCallError::Timeout.code(), "extension_call_timeout");
    }

    #[test]
    fn any_other_status_is_a_call_failure() {
        let error = classify(tonic::Status::new(tonic::Code::Internal, "插件炸了"));
        assert_eq!(error.code(), "extension_call_failed");
        // 消息里要同时有状态码与插件给的那句话 —— 只有后者的话，日志里看不出
        // 是插件报的错还是网络断了。
        let ExtensionCallError::Call(detail) = &error else {
            panic!("应当归为 Call：{error:?}");
        };
        assert!(detail.contains("Internal"), "{detail}");
        assert!(detail.contains("插件炸了"), "{detail}");
    }

    #[test]
    fn an_empty_response_means_not_found_not_success() {
        // `found = false` 是「未收录」：兜底链路据此试下一个来源，
        // 当成成功会让「一部片子都没找到」看起来像成功。
        assert_eq!(
            interpret_fetch_movie(FetchMovieResponse::default()),
            MovieLookup::NotFound
        );
        let found = FetchMovieResponse {
            found: true,
            movie_number: "ABC-123".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            interpret_fetch_movie(found.clone()),
            MovieLookup::Found(Box::new(found))
        );
    }
}
