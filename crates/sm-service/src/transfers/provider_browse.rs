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

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

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

/// 一条浏览结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseEntry {
    /// **不透明**引用。回传给 `import_from_source` 与下一页的 `parent_ref`。
    pub source_ref: serde_json::Value,
    /// 显示名。
    pub name: String,
    /// 是不是目录。`true` 时前端渲染成可点开的层级。
    pub is_directory: bool,
    /// 文件大小。目录时为 `None`。
    pub size_bytes: Option<i64>,
}

/// 浏览服务。
pub struct ProviderBrowseService;

impl ProviderBrowseService {
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
        payload: ImportBrowseRequest,
    ) -> Result<ImportBrowseResponse, ServiceError> {
        let _ = payload;
        todo!("骨架：解析媒体库的 provider -> browse(parent_ref, cursor, limit) -> 原样透传 next_cursor")
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
                    is_directory: true,
                    size_bytes: None,
                },
                BrowseEntry {
                    source_ref: serde_json::json!({"p": "b.mkv"}),
                    name: "b.mkv".to_owned(),
                    is_directory: false,
                    size_bytes: Some(1024),
                },
            ],
            next_cursor: None,
        };
        assert_eq!(response.entries.len(), 2);
        assert!(response.entries[0].is_directory);
        assert!(!response.entries[1].is_directory);
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
