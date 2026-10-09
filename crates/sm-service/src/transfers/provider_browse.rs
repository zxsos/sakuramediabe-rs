//! provider 侧的不透明源浏览（上游 `imports/provider_browse_service.py`，72 行）。
//!
//! # 200 而不是 404：浏览器列不存在的目录也是「列出来是空的」
//!
//! 这个端点**没有**「路径不存在」的错误 —— provider 的命名空间里
//! 「没有子目录」是正常结果（空列表）。分不清这两种情况就会把「用户输错路径」
//! 报成 404，而实际上只是那一层没有可导入的条目。
//!
//! # 游标是 **provider 的**，宿主不解释
//!
//! `next_cursor` 原样透传。上游的 provider 用它做**不透明**分页（本地 provider
//! 用偏移量，115 provider 可能用 etag）。宿主**不要**解析它、更不要自己拼 ——
//! 那会绑死在一个 provider 的实现上。
//!
//! # 「浏览」是**只读**的，不要加缓存
//!
//! 它可能被高频调用（用户在前端一层层点开），但结果**立即过期**。加缓存会
//! 让用户点了好几层之后看到与磁盘不符的内容，而这里没有任何「快照」语义
//! 支撑它。
//!
//! # ⚠️ 形状还有几处与上游不符（留到本模块接线那一轮）
//!
//! 上游（`schema/transfers/media_import.py:11-30`）：
//!
//! | 上游 | 本模块 |
//! |---|---|
//! | `ImportBrowseRequest { library_id, parent_ref, cursor, limit }` | 同（`library_id` 是 `i64`，线上无差别）|
//! | `ImportBrowseEntryResource { source_ref, name, entry_type: "file"\|"directory", size_bytes, modified_at, is_video }` | `BrowseEntry { source_ref, name, is_directory, size_bytes }` |
//!
//! 差别在**条目**上：上游用 `entry_type` 而不是布尔，且多两个字段 ——
//! 客户端要靠 `is_video` 决定「这一项能不能导」、靠 `entry_type` 渲染图标。
//! 这里只登记不改：浏览端点还没接线（[`ProviderBrowseService::browse`] 仍是
//! `todo!()`）。
//!
//! ✅ **2026-10-09：改形状的前置条件已满足。** `sm_plugins::provider_calls::browse`
//! 已补上（单页，`next_cursor` 透传），它返回的是 proto 的 `BrowsePage` ——
//! 上游那 6 个字段（`source_ref` / `name` / `entry_type` / `size_bytes` /
//! `modified_at` / `is_video`）逐个都在（`common.proto:108-121`），并且由
//! `plugin-ref-local/tests/provider_calls_roundtrip.rs` 用真 provider 验过。
//! 所以接线这一轮**应当**把 [`BrowseEntry`] 改成上游的形状，而不是拖到以后。
//!
//! **没有**第二份定义：`sm-api` 的 `/import-sources/browse` 已经 `use` 本模块
//! 的类型（骨架期那里另有一套 `{source, depth}` / `{items, ...}` 的内联
//! DTO，已删）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sm_db::Db;
use sm_plugin_api::host::{HostProviderError, HostProviderFactory};
use sm_plugin_api::v1::{EntryType, LibraryHandle};

use crate::error::ServiceError;
use crate::transfers::download_common::require_library;

/// 浏览请求。
#[derive(Debug, Clone, Deserialize)]
pub struct ImportBrowseRequest {
    /// 媒体库 id。**必填** —— 浏览器上下文由库决定。
    pub library_id: i64,
    /// 父目录的不透明引用。`None` = 库根目录。
    pub parent_ref: Option<serde_json::Value>,
    /// 每页条数。`None` = provider 默认。
    pub limit: Option<i64>,
    /// 上一页的 `next_cursor`。`None` = 第一页。
    pub cursor: Option<String>,
}

/// 浏览响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportBrowseResponse {
    /// 媒体库 id。回显，方便前端校验响应与自己请求的是否同一个库。
    pub library_id: i64,
    /// 条目。**目录与文件混在一起**（用户要能点进子目录）。
    pub entries: Vec<BrowseEntry>,
    /// 下一页游标。**原样透传，宿主不解析**（见模块文档）。
    pub next_cursor: Option<String>,
}

/// 一条浏览结果。对齐上游 `ImportBrowseEntryResource` / proto `BrowseEntry` 形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseEntry {
    /// **不透明**引用。回传给 `import_from_source` 与下一页的 `parent_ref`。
    pub source_ref: serde_json::Value,
    /// 显示名。
    pub name: String,
    /// 条目类型：`"file"` 或 `"directory"`。前端靠它决定渲染成文件还是可点开的层级。
    pub entry_type: String,
    /// 文件大小。目录时为 `None`。
    pub size_bytes: Option<i64>,
    /// 修改时间（RFC 3339）。目录时为 `None`。
    pub modified_at: Option<String>,
    /// 是否视频文件。客户端靠它决定「能不能导」。
    pub is_video: bool,
}

/// 浏览服务。
///
/// # 插件注入
///
/// `provider_factory` 是 `Option`：`None` = 组合根没注入（没装插件），
/// `browse` 直接报 503 `provider_not_installed`。与 `AppState` 里那几个
/// `Option<Arc<dyn ...>>` 同一个理由 —— 缺省也能构造，单测不依赖插件。
pub struct ProviderBrowseService {
    db: Db,
    provider_factory: Option<Arc<dyn HostProviderFactory>>,
}

impl ProviderBrowseService {
    /// 构造。
    pub fn new(db: Db, provider_factory: Option<Arc<dyn HostProviderFactory>>) -> Self {
        Self {
            db,
            provider_factory,
        }
    }
    /// ★ 浏览。上游 `browse(cls, payload)`。
    ///
    /// 错误码：
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 媒体库不存在 | `404 media_library_not_found` |
    /// | 插件未装 | `503 provider_not_installed` |
    /// | provider 抛错 | `provider_{code}` |
    /// | provider 返回了非法结构 | `502 provider_invalid_response` |
    /// | 浏览本身失败 | `502 provider_browse_failed` |
    ///
    /// 注意最后两条的区别：`provider_browse_failed` 是「浏览这个动作失败」，
    /// `provider_invalid_response` 是「provider 返回的东西我们解不了」。
    /// 前者更笼统，后者说明是契约被破坏 —— 客户端要靠它们区分「重试」与
    /// 「上报插件 bug」。
    pub async fn browse(
        &self,
        payload: ImportBrowseRequest,
    ) -> Result<ImportBrowseResponse, ServiceError> {
        // 1. 查媒体库（404）。
        let library = require_library(&self.db, payload.library_id as i32).await?;

        // 2. 取 provider（没注入 factory = 没装插件 → 503）。
        let factory = self.provider_factory.as_ref().ok_or_else(|| {
            ServiceError::unavailable("provider_not_installed", "媒体提供方未安装")
        })?;
        let provider = factory
            .for_provider_key(&library.provider_key)
            .await
            .map_err(|err| map_host_error(&err))?;

        // 3. 调插件。
        let handle = LibraryHandle {
            library_id: library.id as i64,
            provider_key: library.provider_key.clone(),
            provider_config: sm_plugin_api::json_struct::json_to_struct(
                &library.provider_config,
            ),
            account_key: None,
        };
        let page = provider
            .browse(
                handle,
                payload.parent_ref,
                payload.cursor,
                payload.limit.unwrap_or(100) as i32,
            )
            .await
            .map_err(|err| map_host_error(&err))?;

        // 4. 转条目（`entry_type` / `modified_at` / `is_video` 已对齐上游）。
        let entries = page
            .entries
            .into_iter()
            .map(|entry| {
                let entry_type = match entry.entry_type() {
                    EntryType::File => "file",
                    EntryType::Directory => "directory",
                    EntryType::Unspecified => "file",
                }
                .to_owned();
                BrowseEntry {
                    source_ref: sm_plugin_api::json_struct::struct_to_json(
                        entry.source_ref.as_ref(),
                    ),
                    name: entry.name,
                    entry_type,
                    size_bytes: entry.size_bytes,
                    modified_at: entry.modified_at,
                    is_video: entry.is_video,
                }
            })
            .collect();

        // 5. `next_cursor` 原样透传。
        Ok(ImportBrowseResponse {
            library_id: payload.library_id,
            entries,
            next_cursor: page.next_cursor,
        })
    }
}

/// 把 [`HostProviderError`] 映射成 [`ServiceError`]。
///
/// 映射表（按上游 `ProviderOperationError.code` 的语义）：
///
/// | code | 状态码 | 说明 |
/// |---|---|---|
/// | `unavailable` | 503 `provider_not_installed` | 插件没装 / 连不上 |
/// | `invalid_config` | 422 | 配置问题 |
/// | `authentication_failed` | 401 | 认证失败 |
/// | `source_not_found` | 404 | 远端对象不在（浏览里一般不会出现） |
/// | `unsupported` | 502 `provider_invalid_response` | 插件不支持（契约被破坏） |
/// | 其余 | 502 `provider_browse_failed` | 浏览本身失败 |
fn map_host_error(err: &HostProviderError) -> ServiceError {
    // 插件没装 / 连不上 → 503。
    if err.is_not_installed() {
        return ServiceError::unavailable("provider_not_installed", "媒体提供方未安装");
    }
    // provider_{code}：按码分状态。
    let code = format!("provider_{}", err.code);
    match err.code.as_str() {
        "invalid_config" => ServiceError::from_status(422, code, err.safe_message.clone()),
        "authentication_failed" => ServiceError::from_status(401, code, err.safe_message.clone()),
        "source_not_found" => ServiceError::from_status(404, code, err.safe_message.clone()),
        "unsupported" => {
            // 插件说不支持 —— 这是契约被破坏（它声明了能力却调不动）。
            ServiceError::bad_gateway(
                "provider_invalid_response",
                "媒体提供方返回了非法结构",
                {
                    let mut details = serde_json::Map::new();
                    details.insert(
                        "provider_code".to_owned(),
                        serde_json::Value::from(err.code.clone()),
                    );
                    details
                },
            )
        }
        _ => ServiceError::bad_gateway(
            "provider_browse_failed",
            err.safe_message.clone(),
            {
                let mut details = serde_json::Map::new();
                details.insert(
                    "provider_code".to_owned(),
                    serde_json::Value::from(err.code.clone()),
                );
                details
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目录与文件**混在一个列表**里 —— 前端要能点进子目录。
    ///
    /// 拆成两个字段（`dirs` + `files`）会破坏 provider 的返回顺序，
    /// 而那个顺序往往是 provider 建议的「优先看什么」。
    #[test]
    fn directories_and_files_share_one_ordered_list() {
        let response = ImportBrowseResponse {
            library_id: 1,
            entries: vec![
                BrowseEntry {
                    source_ref: serde_json::json!({"p": "a"}),
                    name: "演员合集".to_owned(),
                    entry_type: "directory".to_owned(),
                    size_bytes: None,
                    modified_at: None,
                    is_video: false,
                },
                BrowseEntry {
                    source_ref: serde_json::json!({"p": "b.mkv"}),
                    name: "b.mkv".to_owned(),
                    entry_type: "file".to_owned(),
                    size_bytes: Some(1024),
                    modified_at: Some("2026-10-09T00:00:00Z".to_owned()),
                    is_video: true,
                },
            ],
            next_cursor: None,
        };
        assert_eq!(response.entries.len(), 2);
        assert_eq!(response.entries[0].entry_type, "directory");
        assert_eq!(response.entries[1].entry_type, "file");
        assert!(response.entries[1].is_video);
        assert!(!response.entries[0].is_video);
        assert!(response.entries[1].size_bytes.is_some());
        assert!(response.entries[0].size_bytes.is_none(), "目录没有大小");
    }

    /// `library_id` 必须**回显** —— 否则前端无法确认响应与自己请求的是同一个库。
    #[test]
    fn the_library_id_is_echoed_back() {
        let response = ImportBrowseResponse {
            library_id: 42,
            entries: Vec::new(),
            next_cursor: None,
        };
        assert_eq!(response.library_id, 42);
    }

    /// 游标是**不透明**的：宿主不解析，原样透传。
    #[test]
    fn the_cursor_is_opaque_and_passed_through() {
        // 用一个宿主「看不懂」的内容当游标 —— 它必须能原样存在。
        let weird = "eyJvIjoxLCJhIjoi/9rPS1jPT0ifQ==";
        let response = ImportBrowseResponse {
            library_id: 1,
            entries: Vec::new(),
            next_cursor: Some(weird.to_owned()),
        };
        assert_eq!(response.next_cursor.as_deref(), Some(weird));
    }
}
