//! 影片图片子系统（上游 `catalog/movie_image_service.py`，794 行）。
//!
//! 它是从 `catalog_import` 里**抽出来**的：图片下载、封面切割、薄封面解析。
//! 拆出来的理由是它有自己的重试/临时文件生命周期，与「元数据落库」是两件事。
//!
//! # 下载：6 次重试 + 30 秒超时
//!
//! [`IMAGE_DOWNLOAD_MAX_RETRIES`] / [`IMAGE_DOWNLOAD_TIMEOUT_SECONDS`]。
//! 重试是因为图片站经常抽风；6 次是权衡 —— 再多会让一次导入卡几分钟。
//!
//! # ★ 关键设计：先下到**临时文件**，全部成功才落盘
//!
//! [`MovieImageService::download_image_tasks_to_temporary_files`]
//! -> [`MovieImageService::finalize_prepared_image_files`]。
//!
//! 为什么不直接写最终路径：中途失败会留下**半张图**（0 字节或截断），而
//! `image` 记录一旦建立就会指向它 —— 播放器显示裂图，且**没有重试机会**
//! （记录已存在，下次导入认为「已有」）。
//!
//! 用 `prepare_metadata_images` 这个 context manager 把「临时 -> 落盘 -> 清理」
//! 圈起来，保证异常路径下临时文件也被清掉。
//!
//! # 薄封面：从剧情图里**切**出竖图
//!
//! `resolve_thin_cover_*` 三个方法的区别只在**图从哪来**：
//!
//! | 方法 | 图的来源 |
//! |---|---|
//! | `..._from_downloaded_images` | 刚下载好的临时文件 |
//! | `..._from_prepared_images` | 准备好的临时文件（不重新下） |
//! | `..._from_existing_movie` | **库里已有的**剧情图（不联网） |
//!
//! # ⚠️ `cv2` 缺失时**降级跳过**，不报错
//!
//! 少一张竖封面不影响影片可用性。这与「图片下载失败」不同 —— 那个必须报错。
//!
//! # 失败抛 `ImageDownloadError` 而非 `ApiError`
//!
//! 调用方（`catalog_import`）要决定最终 HTTP 状态码。

use crate::error::ServiceError;

/// 图片下载最大重试次数。
pub const IMAGE_DOWNLOAD_MAX_RETRIES: u32 = 6;
/// 单次下载超时（秒）。
pub const IMAGE_DOWNLOAD_TIMEOUT_SECONDS: u64 = 30;

/// 图片处理失败。**不是** `ApiError`（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageDownloadError {
    /// 全部重试都失败。
    DownloadFailed { url: String, attempts: u32 },
    /// 落盘失败。
    PersistFailed(String),
    /// 图片解码失败（不是有效图片）。
    DecodeFailed(String),
    /// 薄封面切割失败。⚠️ **可降级** —— `cv2` 缺失就属于这类。
    ThinCoverFailed(String),
}

/// 一个待下载的图片任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePersistTask {
    /// 归属类型（`movie_cover` / `plot_image` / `actor_profile`）。
    pub owner_type: String,
    /// 归属键（番号或演员 JavDB id）。
    pub owner_key: String,
    /// 图片 URL。**出网**。
    pub image_url: String,
    /// 剧情图序号。`None` = 不是剧情图（封面/头像）。
    pub plot_index: Option<i32>,
}

/// 已下载到临时文件的图片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImageFile {
    pub task: ImagePersistTask,
    /// 临时文件路径。**落盘前不要假设它稳定**。
    pub temp_path: std::path::PathBuf,
    pub size_bytes: u64,
}

/// 薄封面解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinCoverResolution {
    /// 选中的剧情图序号。
    pub plot_index: i32,
    /// 源图片路径。
    pub source_path: std::path::PathBuf,
    /// 切出的竖图临时路径。`None` = `cv2` 缺失，未切。
    pub thin_cover_path: Option<std::path::PathBuf>,
    /// 本次是否刷新了（刷新过的要重建 assets.zip）。
    pub refreshed: bool,
}

/// 构造图片任务清单。供 [`super::catalog_import`] 依赖（那个 trait 名是
/// `ImageTasksBuilder`）。
pub trait ImageTasksBuilder {
    /// 为一次导入构造全部图片任务。
    ///
    /// 返回 `(封面任务, 剧情图任务列表, 演员头像任务按 JavDB id 分组)`。
    fn build_movie_import_image_tasks(
        &self,
        movie_number: &str,
        cover_image_url: Option<&str>,
        plot_urls: &[String],
        actors: &[serde_json::Value],
    ) -> Result<ImageTaskSet, ServiceError>;
}

/// 一次导入的全部图片任务。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageTaskSet {
    /// 封面任务。`None` = 没有封面。
    pub cover: Option<ImagePersistTask>,
    /// 剧情图任务，按序号升序。
    pub plots: Vec<ImagePersistTask>,
    /// 演员头像任务，按演员 JavDB id 分组。
    pub actor_avatars: Vec<(String, ImagePersistTask)>,
}

impl ImageTaskSet {
    /// 汇总成**扁平列表**。`collect_image_tasks` 用它。
    pub fn flatten(&self) -> Vec<ImagePersistTask> {
        let mut all = Vec::new();
        if let Some(cover) = &self.cover {
            all.push(cover.clone());
        }
        all.extend(self.plots.iter().cloned());
        all.extend(self.actor_avatars.iter().map(|(_, task)| task.clone()));
        all
    }
}

/// 图片服务。
pub struct MovieImageService;

impl MovieImageService {
    /// 下载全部任务到**临时文件**。`Ok` 不代表落盘成功。
    pub async fn download_image_tasks_to_temporary_files(
        &self,
        tasks: &[ImagePersistTask],
    ) -> Result<Vec<PreparedImageFile>, ServiceError> {
        let _ = tasks;
        todo!("骨架：逐个下载(6 次重试/30s 超时)到临时文件；任一彻底失败则整体 Err")
    }

    /// 把准备好的文件**落盘 + 登记记录**。返回建立的 `image` id。
    pub async fn finalize_prepared_image_files(
        &self,
        prepared: &[PreparedImageFile],
    ) -> Result<Vec<i32>, ServiceError> {
        let _ = prepared;
        todo!("骨架：os.replace 原子落盘 -> 登记 image 记录 -> 必要时清 Qdrant 旧向量")
    }

    /// 清理临时文件。**异常路径也必须调**（否则临时文件堆满磁盘）。
    pub fn cleanup_prepared_image_files(prepared: &[PreparedImageFile]) {
        for file in prepared {
            let _ = std::fs::remove_file(&file.temp_path);
        }
    }

    /// 从已下载的图里解析薄封面。**不联网**。
    pub async fn resolve_thin_cover_from_prepared_images(
        &self,
        movie_number: &str,
        prepared: &[PreparedImageFile],
    ) -> Result<ThinCoverResolution, ServiceError> {
        let _ = (movie_number, prepared);
        todo!("骨架：从剧情图里选竖图候选 -> cv2 切割(缺失则 None) -> ThinCoverResolution")
    }

    /// 从**库里已有**的剧情图解析薄封面。**完全离线**。
    ///
    /// 由 [`super::movie_thin_cover_backfill`] 调用 —— 那个任务不出网。
    pub async fn resolve_thin_cover_from_existing_movie(
        &self,
        movie_id: i64,
    ) -> Result<ThinCoverResolution, ServiceError> {
        let _ = movie_id;
        todo!("骨架：读库里剧情图 -> 切割；无剧情图返回 Err(可降级)")
    }

    /// 登记一张图片。`plot_index = None` 表示不是剧情图。
    pub async fn persist_image(
        &self,
        owner_type: &str,
        owner_key: &str,
        image_url: Option<&str>,
        plot_index: Option<i32>,
    ) -> Result<i32, ServiceError> {
        let _ = (owner_type, owner_key, image_url, plot_index);
        todo!("骨架：下载 -> 落盘 -> 登记；image_url 为 None 时返回 Err")
    }
}
