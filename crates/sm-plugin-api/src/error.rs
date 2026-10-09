//! provider 的**结构化错误**怎么过 gRPC 线。
//!
//! # 上游对应
//!
//! `ProviderOperationError`（`src/plugins/provider_protocol.py:254-302`）有
//! `provider_key` / `operation` / `code` / `safe_message` / `retryable` 五个
//! 字段，其中 **`code` 只有 7 个取值**（`:287-295`），调用方按它分支：
//!
//! | code | 调用方处置（上游出处） |
//! |---|---|
//! | `source_not_found` | 远端对象已不在 → **继续**（`delete_media` 的后续清理照做）|
//! | `unavailable` | 稍后再试（缩略图任务据此走延迟轨）|
//! | 其余五个 | 确定性失败，重试没有意义 |
//!
//! ⚠️ `retryable` 是 **provider 给的独立字段**，不是从 `code` 推出来的
//! （`provider_protocol.py:661-665` 里 `unsupported` 显式给 `retryable=False`）。
//! 所以宿主**优先信它**，只有在拿不到结构化错误时才退回
//! [`default_retryable`] 那份猜测。
//!
//! # 为什么需要这个模块
//!
//! `ProviderError` 消息 proto 里**已经有了**（`proto/common.proto:322-329`），
//! 但 rpc 的**成功响应**里没有它的位置 —— 失败只能经 gRPC `Status` 表达。
//! 于是把它编进 `Status` 的 `details` 字节里：
//!
//! ```text
//! Status { code, message, details: <ProviderError 的 prost 编码> }
//! ```
//!
//! # 为什么用 `details` 而不是改 rpc 签名
//!
//! 改签名要动 30+ 个方法（`storage.proto` 的 `StorageProvider` 就有 32 个
//! rpc），而失败路径在每个方法里都是同一个形状。`details` 是 gRPC 专门留给
//! 「结构化错误」的通道（`google.rpc.Status` 就是这么用的），**不需要改任何
//! 现有 rpc**。
//!
//! # 给插件作者
//!
//! 失败时调 [`to_status`] 而不是手写
//! `Status::unimplemented(...)` —— 前者让宿主能分清「你不支持这个操作」与
//! 「你崩了」，后者在宿主侧一律归成 `unspecified`（认不出的失败）。
//! `safe_message` 会展示给用户，**不要**放 Cookie、密码或内部路径（proto 注释
//! 的原话）。
//!
//! # 给宿主
//!
//! [`from_status`] 解不出时返回 `None`，调用方
//! **必须**还有一条按 `Status::code` 猜的回落路径 —— 老插件与手写 `Status` 的
//! 插件不会带这个结构。

use prost::Message;
use tonic::codegen::Bytes;
use tonic::{Code, Status};

use crate::v1::{ProviderError, ProviderErrorCode};

/// 7 个码的对外字符串，**与上游逐字一致**（外加 `UNSPECIFIED` 兜底）。
///
/// ⚠️ `unspecified` **不在**上游那七个里 —— 它表示「宿主没认出这个错误」。
/// 调用方必须把它当**未知失败**（500 一类）而不是硬塞进七个之一，否则会把
/// 「插件崩了」说成「你的配置不对」。
pub fn code_name(code: ProviderErrorCode) -> &'static str {
    match code {
        ProviderErrorCode::InvalidConfig => "invalid_config",
        ProviderErrorCode::AuthenticationFailed => "authentication_failed",
        ProviderErrorCode::SourceNotFound => "source_not_found",
        ProviderErrorCode::TaskNotManaged => "task_not_managed",
        ProviderErrorCode::SourceBlacklisted => "source_blacklisted",
        ProviderErrorCode::Unsupported => "unsupported",
        ProviderErrorCode::Unavailable => "unavailable",
        ProviderErrorCode::Unspecified => "unspecified",
    }
}

/// 从字符串反查。认不出的一律 [`ProviderErrorCode::Unspecified`] ——
/// 不认输会把一个未知码当成某个已知码来处置。
pub fn code_from_name(name: &str) -> ProviderErrorCode {
    match name.trim() {
        "invalid_config" => ProviderErrorCode::InvalidConfig,
        "authentication_failed" => ProviderErrorCode::AuthenticationFailed,
        "source_not_found" => ProviderErrorCode::SourceNotFound,
        "task_not_managed" => ProviderErrorCode::TaskNotManaged,
        "source_blacklisted" => ProviderErrorCode::SourceBlacklisted,
        "unsupported" => ProviderErrorCode::Unsupported,
        "unavailable" => ProviderErrorCode::Unavailable,
        _ => ProviderErrorCode::Unspecified,
    }
}

/// 该码**默认**是否值得重试 —— **只在 provider 没给 `retryable` 时**才用这个。
///
/// # 这是猜的，而上游不是
///
/// 上游的 `retryable` 是 provider 显式给的字段；这里是按语义近似：
///
/// | code | 猜 | 理由 |
/// |---|---|---|
/// | `unavailable` | **是** | 网络抖动、后端重启 —— 上游缩略图任务正是靠它走延迟轨 |
/// | `source_not_found` | 否 | 远端对象不在，重试还是不在（**除非**是挂载点还没就绪 —— 那种情况上游由 provider 显式给 `retryable=true`）|
/// | 其余 | 否 | 配置 / 认证 / 黑名单 / 不支持 —— 全是确定性失败 |
///
/// 猜错的代价是**不对称的**：把「该重试的」判成不该重试 → 差一次重试机会；
/// 反过来把确定性失败判成该重试 → 每轮都白烧一次 provider 调用（而缩略图任务
/// 的重试有次数上限，最终仍会进终态）。所以这里**偏保守**。
pub fn default_retryable(code: ProviderErrorCode) -> bool {
    matches!(code, ProviderErrorCode::Unavailable)
}

/// 把结构化错误编进 gRPC status 的 `details`。
///
/// # `message` 用 `safe_message`
///
/// 它是 proto 定义的「对外展示的安全文案」；空串时回落 gRPC 码的默认文案，
/// 免得 status 里挂一句空话。
///
/// # 编码失败不会发生
///
/// `encode` 到 `Vec` 只在内存不足时失败，而那时进程已经没救了 —— 与其让
/// 错误路径再抛一个错误，不如就地放弃结构（回落成普通 status）。
pub fn to_status(error: &ProviderError, grpc: Code) -> Status {
    let mut bytes = Vec::with_capacity(error.encoded_len());
    if error.encode(&mut bytes).is_err() {
        return Status::new(grpc, default_message(grpc));
    }
    let message = match error.safe_message.as_str() {
        "" => default_message(grpc),
        text => text,
    };
    Status::with_details(grpc, message.to_owned(), Bytes::from(bytes))
}

/// 从 status 的 `details` 还原结构化错误。
///
/// 解不出（没有 details / 不是 `ProviderError` 的编码）返回 `None` ——
/// **调用方必须有回落路径**：老插件与手写 `Status` 的插件不带这个结构。
pub fn from_status(status: &Status) -> Option<ProviderError> {
    let details = status.details();
    if details.is_empty() {
        return None;
    }
    ProviderError::decode(details).ok()
}

/// gRPC 码的默认文案（只在插件没给 `safe_message` 时用）。
fn default_message(code: Code) -> &'static str {
    match code {
        Code::Unimplemented => "该操作未实现",
        Code::NotFound => "远端对象不存在",
        Code::Unauthenticated | Code::PermissionDenied => "认证或权限不足",
        Code::InvalidArgument => "请求参数无效",
        Code::Unavailable | Code::DeadlineExceeded => "服务暂不可用",
        _ => "操作失败",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(code: ProviderErrorCode, retryable: bool) -> ProviderError {
        ProviderError {
            provider_key: "local".to_owned(),
            operation: "delete_media".to_owned(),
            code: code as i32,
            safe_message: "本地源暂不可用".to_owned(),
            retryable,
        }
    }

    /// ★ 结构化错误**往返无损** —— 码与 `retryable` 都要能过线。
    ///
    /// 这两个字段正是之前「过不了线」的东西：宿主只能按 gRPC 码猜 `code`，
    /// `retryable` 则完全没有来源。
    #[test]
    fn a_structured_error_survives_the_round_trip() {
        let sent = error(ProviderErrorCode::Unavailable, true);
        let status = to_status(&sent, Code::Unavailable);
        let received = from_status(&status).expect("应当解出");
        assert_eq!(received.provider_key, "local");
        assert_eq!(received.operation, "delete_media");
        assert_eq!(received.code, ProviderErrorCode::Unavailable as i32);
        assert!(received.retryable, "provider 给的 retryable 要过线");
        // 对外文案也过线了 —— 宿主不必再拿自己的模板猜。
        assert_eq!(status.message(), "本地源暂不可用");
    }

    /// 没有 details（老插件 / 手写 `Status`）→ `None`，**不是**一个默认值。
    ///
    /// 返回默认值会让宿主以为「provider 说不可用」，而实际上它什么都没说。
    #[test]
    fn a_plain_status_yields_nothing() {
        assert!(from_status(&Status::new(Code::Internal, "插件炸了")).is_none());
        // 编不出结构的 details 同样当作没有。
        let junk = Status::with_details(
            Code::Internal,
            "乱写的",
            Bytes::from_static(b"\xff\xfe\xfd"),
        );
        assert!(from_status(&junk).is_none());
    }

    /// 七个码的名字与上游逐字一致；`unspecified` 是宿主侧的兜底。
    #[test]
    fn the_code_names_match_upstream() {
        let names: Vec<&str> = [
            ProviderErrorCode::InvalidConfig,
            ProviderErrorCode::AuthenticationFailed,
            ProviderErrorCode::SourceNotFound,
            ProviderErrorCode::TaskNotManaged,
            ProviderErrorCode::SourceBlacklisted,
            ProviderErrorCode::Unsupported,
            ProviderErrorCode::Unavailable,
        ]
        .iter()
        .map(|code| code_name(*code))
        .collect();
        assert_eq!(
            names,
            [
                "invalid_config",
                "authentication_failed",
                "source_not_found",
                "task_not_managed",
                "source_blacklisted",
                "unsupported",
                "unavailable",
            ]
        );
        // 反查：认不出的一律 Unspecified，绝不猜成某个已知码。
        assert_eq!(
            code_from_name("source_not_found"),
            ProviderErrorCode::SourceNotFound
        );
        assert_eq!(code_from_name("nonsense"), ProviderErrorCode::Unspecified);
    }

    /// `retryable` 的默认猜测**偏保守**：只有 `unavailable` 算值得重试。
    #[test]
    fn only_unavailable_is_retryable_by_default() {
        assert!(default_retryable(ProviderErrorCode::Unavailable));
        assert!(!default_retryable(ProviderErrorCode::SourceNotFound));
        assert!(!default_retryable(ProviderErrorCode::Unspecified));
    }
}
