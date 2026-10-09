//! 影片字幕的读取与列出（上游 `catalog/movie_subtitle_service.py`，183 行）。
//!
//! # 这个文件解掉一个曾被列为「卡死」的端点
//!
//! `docs/handoff.md` 里 `GET /movies/{n}/subtitles` 长期挂在「卡死的（不用试）」，
//! 理由写的是「要读媒体文件系统（provider 族）」。
//!
//! ⚠️ **那个判断是错的**：读字幕**不需要** provider。字幕文件由
//! [`super::subtitle_asset`] 落在**图片根目录同级**的字幕目录里，是宿主自己
//! 的文件系统。provider 参与的是「把字幕从媒体库**搬过来**」那一步
//! （写侧），读侧只读宿主目录。
//!
//! # ★ 10 MiB 上限是安全边界，必须在**读之前**判
//!
//! [`MAX_SUBTITLE_CONTENT_BYTES`]。用 `stat` 拿大小即可，**不要**先读进内存
//! 再判长度 —— 一个 2 GB 的「字幕」会把内存打满，而它可能只是个误传的视频。
//!
//! 上游用 `os.stat`（不是 `fstat`）—— 因为要先确认路径没逃逸再打开。
//!
//! # 路径逃逸防护：读之前必须做
//!
//! [`ensure_subtitle_path`]。字幕 id 来自 URL，转成路径时若不做
//! 前缀校验，`../` 就能读出字幕目录之外的文件。
//!
//! **不要**因为「id 是整数」就跳过 —— 落盘的路径是按 id 拼的，但记录里的
//! `file_path` 字段来自插件写入，那一侧的校验不能替代这里的校验。

use crate::error::ServiceError;

/// 字幕内容大小上限（10 MiB）。见模块文档。
pub const MAX_SUBTITLE_CONTENT_BYTES: u64 = 10 * 1024 * 1024;

/// 读侧错误。**抛错**（与写侧相反，见 [`super::subtitle_asset`] 的模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleReadError {
    MovieNotFound,
    SubtitleNotFound,
    /// ★ 路径逃逸（`../`、绝对路径、软链指向目录外）。
    PathInvalid,
    /// 文件在库里有记录但磁盘上没有。
    Unavailable,
    /// 超过 [`MAX_SUBTITLE_CONTENT_BYTES`]。
    TooLarge,
}

impl std::fmt::Display for SubtitleReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::MovieNotFound => "影片不存在",
            Self::SubtitleNotFound => "字幕不存在",
            Self::PathInvalid => "字幕路径不合法",
            Self::Unavailable => "字幕文件不可用",
            Self::TooLarge => "字幕文件过大",
        };
        f.write_str(text)
    }
}

impl std::error::Error for SubtitleReadError {}

/// 一条字幕记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubtitleAsset {
    pub id: i32,
    pub movie_id: i64,
    /// 语言。`None` = 从文件名推断不出。
    pub language: Option<String>,
    /// 原始文件名（**仅展示**，不可用于拼路径）。
    pub file_name: String,
    /// 内容指纹（sha256 十六进制）。用于去重。
    pub content_hash: String,
    /// 文件字节数。上限见 [`MAX_SUBTITLE_CONTENT_BYTES`]。
    pub size_bytes: i64,
}

/// 字幕内容。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleContent {
    pub subtitle_id: i32,
    /// 原始文件名。客户端据此决定编码与展示名。
    pub file_name: String,
    pub language: Option<String>,
    /// 文件字节。**按原始字节返回**，编码判断交给客户端。
    pub content: Vec<u8>,
}

/// 字幕目录（图片根目录的同级）。**从配置读**。
pub fn subtitle_dir() -> std::path::PathBuf {
    todo!("骨架：从 config 读字幕目录")
}

/// ★ 路径逃逸校验。**纯函数**，读任何字幕文件前必过。
///
/// 拒绝三样东西：
///
/// | 输入 | 原因 |
/// |---|---|
/// | 绝对路径 | 跳出字幕目录 |
/// | 含 `..` 的路径 | 同上 |
/// | 解析后不在字幕目录之下 | 软链指向目录外 |
///
/// 用 `canonicalize` 之后再判「前缀相同」—— 只做字符串前缀检查会被软链绕过。
pub fn ensure_subtitle_path(candidate: &std::path::Path, root: &std::path::Path) -> bool {
    let Ok(resolved) = candidate.canonicalize() else {
        return false; // 文件不存在 -> 交给上层报 Unavailable
    };
    let Ok(root_resolved) = root.canonicalize() else {
        return false;
    };
    resolved.starts_with(&root_resolved)
}

/// 字幕服务。
pub struct MovieSubtitleService;

impl MovieSubtitleService {
    /// 列出某部影片的全部字幕。
    ///
    /// 上游 `list_subtitle_assets(cls, movie_id) -> tuple[SubtitleAsset, ...]`。
    /// **影片不存在与「影片没有字幕」不同**：前者 404，后者空列表。
    pub async fn list_subtitle_assets(movie_id: i64) -> Result<Vec<SubtitleAsset>, ServiceError> {
        let _ = movie_id;
        todo!("骨架：先确认影片存在(404) -> 再查字幕记录(可空)")
    }

    /// ★ 读一条字幕的内容。
    ///
    /// 上游 `read_subtitle_content(cls, movie_id, subtitle_id) -> SubtitleContent`。
    ///
    /// 顺序（照上游，**别重排**）：
    ///
    /// 1. 查记录（不存在 → `SubtitleNotFound`）
    /// 2. 拼路径 → [`ensure_subtitle_path`]（不通过 → `PathInvalid`）
    /// 3. `stat` 判大小（超限 → `TooLarge`；**先判后读**）
    /// 4. 读内容
    /// 5. 读不到（记录在、文件没了）→ `Unavailable`
    pub async fn read_subtitle_content(
        movie_id: i64,
        subtitle_id: i32,
    ) -> Result<SubtitleContent, SubtitleReadError> {
        let _ = (movie_id, subtitle_id);
        todo!(
            "骨架：查记录 -> 路径逃逸校验 -> stat 判上限 -> 读内容；Unavailable 与 NotFound 要分开"
        )
    }

    /// 按番号取该影片的字幕列表（端点入口）。
    ///
    /// 上游 `get_movie_subtitles(cls, movie_number)`。**按番号**定位 ——
    /// 端点是 `/movies/{n}/subtitles`，`n` 就是番号。
    pub async fn get_movie_subtitles(
        movie_number: &str,
    ) -> Result<Vec<SubtitleAsset>, ServiceError> {
        let _ = movie_number;
        todo!("骨架：按番号定位影片 -> list_subtitle_assets；影片不存在 -> 404 movie_not_found")
    }

    /// 扫描字幕目录，同步该影片的字幕记录。
    ///
    /// 上游 `sync_movie_subtitles(cls, movie) -> dict[str, int]`。扫 `.srt`
    /// 等文件并登记 —— 用于「字幕已由其它途径放进目录」的情况。
    pub async fn sync_movie_subtitles(movie_id: i64) -> Result<serde_json::Value, ServiceError> {
        let _ = movie_id;
        todo!("骨架：扫字幕目录下的 .srt -> 算指纹查重 -> 登记缺失的")
    }
}

impl SubtitleReadError {
    /// 对应的 HTTP 状态码。
    ///
    /// `PathInvalid` 是 **403** 而不是 404 —— 路径不合法是「请求不被允许」，
    /// 而「字幕不存在」才是 404。
    pub fn status(&self) -> u16 {
        match self {
            Self::MovieNotFound | Self::SubtitleNotFound => 404,
            Self::PathInvalid => 403,
            Self::Unavailable => 409,
            Self::TooLarge => 413,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 路径逃逸必须被拒。**三种手法都要挡住。**
    #[test]
    fn path_traversal_attempts_are_rejected() {
        let root = std::env::temp_dir();
        // 根之外的路径 -> 拒
        assert!(!ensure_subtitle_path(
            std::path::Path::new("/etc/passwd"),
            &root
        ));
        // 含 .. 的路径 -> 拒
        assert!(!ensure_subtitle_path(
            std::path::Path::new("../../../etc/passwd"),
            &root
        ));
        // 不存在的文件 -> 拒（交给上层报 Unavailable，而不是当成合法）
        assert!(!ensure_subtitle_path(
            &root.join("definitely-not-here.srt"),
            &root
        ));
    }

    /// ★ `PathInvalid` 是 **403**，`SubtitleNotFound` 是 **404**。
    ///
    /// 混成 404 会把「有人在探测路径逃逸」伪装成「字幕不存在」。
    #[test]
    fn invalid_path_is_403_but_missing_subtitle_is_404() {
        assert_eq!(SubtitleReadError::PathInvalid.status(), 403);
        assert_eq!(SubtitleReadError::SubtitleNotFound.status(), 404);
        assert_eq!(SubtitleReadError::MovieNotFound.status(), 404);
    }

    /// 大小上限是 10 MiB。
    #[test]
    fn the_size_cap_is_ten_mib() {
        assert_eq!(MAX_SUBTITLE_CONTENT_BYTES, 10 * 1024 * 1024);
    }

    /// 「记录在、文件没了」是 **409** 而不是 404 —— 记录确实存在。
    ///
    /// 报 404 会让客户端以为字幕被删了，从而不再重试同步。
    #[test]
    fn a_missing_file_is_conflict_not_not_found() {
        assert_eq!(SubtitleReadError::Unavailable.status(), 409);
        assert_ne!(
            SubtitleReadError::Unavailable.status(),
            SubtitleReadError::SubtitleNotFound.status()
        );
    }
}
