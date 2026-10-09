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
//! 圈起来，保证异常路径下临时文件也被清掉（本仓里由调用方保证
//! [`MovieImageService::cleanup_prepared_image_files`] 一定被调用）。
//!
//! # 薄封面：从剧情图里**切**出竖图
//!
//! `resolve_thin_cover_*` 两个方法的区别只在**图从哪来**：
//!
//! | 方法 | 图的来源 |
//! |---|---|
//! | `..._from_prepared_images` | 刚下好的临时文件（不重新联网） |
//! | `..._from_existing_movie` | **库里已有的**封面（完全离线） |
//!
//! ★ 优先级是「**先切封面**、切不出来才回退「挑一张竖的剧情图」」
//! （上游 `resolve_thin_cover_*`：cover 分支在前）。
//!
//! # ⚠️ `cv2` 那一步现在是纯 Rust 的，不再降级
//!
//! 上游缺 `cv2` 时整条薄封面链路不可用；本仓用 `svc-image`（Sobel 书脊检测）
//! 替代，所以「切割」不再是缺口 —— 但**切不出来**仍然 `Ok(None)`（可降级）。
//!
//! # ⚠️ 未闭环：`assets.zip` 读写与剧情图回退
//!
//! 1. 包（zip）读写：判据在 [`svc_image::paths::image_pack_relative_path`] 就位，
//!    读取/写入还没实现（缺 zip 依赖）。单文件那一路在任何情况下都正确。
//! 2. `resolve_thin_cover_from_existing_movie` 的**剧情图回退**分支：需要
//!    `movie_plot_image` 的按影片查询（仓储层还没有），现在只走封面那一支。
//!
//! # 失败抛 `ImageDownloadError` 而非 `ApiError`
//!
//! 调用方（`catalog_import`）要决定最终 HTTP 状态码。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sm_db::repo::{ImageRepository, MovieRepository, NewImage};
use sm_db::Db;

use crate::error::ServiceError;

/// 图片下载最大重试次数。
pub const IMAGE_DOWNLOAD_MAX_RETRIES: u32 = 6;
/// 单次下载超时（秒）。
pub const IMAGE_DOWNLOAD_TIMEOUT_SECONDS: u64 = 30;
/// 书脊检测的搜索半径（列）。上游 `_detect_split_points(center_range=100)`。
pub const THIN_COVER_CENTER_RANGE: usize = 100;

/// 图片处理失败。**不是** `ApiError`（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageDownloadError {
    /// 全部重试都失败。
    DownloadFailed { url: String, attempts: u32 },
    /// 落盘失败。
    PersistFailed(String),
    /// 图片解码失败（不是有效图片）。
    DecodeFailed(String),
    /// 薄封面切割失败。⚠️ **可降级** —— 找不到书脊就属于这类。
    ThinCoverFailed(String),
}

/// 一个待下载的图片任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePersistTask {
    /// 归属类型（`movie_cover` / `movie_plot` / `actor`）。
    pub owner_type: String,
    /// 归属键（番号或演员 JavDB id）。
    pub owner_key: String,
    /// 图片 URL。**出网**。
    pub image_url: String,
    /// ★ 库内相对路径（`image.origin`）。由
    /// [`svc_image::paths`] 那套规则算出 —— 落盘、读取、清理都用同一个值。
    pub relative_path: String,
    /// 剧情图序号。`None` = 不是剧情图（封面/头像）。
    pub plot_index: Option<i32>,
}

/// 已下载到临时文件的图片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImageFile {
    pub task: ImagePersistTask,
    /// 临时文件路径。**落盘前不要假设它稳定**。
    pub temp_path: PathBuf,
    pub size_bytes: u64,
}

/// 薄封面解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinCoverResolution {
    /// 选中的剧情图序号。**切封面成功时是 `-1`**（上游同款哨兵：那一刻
    /// `selected_plot_index` 没有意义）。
    pub plot_index: i32,
    /// 源图片路径。
    pub source_path: PathBuf,
    /// 切出的竖图临时路径（还没落盘）。`None` = 切不出来，**可降级**。
    pub thin_cover_path: Option<PathBuf>,
    /// 切出来的图该落在哪个相对路径。`None` = 没切出图。
    pub relative_path: Option<String>,
    /// 本次是否刷新了（刷新过的要重建 `assets.zip`）。
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

/// 图片下载器（出网）。**可注入**，测试用替身。
///
/// 抽成别名：`Box<dyn Fn(&str, &Path) -> Result<(), ServiceError>>` 这个形状
/// 在字段与构造参数上各写一遍，`clippy::type_complexity` 也会在这里报警。
pub type ImageDownloader = Box<dyn Fn(&str, &Path) -> Result<(), ServiceError> + Send + Sync>;

/// 下载器的**共享**形态。每次重试都要把它送进 `spawn_blocking`，所以得是
/// `Arc` —— 抽成别名是因为 `Arc<dyn Fn(...) + Send + Sync>` 写在字段上会触发
/// `clippy::type_complexity`。
type SharedDownloader = Arc<dyn Fn(&str, &Path) -> Result<(), ServiceError> + Send + Sync>;

/// 图片服务。
pub struct MovieImageService {
    db: Db,
    /// 图片根目录（上游 `settings.media.import_image_root_path`）。
    root: PathBuf,
    /// 下载器。**包在 `Arc` 里**：每次重试都要把它送进
    /// `spawn_blocking`（它是阻塞式出网调用，不能直接占着异步线程）。
    downloader: SharedDownloader,
}

impl ImageTasksBuilder for MovieImageService {
    /// 为一次导入构造全部图片任务。上游 `build_movie_import_image_tasks`：
    ///
    /// - 封面一个任务；剧情图按序号逐个；演员头像按 **JavDB id 去重**（同一
    ///   演员在一部片里出现两次很正常）；
    /// - URL 为空的条目直接**跳过**（不是错误）；
    /// - 相对路径规则**只此一份**：委托给 [`relative_path_for`]（它走
    ///   `svc_image::paths`，与本仓全部落盘/读取/清理共用同一套规则）。
    fn build_movie_import_image_tasks(
        &self,
        movie_number: &str,
        cover_image_url: Option<&str>,
        plot_urls: &[String],
        actors: &[serde_json::Value],
    ) -> Result<ImageTaskSet, ServiceError> {
        let cover = cover_image_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| {
                relative_path_for("movie_cover", movie_number, url, None).map(|relative_path| {
                    ImagePersistTask {
                        owner_type: "movie_cover".to_owned(),
                        owner_key: movie_number.to_owned(),
                        image_url: url.to_owned(),
                        relative_path,
                        plot_index: None,
                    }
                })
            })
            .transpose()?;

        let mut plots = Vec::with_capacity(plot_urls.len());
        for (index, url) in plot_urls.iter().enumerate() {
            let url = url.trim();
            if url.is_empty() {
                continue;
            }
            let plot_index = i32::try_from(index)
                .map_err(|_| ServiceError::from_status(500, "internal_error", "剧情图序号溢出"))?;
            let relative_path =
                relative_path_for("movie_plot", movie_number, url, Some(plot_index))?;
            plots.push(ImagePersistTask {
                owner_type: "movie_plot".to_owned(),
                owner_key: movie_number.to_owned(),
                image_url: url.to_owned(),
                relative_path,
                plot_index: Some(plot_index),
            });
        }

        let mut actor_avatars: Vec<(String, ImagePersistTask)> = Vec::new();
        for actor in actors {
            let javdb_id = actor
                .get("javdb_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if javdb_id.is_empty() || actor_avatars.iter().any(|(id, _)| id == javdb_id) {
                continue;
            }
            let Some(avatar_url) = actor.get("avatar_url").and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let relative_path = relative_path_for("actor", javdb_id, avatar_url, None)?;
            actor_avatars.push((
                javdb_id.to_owned(),
                ImagePersistTask {
                    owner_type: "actor".to_owned(),
                    owner_key: javdb_id.to_owned(),
                    image_url: avatar_url.to_owned(),
                    relative_path,
                    plot_index: None,
                },
            ));
        }

        Ok(ImageTaskSet {
            cover,
            plots,
            actor_avatars,
        })
    }
}

impl MovieImageService {
    /// 构造。
    pub fn new(db: &Db, root: PathBuf, downloader: ImageDownloader) -> Self {
        Self {
            db: db.clone(),
            root,
            downloader: Arc::from(downloader),
        }
    }

    /// 下载全部任务到**临时文件**。`Ok` 不代表落盘成功。
    ///
    /// # ★ 任一任务彻底失败 → **整体 `Err`**
    ///
    /// 上游 `download_image_tasks_to_temporary_files` 也是这个语义：不做
    /// 「下到一半就落盘」的部分成功。理由是 `image` 记录一旦建立就**没有重试
    /// 机会**（下次导入认为「已有」），少一张图比一张裂图代价小。
    ///
    /// 已经下好的临时文件在返回前被清掉（调用方不必再清理失败路径）。
    pub async fn download_image_tasks_to_temporary_files(
        &self,
        tasks: &[ImagePersistTask],
    ) -> Result<Vec<PreparedImageFile>, ServiceError> {
        let mut prepared = Vec::with_capacity(tasks.len());
        for task in tasks {
            match self.download_one(task).await {
                Ok(file) => prepared.push(file),
                Err(error) => {
                    // 已经下好的全清掉 —— 调用方拿到 Err 时不必再自己收拾。
                    Self::cleanup_prepared_image_files(&prepared);
                    return Err(error);
                }
            }
        }
        Ok(prepared)
    }

    /// 把一个准备好的文件**落盘 + 登记记录**。返回建立的 `image` id。
    ///
    /// 落盘是**原子的**（`svc_image::store::atomic_write`：同目录临时文件 →
    /// fsync → rename），所以不会出现「记录指向半张图」。
    ///
    /// 登记走 [`ImageRepository::upsert`]（按 `origin` 幂等），所以重复导入
    /// 同一张图不会多出一条记录。
    pub async fn finalize_prepared_image_files(
        &self,
        prepared: &[PreparedImageFile],
    ) -> Result<Vec<i32>, ServiceError> {
        let repo = ImageRepository::new(self.db.clone());
        let mut ids = Vec::with_capacity(prepared.len());
        for file in prepared {
            let bytes = std::fs::read(&file.temp_path).map_err(|error| {
                ServiceError::unavailable(
                    "image_temp_read_failed",
                    format!("读临时图片失败：{}", error),
                )
            })?;
            svc_image::store::atomic_write(&self.root, &file.task.relative_path, &bytes).map_err(
                |error| {
                    ServiceError::unavailable(
                        "image_persist_failed",
                        format!("图片落盘失败：{error}"),
                    )
                },
            )?;
            let (image_id, _created) = repo
                .upsert(&NewImage {
                    origin: file.task.relative_path.clone(),
                })
                .await?;
            ids.push(image_id);
        }
        Self::cleanup_prepared_image_files(prepared);
        Ok(ids)
    }

    /// 清理临时文件。**异常路径也必须调**（否则临时文件堆满磁盘）。
    pub fn cleanup_prepared_image_files(prepared: &[PreparedImageFile]) {
        for file in prepared {
            let _ = std::fs::remove_file(&file.temp_path);
        }
    }

    /// 从已下载的图里解析薄封面。**不联网**。
    ///
    /// # ★ 先切封面，切不出来才挑竖的剧情图
    ///
    /// 上游 `resolve_thin_cover_from_prepared_images`（`:273-303`）：先拿封面
    /// 走书脊检测，成功就直接返回；否则在**前两张**剧情图里挑第一张竖图
    /// （`:243-248` 的硬约定：只有前两张参与判定）。
    pub async fn resolve_thin_cover_from_prepared_images(
        &self,
        movie_number: &str,
        prepared: &[PreparedImageFile],
    ) -> Result<ThinCoverResolution, ServiceError> {
        let cover = prepared
            .iter()
            .find(|file| file.task.owner_type == "movie_cover");
        if let Some(cover) = cover {
            if let Some(resolution) = self.crop_to_temp(movie_number, &cover.temp_path)? {
                return Ok(resolution);
            }
        }
        // 回退：前两张剧情图里的第一张竖图。
        for file in prepared
            .iter()
            .filter(|file| file.task.owner_type == "movie_plot")
            .take(2)
        {
            let bytes = std::fs::read(&file.temp_path).unwrap_or_default();
            if svc_image::store::is_portrait(&bytes) {
                return Ok(ThinCoverResolution {
                    plot_index: file.task.plot_index.unwrap_or(0),
                    source_path: file.temp_path.clone(),
                    thin_cover_path: None,
                    relative_path: None,
                    refreshed: false,
                });
            }
        }
        Err(ServiceError::not_found(
            "thin_cover_unavailable",
            "既切不出竖封面，也没有竖的剧情图",
            "movie_number",
            0,
        ))
    }

    /// 从**库里已有**的封面解析薄封面。**完全离线**。
    ///
    /// 由 [`super::movie_thin_cover_backfill`] 调用 —— 那个任务不出网。
    ///
    /// ⚠️ **剧情图回退分支尚未接线**：需要 `movie_plot_image` 的按影片查询
    /// （仓储层还没有）。现在没有封面就返回 `Err`（可降级，调用方记
    /// `skipped`）。
    pub async fn resolve_thin_cover_from_existing_movie(
        &self,
        movie_id: i32,
    ) -> Result<ThinCoverResolution, ServiceError> {
        let movie = MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found("movie_not_found", "影片不存在", "movie_id", movie_id)
            })?;
        let Some(cover_id) = movie.cover_image_id else {
            return Err(ServiceError::not_found(
                "cover_missing",
                "这部影片没有封面，切不出薄封面",
                "movie_id",
                movie_id,
            ));
        };
        let cover = ImageRepository::new(self.db.clone())
            .find_by_id(cover_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found("cover_missing", "封面记录不存在", "image_id", cover_id)
            })?;
        let absolute = svc_image::paths::absolute(&self.root, &cover.origin);
        self.crop_to_temp(&movie.movie_number, &absolute)?
            .ok_or_else(|| {
                ServiceError::validation(
                    "thin_cover_unavailable",
                    "封面里找不到书脊，切不出薄封面（可降级）",
                )
            })
    }

    /// 登记一张图片。`plot_index = None` 表示不是剧情图。
    ///
    /// 上游 `persist_image`（`:620-645`）：`image_url` 为空直接返回 `None`
    /// —— 「没有图」不是错误。
    pub async fn persist_image(
        &self,
        owner_type: &str,
        owner_key: &str,
        image_url: Option<&str>,
        plot_index: Option<i32>,
    ) -> Result<Option<i32>, ServiceError> {
        let Some(url) = image_url.map(str::trim).filter(|url| !url.is_empty()) else {
            return Ok(None);
        };
        let relative_path = relative_path_for(owner_type, owner_key, url, plot_index)?;
        let task = ImagePersistTask {
            owner_type: owner_type.to_owned(),
            owner_key: owner_key.to_owned(),
            image_url: url.to_owned(),
            relative_path,
            plot_index,
        };
        let prepared = self
            .download_image_tasks_to_temporary_files(&[task])
            .await?;
        let ids = self.finalize_prepared_image_files(&prepared).await?;
        Ok(ids.into_iter().next())
    }

    /// 下一个临时文件路径。**同进程内不撞名**（pid + 原子计数）。
    fn temp_path(&self, name_hint: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join("sakuramedia-image-tmp")
            .join(format!("{}-{}-{name_hint}", std::process::id(), seq))
    }

    async fn download_one(
        &self,
        task: &ImagePersistTask,
    ) -> Result<PreparedImageFile, ServiceError> {
        let temp_path = self.temp_path(task.relative_path.rsplit('/').next().unwrap_or("image"));
        if let Some(parent) = temp_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ServiceError::unavailable("image_temp_failed", format!("建临时目录失败：{error}"))
            })?;
        }
        let mut last_error = String::new();
        for attempt in 1..=IMAGE_DOWNLOAD_MAX_RETRIES {
            let downloader = Arc::clone(&self.downloader);
            let url = task.image_url.clone();
            let path = temp_path.clone();
            // 下载是**阻塞式出网**：放进 blocking 池，别占着异步执行线程。
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(IMAGE_DOWNLOAD_TIMEOUT_SECONDS),
                tokio::task::spawn_blocking(move || downloader(&url, &path)),
            )
            .await;
            match outcome {
                Ok(Ok(Ok(()))) => {
                    let size_bytes = std::fs::metadata(&temp_path)
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    return Ok(PreparedImageFile {
                        task: task.clone(),
                        temp_path,
                        size_bytes,
                    });
                }
                Ok(Ok(Err(error))) => last_error = error.code().to_owned(),
                Ok(Err(error)) => last_error = format!("下载线程异常：{error}"),
                Err(_) => last_error = format!("下载超时 {IMAGE_DOWNLOAD_TIMEOUT_SECONDS}s"),
            }
            tracing::warn!(url = task.image_url.as_str(), attempt, "图片下载失败，重试");
        }
        let _ = std::fs::remove_file(&temp_path);
        Err(ServiceError::bad_gateway(
            "image_download_failed",
            format!(
                "图片下载失败（{} 次重试后仍失败）：{} —— {}",
                IMAGE_DOWNLOAD_MAX_RETRIES, task.image_url, last_error
            ),
            crate::error::details_of("image_url", task.image_url.as_str()),
        ))
    }

    /// 切封面 → 写到临时文件。**切不出来返回 `Ok(None)`**（可降级）。
    fn crop_to_temp(
        &self,
        movie_number: &str,
        source_path: &Path,
    ) -> Result<Option<ThinCoverResolution>, ServiceError> {
        let bytes = std::fs::read(source_path).unwrap_or_default();
        let Some(cropped) = svc_image::store::crop_thin_cover(&bytes, THIN_COVER_CENTER_RANGE)
        else {
            return Ok(None);
        };
        let extension = extension_of(source_path);
        let temp_path = self.temp_path(&format!(
            "thin-cover-{}",
            svc_image::paths::normalize_asset_dir_name(movie_number)
        ));
        let final_name = format!("thin-cover{extension}");
        svc_image::store::save_rgb(&temp_path, &cropped).map_err(|error| {
            ServiceError::unavailable(
                "thin_cover_encode_failed",
                format!("薄封面编码失败：{error}"),
            )
        })?;
        Ok(Some(ThinCoverResolution {
            plot_index: -1,
            source_path: source_path.to_path_buf(),
            thin_cover_path: Some(temp_path),
            relative_path: Some(format!(
                "{}/{final_name}",
                svc_image::paths::movie_asset_relative_dir(
                    &svc_image::paths::normalize_asset_dir_name(movie_number)
                )
            )),
            refreshed: true,
        }))
    }
}

/// 按归属类型算出相对路径（上游 `_build_image_task`，`:395-440`）。
fn relative_path_for(
    owner_type: &str,
    owner_key: &str,
    image_url: &str,
    plot_index: Option<i32>,
) -> Result<String, ServiceError> {
    let url_path = url_path_of(image_url);
    let extension = extension_of(Path::new(&url_path));
    match owner_type {
        "actor" => Ok(svc_image::paths::actor_relative_path(owner_key, &extension)),
        "movie_cover" => Ok(svc_image::paths::movie_cover_relative_path(
            owner_key, &extension,
        )),
        "movie_plot" => {
            let index = plot_index.ok_or_else(|| {
                ServiceError::validation("plot_index_required", "剧情图必须带 plot_index")
            })?;
            Ok(svc_image::paths::movie_plot_relative_path(
                owner_key, index, &extension,
            ))
        }
        other => Err(ServiceError::validation(
            "unsupported_owner_type",
            format!("不支持的归属类型：{other}"),
        )),
    }
}

/// 取 URL 的 path 部分（扩展名要从它上面截，查询串不能算进来）。
fn url_path_of(image_url: &str) -> String {
    image_url
        .split("://")
        .nth(1)
        .unwrap_or(image_url)
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_else(|| ".jpg".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 扩展名从 **URL 的 path** 上截，不带查询串 —— 否则会截出一长串，
    /// 而 [`svc_image::paths::normalize_image_extension`] 会把它回退成 `.jpg`。
    #[test]
    fn the_extension_comes_from_the_url_path() {
        let url = "https://example.com/a/cover.JPG?token=abcdef";
        assert_eq!(extension_of(Path::new(&url_path_of(url))), ".JPG");
        assert_eq!(
            relative_path_for("movie_cover", "ABC-123", url, None).expect("可算"),
            format!(
                "{}/cover.jpg",
                svc_image::paths::movie_asset_relative_dir("ABC-123")
            ),
            "过长的扩展名回退 .jpg"
        );
    }

    /// 三类归属的相对路径（上游 `:414-431`）。
    #[test]
    fn the_relative_path_follows_the_owner_type() {
        assert!(relative_path_for("actor", "abc", "https://x/y/a.png", None)
            .expect("可算")
            .starts_with("actors/abc."));
        assert!(
            relative_path_for("movie_plot", "ABC-123", "https://x/y/p.jpg", Some(2))
                .expect("可算")
                .ends_with("/plot-2.jpg")
        );
        assert_eq!(
            relative_path_for("movie_plot", "ABC-123", "https://x/y/p.jpg", None)
                .expect_err("剧情图没有序号就是调用方 bug")
                .code(),
            "plot_index_required"
        );
        assert!(relative_path_for("nope", "k", "https://x/y/a.jpg", None).is_err());
    }

    /// ★ 构造器级别的语义：演员按 **JavDB id 去重**、URL 为空/缺失的条目
    /// **跳过**、封面与剧情图各自归位。
    ///
    /// 夹具用 lazy 池 —— 构造任务不碰数据库，`Db` 只是结构体字段。
    fn builder_service() -> MovieImageService {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://offline@127.0.0.1/offline")
            .expect("lazy 池不需要真实库");
        MovieImageService::new(
            &pool,
            std::env::temp_dir().join("sm-movie-image-builder-test"),
            Box::new(|_, _| Ok(())),
        )
    }

    /// ★ `#[tokio::test]` 而非 `#[test]`：`connect_lazy` 的池在创建时就要往
    /// 当前运行时里挂维护任务 —— 同步测试没有运行时，直接 panic。
    #[tokio::test]
    async fn the_builder_dedupes_actors_and_skips_empty_urls() {
        let service = builder_service();
        let actors = [
            serde_json::json!({"javdb_id": "a1", "avatar_url": "https://x/a1.jpg"}),
            serde_json::json!({"javdb_id": "a1", "avatar_url": "https://x/a1-again.jpg"}),
            serde_json::json!({"javdb_id": "a2"}),
            serde_json::json!({"javdb_id": "a3", "avatar_url": "https://x/a3.png"}),
        ];
        let tasks = service
            .build_movie_import_image_tasks(
                "ABC-123",
                Some("https://x/cover.jpg"),
                &[
                    "https://x/p0.jpg".to_owned(),
                    String::new(),
                    "https://x/p2.jpg".to_owned(),
                ],
                &actors,
            )
            .expect("构造成功");

        assert!(tasks.cover.is_some(), "有封面 URL 就有封面任务");
        assert_eq!(tasks.plots.len(), 2, "空 URL 的剧情图跳过");
        assert_eq!(
            tasks
                .plots
                .iter()
                .map(|task| task.plot_index)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(2)],
            "序号保留原位，不重排"
        );
        assert_eq!(
            tasks
                .actor_avatars
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["a1", "a3"],
            "同一演员只下一张；没有头像 URL 的跳过"
        );
    }

    /// 全部 URL 为空 → 空任务集，**不是错误**。
    #[tokio::test]
    async fn an_all_empty_import_yields_an_empty_task_set() {
        let service = builder_service();
        let tasks = service
            .build_movie_import_image_tasks("ABC-123", None, &[], &[])
            .expect("构造成功");
        assert!(tasks.cover.is_none());
        assert!(tasks.plots.is_empty());
        assert!(tasks.actor_avatars.is_empty());
        assert!(tasks.flatten().is_empty());
    }
}
