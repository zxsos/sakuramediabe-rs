//! 插件句柄工厂（上游 `playback/provider_helpers.py`，25 行）。
//!
//! # 只有 25 行，但它是**整个 playback 域的入口**
//!
//! ```python
//! def library_handle_for(library: MediaLibrary) -> LibraryHandle
//! def media_handle_for(media: Media) -> MediaHandle
//! ```
//!
//! 两个函数都**不发任何请求** —— 它们只从 DB 记录构造句柄：`deepcopy` 一份
//! `provider_config` / `storage_ref`，带上 provider key。
//!
//! # ★ `deepcopy` 不是细节，是**安全边界**
//!
//! 上游显式 `deepcopy(provider_config)`。原因是 `provider_config` 来自
//! `jsonb`，而句柄会被交给 provider 插件；若不拷贝，插件一次 in-place 修改
//! 就可能**改到调用方的数据**（而调用方往往还拿着它做别的事）。
//!
//! Rust 侧对应 `Clone`（`serde_json::Value` 的深拷贝）—— 语义一致。
//!
//! # 句柄里**没有**密钥
//!
//! `provider_config` 是插件的**配置**（库 id、路径前缀…），凭据在插件自己的
//! 进程内（见 `docs/adr/2026-10-05-plugin-lifecycle.md`）。所以句柄可以
//! 安全地跨线程传递、日志里打印。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 媒体库句柄。**发给插件**的形态。
///
/// 字段与 `proto/common.proto` 的 `LibraryHandle` **同名同序**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryHandle {
    /// 库 id（宿主侧主键）。
    pub library_id: i64,
    /// provider 键（`local` / `115` / …）。插件据此选实现。
    pub provider_key: String,
    /// 插件配置。**已深拷贝**（见模块文档）。
    pub provider_config: serde_json::Value,
    /// 多账号存储（115 等）用它区分 cookie 归属。**可空** —— 单账号库没有它。
    pub account_key: Option<String>,
}

/// 媒体句柄。
///
/// 字段与 `proto/common.proto` 的 `MediaHandle` 对齐（那个消息把 `library`
/// 嵌在里头，这里是**展平**的 —— 见 [`MediaRecord`] 的说明）。
///
/// # ★ `file_name` / `duration_seconds` 不是装饰
///
/// `plugin-ref-local` 的 `generate_thumbnails` 两个都要用：
/// `duration_seconds` 决定**生成几张**，`file_name` 决定产物叫什么名字
/// （`file_stem_of`）；`media_path` 在 `storage_ref` 没有 path 时也**回退到
/// `file_name`**。缺了它们，provider 会「成功」地生成出 0 张、或写到别的名字
/// 上 —— 宿主侧看起来是「这轮没产物」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaHandle {
    pub media_id: i64,
    pub library_id: i64,
    /// provider 键。**冗余**（也能从 library 查到）—— 但省一次查询，
    /// 且插件处理单个媒体时不该被迫回查库。
    pub provider_key: String,
    /// 宿主侧的存储引用。**宿主不解释它的内容**（是 provider 的命名空间）。
    pub storage_ref: serde_json::Value,
    /// 该媒体的库配置。**深拷贝**。
    pub provider_config: serde_json::Value,
    /// 库的账号标识。**可空**。
    pub account_key: Option<String>,
    pub file_name: String,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
}

/// 媒体库的最小投影（构造句柄所需）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryRecord {
    pub id: i64,
    pub provider_key: String,
    pub provider_config: serde_json::Value,
    /// 多账号存储用它区分 cookie 归属。
    pub account_key: Option<String>,
}

/// 媒体的最小投影（构造句柄所需）。
///
/// # ★ 这个投影**展平了**库的两个字段，与上游不同
///
/// 上游 `media_handle_for(media)` 是 `media.library` 直接拿得到整条库记录
/// （ORM 关系），所以 `MediaHandle.library` 是**嵌套**的。Rust 侧
/// `sm_db::Media` 只有 `library_id`，库要单独查 —— 于是这里把库的三个字段
/// 展平进来，由调用方 JOIN/回查后填。
///
/// ⚠️ 后果是「忘了填库字段」**不会编译报错**（字段是必填的，会报错；但填错
/// 来源不会）。所以约定：**先查库记录，再照它填**，见
/// `thumbnails::task_service::generate_one` 里的注释。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRecord {
    pub id: i64,
    pub library_id: i64,
    pub storage_ref: serde_json::Value,
    /// 所属库的配置。**由调用方 JOIN 出来** —— 构造句柄需要它。
    pub provider_config: serde_json::Value,
    /// 所属库的 provider 键。同上。
    pub provider_key: String,
    /// 所属库的账号标识。同上，可空。
    pub account_key: Option<String>,
    pub file_name: String,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
}

/// 构造媒体库句柄。**纯函数**。
pub fn library_handle_for(library: &LibraryRecord) -> LibraryHandle {
    LibraryHandle {
        library_id: library.id,
        provider_key: library.provider_key.clone(),
        // `Clone` 即深拷贝：插件改不到我们的数据。
        provider_config: library.provider_config.clone(),
        account_key: library.account_key.clone(),
    }
}

/// 构造媒体句柄。**纯函数**。
pub fn media_handle_for(media: &MediaRecord) -> MediaHandle {
    MediaHandle {
        media_id: media.id,
        library_id: media.library_id,
        provider_key: media.provider_key.clone(),
        storage_ref: media.storage_ref.clone(),
        provider_config: media.provider_config.clone(),
        account_key: media.account_key.clone(),
        file_name: media.file_name.clone(),
        file_size_bytes: media.file_size_bytes,
        duration_seconds: media.duration_seconds,
    }
}

/// 把库里存的不透明 JSON 文本解析成 `Value`；空 / 解析失败给 `Null`。
///
/// **不报错**：`storage_ref` 与 `provider_config` 都是 provider 的私有命名空间，
/// 宿主**不解释**它的内容 —— 解析不出来就原样传 `Null`，让 provider 自己决定
/// 怎么处置。反之若在这里报错，「一个字段脏了」会让整部片子拿不到缩略图。
///
/// 放在这里而不是某个服务里：构造句柄的**两条**路径都要它
/// （`MediaService::delete_media` 与 `thumbnails::task_service`），
/// 各写一份就会在「空值到底给 `Null` 还是 `{}`」上分叉。
pub fn json_or_null(raw: Option<&str>) -> serde_json::Value {
    raw.and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// provider 报的失败。
///
/// `code` 用**上游那七个字面量**（`thumbnails::task_service::TERMINAL_ERROR_CODES`
/// 那段有完整说明）—— 宿主据此分流（终态 / 失败轨 / 延迟轨）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub code: String,
    /// 对外展示的安全文案。**不含插件的内部路径或凭据。**
    pub safe_message: String,
    /// 是否值得稍后再试。宿主的延迟轨靠它 ——
    /// 上游 `retryable` 是 provider 给的独立字段。
    pub retryable: bool,
}

/// provider 数据面的调用能力。
///
/// # ★ 为什么是个 trait 而不是直接调 `sm-plugins`
///
/// `sm-service` **不能**依赖 `sm-plugins`：依赖方向会变成
/// `sm-plugins → sm-scheduler → sm-service → sm-plugins`，**成环**。
/// 所以这里声明能力，由**组合根**（`sm-server`，它同时看得见两边）注入实现 ——
/// 与 `RankingSourceCatalog` 同一个取向。
///
/// 这是 playback 域**唯一**的插件接缝：不要在任何别处再造一个（此前
/// `thumbnails::task_service` 自己造过一个 `ThumbnailGenerator`，已合并到这里）。
pub trait StorageGateway: Send + Sync {
    /// 该 provider 是否已安装。
    fn has_provider(&self, provider_key: &str) -> bool;

    /// 删掉远端媒体文件。**只删远端** —— 本地记录与图片由调用方负责。
    fn delete_media(
        &self,
        handle: &MediaHandle,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ProviderFailure>> + Send + '_>>;

    /// 生成缩略图，产物写进宿主提供的 `workspace`。
    fn generate_thumbnails(
        &self,
        handle: &MediaHandle,
        workspace: &std::path::Path,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ThumbnailJobResult, ProviderFailure>>
                + Send
                + '_,
        >,
    >;
}

/// 一件缩略图产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailJobArtifact {
    /// 在媒体里的偏移（秒）。
    pub offset_seconds: i32,
    /// **相对 `workspace`** 的路径。宿主拿它拼绝对路径。
    pub relative_path: String,
}

/// 一次缩略图生成的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailJobResult {
    /// provider 期望生成多少张。
    pub expected_count: u32,
    pub artifacts: Vec<ThumbnailJobArtifact>,
}

/// 取 provider 能力。未安装该 provider → **503 `provider_not_installed`**。
///
/// ⚠️ 503 而不是 404：库记录**存在**，只是负责它的插件没装。这是配置/
/// 部署问题，客户端该提示「去装插件」，不是「资源不存在」。
///
/// `gateway` 为 `None` 时一律 503 —— 组合根没注入就等价于「一个插件都没装」，
/// 而不是「都装了」（后者会让每个调用都以一种更隐蔽的方式失败）。
pub fn require_provider(
    gateway: Option<&dyn StorageGateway>,
    provider_key: &str,
) -> Result<PluginStorageProvider, ServiceError> {
    let installed = gateway.is_some_and(|gateway| gateway.has_provider(provider_key));
    if !installed {
        return Err(ServiceError::unavailable(
            "provider_not_installed",
            "媒体提供方未安装",
        ));
    }
    Ok(PluginStorageProvider {
        provider_key: provider_key.to_owned(),
    })
}

/// 插件的存储能力。**形状待插件 ABI 定型**。
///
/// ★ 这里刻意用 **struct** 而不是 `trait`：它出现在
/// [`require_provider`] 的返回类型位置上，而 trait 不能作返回类型
/// （需要 `dyn Trait` 或泛型，而那时我们还没有可用的具体实现）。
///
/// 真正的多态接入（按 `provider_key` 分发到不同插件）是 `sm-plugins` 的事；
/// 那一层落地后这里会换成持有 gRPC client 的 struct。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStorageProvider {
    pub provider_key: String,
}

/// 空间占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUsage {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_record() -> MediaRecord {
        MediaRecord {
            id: 7,
            library_id: 3,
            storage_ref: serde_json::json!({"path": "a/b.mp4"}),
            provider_config: serde_json::json!({"root": "/mnt"}),
            provider_key: "local".to_owned(),
            account_key: Some("acct-1".to_owned()),
            file_name: "ABC-001.mp4".to_owned(),
            file_size_bytes: 4_096,
            duration_seconds: 3_600,
        }
    }

    /// ★ `file_name` / `duration_seconds` / `file_size_bytes` 必须过线。
    ///
    /// `plugin-ref-local` 靠 `duration_seconds` 算生成几张、靠 `file_name` 给产物
    /// 命名，而 `media_path` 在 `storage_ref` 没有 path 时**回退到 `file_name`**。
    /// 把它们漏掉的表现是「provider 报成功但 0 张」—— 排查方向会被带偏到插件。
    #[test]
    fn the_media_handle_carries_what_the_plugin_actually_reads() {
        let handle = media_handle_for(&media_record());
        assert_eq!(handle.media_id, 7);
        assert_eq!(handle.library_id, 3);
        assert_eq!(handle.file_name, "ABC-001.mp4");
        assert_eq!(handle.duration_seconds, 3_600);
        assert_eq!(handle.file_size_bytes, 4_096);
        assert_eq!(handle.account_key.as_deref(), Some("acct-1"));
        assert_eq!(handle.provider_key, "local");
        assert_eq!(handle.storage_ref, serde_json::json!({"path": "a/b.mp4"}));
        assert_eq!(handle.provider_config, serde_json::json!({"root": "/mnt"}));
    }

    /// ★ `provider_config` 必须是**深拷贝**（上游显式 `deepcopy`）。
    ///
    /// 句柄会交到插件手里；若只是借用，插件一次 in-place 修改就会改到调用方的
    /// 数据，而调用方往往还拿着它做别的事。
    #[test]
    fn the_handles_do_not_alias_the_caller_config() {
        let mut record = media_record();
        let handle = media_handle_for(&record);

        record.provider_config["root"] = serde_json::json!("/elsewhere");
        record.storage_ref["path"] = serde_json::json!("other.mp4");

        assert_eq!(
            handle.provider_config,
            serde_json::json!({"root": "/mnt"}),
            "句柄里的配置不该跟着源记录变"
        );
        assert_eq!(handle.storage_ref, serde_json::json!({"path": "a/b.mp4"}));
    }

    /// 库句柄的字段与上游 `library_handle_for` 一致（含可空的 `account_key`）。
    #[test]
    fn the_library_handle_carries_the_account_key() {
        let with_account = library_handle_for(&LibraryRecord {
            id: 3,
            provider_key: "local".to_owned(),
            provider_config: serde_json::json!({}),
            account_key: Some("acct-1".to_owned()),
        });
        assert_eq!(with_account.account_key.as_deref(), Some("acct-1"));

        let without = library_handle_for(&LibraryRecord {
            id: 4,
            provider_key: "local".to_owned(),
            provider_config: serde_json::json!({}),
            account_key: None,
        });
        assert!(without.account_key.is_none(), "单账号库就是没有它");
    }

    /// 没注入/没装插件 → 503 `provider_not_installed`（不是 404）。
    ///
    /// 库记录存在、只是负责它的插件没装：客户端该提示「去装插件」。
    #[test]
    fn a_missing_gateway_is_reported_as_not_installed() {
        let error = require_provider(None, "local").expect_err("没注入就是没装");
        assert_eq!(error.status, 503);
        assert_eq!(error.code(), "provider_not_installed");
    }
}
