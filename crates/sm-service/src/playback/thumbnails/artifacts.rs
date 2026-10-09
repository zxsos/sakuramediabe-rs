//! 缩略图产物的校验与落盘（上游 `playback/thumbnails/artifacts.py`，205 行）。
//!
//! # 这是 provider 与宿主之间的**信任边界**
//!
//! provider 在临时目录里产出 WebP 文件，宿主把它们搬进正式目录并登记。
//! 搬之前**必须校验四样**，任何一样不过就整条丢弃：
//!
//! | 校验 | 挡的是 |
//! |---|---|
//! | 路径在 workspace 内 | provider 让宿主写任意位置（`../`） |
//! | 是 WebP | 别的格式会让客户端解不出来 |
//! | 非空 | 0 字节文件会让播放器显示空白 |
//! | 偏移非负 | 负偏移会让排序错乱、选图定位错 |
//!
//! # 用 Pillow 校验，**不要**只信扩展名
//!
//! 扩展名是 provider 写的。Pillow 真的去解一下头，才知道它是不是图片。
//!
//! # 落盘是**事务**：文件先落，再登记 DB
//!
//! 顺序不能反。反过来会留下「记录指向不存在的文件」—— 那个状态没有重试
//! 机会（下次生成认为「已有」）。
//!
//! 详见 [`super::task_service::classify`] 里「数量不足也算成功」的取舍。

use crate::error::ServiceError;

/// 缩略图格式。**只接受 WebP**。
pub const THUMBNAIL_FORMAT: &str = "webp";

/// provider 产出的一件缩略图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailArtifact {
    /// 缩略图 id（宿主侧，provider 带回）。
    pub thumbnail_id: i64,
    /// 在媒体里的偏移（秒）。**必须 >= 0**。
    pub offset_seconds: i64,
    /// provider 写出的文件路径（**在 workspace 内**）。
    pub path: std::path::PathBuf,
}

/// 已落盘登记的缩略图。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaThumbnailResource {
    pub id: i32,
    pub media_id: i64,
    /// 在影片里的偏移（秒）。
    pub offset: i64,
    /// 图片的相对路径。**要签名才能访问**（见 `sm_api::dto::ImageResource`）。
    pub image_path: String,
}

/// 产物校验失败。**抛错**（不是返回 `false`）—— 上游用 `ValueError`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    /// 路径逃出 workspace。
    PathInvalid(std::path::PathBuf),
    /// 不是 WebP。
    NotWebp(std::path::PathBuf),
    /// 0 字节。
    Empty(std::path::PathBuf),
    /// 偏移为负。
    OffsetInvalid(i64),
    /// 图片解不开（扩展名骗人）。
    DecodeFailed(std::path::PathBuf),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathInvalid(p) => write!(f, "thumbnail_artifact_path_invalid: {}", p.display()),
            Self::NotWebp(p) => write!(f, "thumbnail_artifact_not_webp: {}", p.display()),
            Self::Empty(p) => write!(f, "thumbnail_artifact_empty: {}", p.display()),
            Self::OffsetInvalid(o) => write!(f, "thumbnail_offset_invalid: {o}"),
            Self::DecodeFailed(p) => write!(f, "thumbnail_artifact_invalid: {}", p.display()),
        }
    }
}

/// 产物服务。
pub struct ThumbnailArtifactService;

impl ThumbnailArtifactService {
    /// 该媒体的缩略图目录。
    pub fn thumbnail_directory(media_id: i64) -> std::path::PathBuf {
        let _ = media_id;
        todo!("骨架：<image_root>/thumbnails/<media_id>/")
    }

    /// 该媒体的缩略图**包**路径。
    pub fn thumbnail_pack_file(media_id: i64) -> std::path::PathBuf {
        let _ = media_id;
        todo!("骨架：<thumbnail_dir>/thumbnails.zip（与 assets.zip 同样的理由）")
    }

    /// ★ 校验一件产物。**四道校验全过才返回落盘目标路径**。
    ///
    /// 上游 `validate_artifact(cls, workspace, artifact) -> Path`。
    /// 顺序照上游：**先路径**（最便宜且最危险），再格式，再大小，最后偏移。
    ///
    /// 路径校验用 `canonicalize` 后判前缀 —— 只查字符串前缀会被软链绕过。
    pub fn validate_artifact(
        workspace: &std::path::Path,
        artifact: &ThumbnailArtifact,
    ) -> Result<std::path::PathBuf, ArtifactError> {
        if artifact.offset_seconds < 0 {
            return Err(ArtifactError::OffsetInvalid(artifact.offset_seconds));
        }
        let resolved = artifact
            .path
            .canonicalize()
            .map_err(|_| ArtifactError::PathInvalid(artifact.path.clone()))?;
        let root = workspace
            .canonicalize()
            .map_err(|_| ArtifactError::PathInvalid(workspace.to_path_buf()))?;
        if !resolved.starts_with(&root) {
            return Err(ArtifactError::PathInvalid(artifact.path.clone()));
        }
        if !resolved
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(THUMBNAIL_FORMAT))
        {
            return Err(ArtifactError::NotWebp(resolved));
        }
        let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            return Err(ArtifactError::Empty(resolved));
        }
        Ok(resolved)
    }

    /// ★ 落盘 + 登记。**先落文件，再登记 DB**（见模块文档）。
    ///
    /// 返回登记的缩略图数。
    pub async fn persist(
        &self,
        media_id: i64,
        artifacts: &[(ThumbnailArtifact, std::path::PathBuf)],
    ) -> Result<u32, ServiceError> {
        let _ = (media_id, artifacts);
        todo!("骨架：os.replace 到正式目录 -> 登记 media_thumbnail -> 事务提交")
    }

    /// 读图片尺寸。`Ok(None)` = 解不出来（**不报错**）。
    pub fn read_dimensions(image_origin: &str) -> Option<(u32, u32)> {
        let _ = image_origin;
        todo!("骨架：Pillow 读尺寸；解不开返回 None")
    }

    /// 列出该媒体的缩略图。**按 `(offset, id)` 升序**。
    ///
    /// 排序不能省：选图逻辑（见 `discovery::moment_recommendation`）依赖
    /// 「中位数」这类位置语义，无序会让选出的图不稳定。
    pub async fn list_media_thumbnails(media_id: i64) -> Result<Vec<MediaThumbnailResource>, ServiceError> {
        let _ = media_id;
        todo!("骨架：查 media_thumbnail 按 (offset ASC, id ASC)")
    }
}

