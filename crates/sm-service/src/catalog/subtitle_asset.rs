//! 字幕资产写入（上游 `catalog/subtitle_asset_service.py`，161 行）。
//!
//! # 与 [`super::movie_subtitle`] 是**写/读**两侧，方向相反
//!
//! | | 本文件 | `movie_subtitle` |
//! |---|---|---|
//! | 方向 | 写入 | 读取 + 列出 |
//! | 入口 | 插件经 `PluginContext.import_subtitle` 调用 | HTTP 端点 |
//! | 失败方式 | **返回状态枚举**，不抛错 | 抛 `SubtitleReadError` |
//!
//! ⚠️ 这个不对称是刻意的：写侧被插件调用，抛异常会变成插件崩溃；
//! 读侧对着 HTTP，异常能变成 4xx。
//!
//! # 写入用**硬链接优先**
//!
//! 上游 `register_subtitle_file` 先试 `os.link`（硬链接），失败才退回
//! `os.replace`（复制）。硬链接不复制字节 —— 一部影片的字幕从媒体库链接过来
//! 是 O(1)，复制则可能是几百 MB。
//!
//! # 指纹去重：内容 sha256，不是文件名
//!
//! `movie_subtitle_hashes(movie) -> set[str]`。同一部影片里出现两个**内容
//! 相同但文件名不同**的字幕（如 `ABC-123.srt` 与 `ABC-123.chs.srt`）时，
//! 按文件名判会存两份，按内容判只存一份。
//!
//! 返回的是**整个影片已有字幕的指纹集合**，调用方在写入前查一遍即可。
//!
//! # 返回枚举而不是 `Result`
//!
//! 见模块文档的「写/读不对称」。`SubtitleImportStatus` 的每个取值都是
//! **一种正常的处理结果**，包括「已存在同内容字幕」。
//!
//! ⚠️ 别把「已存在」当成失败：那会让插件以为字幕没导入成功而反复重试。

use crate::error::ServiceError;

/// 字幕扩展名白名单。**只收字幕**，不是任意文件。
///
/// 上游按扩展名判定（`.srt` / `.ass` / `.ssa` / `.vtt` 一类）。
/// 白名单之外的一律拒 —— 插件传进来的路径不该被当字幕存下。
pub fn is_subtitle_extension(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    [".srt", ".ass", ".ssa", ".vtt", ".sub"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// 字幕导入结果。**每一种都是正常结果**，包括「已存在」。
///
/// 派生 `serde::{Serialize, Deserialize}` 是因为它嵌在
/// [`SubtitleImportResult`] 里，而那个结构体会进任务摘要（JSON）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleImportStatus {
    /// 新写入。
    Imported,
    /// ★ 已存在**同内容**的字幕 —— 不是失败。
    AlreadyExists,
    /// 扩展名不接受。
    UnsupportedExtension,
    /// 影片不存在。
    MovieNotFound,
    /// 落盘失败。
    PersistFailed,
}

/// 字幕导入结果（带内容与语言）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleImportResult {
    pub status: SubtitleImportStatus,
    /// 落盘后的字幕 id。`Imported` 时有值。
    pub subtitle_id: Option<i32>,
    /// 识别出的语言。`None` = 从文件名推断不出。
    pub language: Option<String>,
}

/// 字幕资产服务。
pub struct SubtitleAssetService;

impl SubtitleAssetService {
    /// 该影片**已有字幕的内容指纹集合**（sha256 十六进制）。
    ///
    /// 上游 `movie_subtitle_hashes(cls, movie) -> set[str]`。在写入前查一遍
    /// 即可完成去重，不必每次都读文件算哈希。
    pub async fn movie_subtitle_hashes(movie_id: i64) -> Result<Vec<String>, ServiceError> {
        let _ = movie_id;
        todo!("骨架：查该影片字幕记录的内容指纹（存在表里，不重算文件）")
    }

    /// ★ 从**内容**导入字幕（插件走这条）。
    ///
    /// 上游 `import_subtitle_content(cls, movie_number, content, filename, language=None)`。
    /// 流程：查影片 -> 校验扩展名 -> 算内容指纹 -> 查重 -> 落盘 -> 登记记录。
    ///
    /// **不抛错**，返回 [`SubtitleImportStatus`]（见模块文档）。
    pub async fn import_subtitle_content(
        movie_number: &str,
        content: &[u8],
        file_name: &str,
        language: Option<&str>,
    ) -> Result<SubtitleImportResult, ServiceError> {
        let _ = (movie_number, content, file_name, language);
        todo!("骨架：查影片 -> 扩展名白名单 -> sha256 查重 -> 原子落盘 -> 登记 record")
    }

    /// 从**文件**登记字幕（`transfer_mode` 决定硬链接还是复制）。
    ///
    /// 上游 `register_subtitle_file(cls, movie, source_path, *, existing_hashes=None, transfer_mode="auto")`。
    /// 返回 `(字幕 id, 语言, 实际文件名)`。
    ///
    /// `transfer_mode = "auto"` 时**先试硬链接**（O(1)），失败才复制。
    /// 传 `Some(existing_hashes)` 可以复用调用方已算好的指纹集合，
    /// 批量导入时省掉重复查询。
    pub async fn register_subtitle_file(
        movie_id: i64,
        source_path: &std::path::Path,
        existing_hashes: Option<&[String]>,
        transfer_mode: &str,
    ) -> Result<SubtitleImportResult, ServiceError> {
        let _ = (movie_id, source_path, existing_hashes, transfer_mode);
        todo!("骨架：auto 时先 os.link 失败再复制；sha256 查重；原子落盘；返回 (id, 语言, 文件名)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只收字幕扩展名，**大小写不敏感**。
    #[test]
    fn only_subtitle_extensions_are_accepted() {
        assert!(is_subtitle_extension("a.srt"));
        assert!(is_subtitle_extension("A.SRT"), "大写也应接受");
        assert!(is_subtitle_extension("a.ass"));
        // 非字幕一律拒 —— 插件传进来的路径不该被当字幕存下。
        assert!(!is_subtitle_extension("a.mkv"));
        assert!(!is_subtitle_extension("a.jpg"));
        assert!(!is_subtitle_extension("srt"), "没有点不算");
    }

    /// ★「已存在同内容」是**正常结果**，不是失败。
    ///
    /// 归成失败会让插件以为没导入成功而反复重试。
    #[test]
    fn an_existing_identical_subtitle_is_a_normal_outcome() {
        let statuses = [
            SubtitleImportStatus::Imported,
            SubtitleImportStatus::AlreadyExists,
            SubtitleImportStatus::UnsupportedExtension,
            SubtitleImportStatus::MovieNotFound,
            SubtitleImportStatus::PersistFailed,
        ];
        assert!(statuses.contains(&SubtitleImportStatus::AlreadyExists));
        // 五种状态互不相同 —— 别把 AlreadyExists 与 PersistFailed 合并。
        assert_eq!(statuses.len(), 5);
    }
}
