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

use sm_db::repo::{ImageRepository, MediaThumbnailRepository};
use sm_db::Db;

use crate::catalog::image_store::{
    backup_pack_path, cleanup_stale_temp_files, read_image_bytes, temp_pack_path, write_pack,
};
use crate::catalog::media_paths::{
    self, MEDIA_THUMBNAILS_PACK_SUFFIX, MEDIA_THUMBNAILS_SUBDIR, MOVIE_MEDIA_SUBDIR,
};
use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 缩略图格式。**只接受 WebP**。
pub const THUMBNAIL_FORMAT: &str = "webp";

/// provider 产出的一件缩略图。
///
/// ⚠️ **这个形状是骨架自造的**：上游的 `ThumbnailArtifact` 来自插件
/// `provider_protocol`，字段是 `relative_path: str`（**相对 workspace 的路径**）
/// 加 `offset_seconds`，**没有** `thumbnail_id`（那是宿主侧登记时才产生的）。
/// 插件 ABI 落地时按彼时的协议改，别顺着这里的形状实现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailArtifact {
    /// 缩略图 id（宿主侧，provider 带回）。
    pub thumbnail_id: i64,
    /// 在媒体里的偏移（秒）。**必须 >= 0**。
    pub offset_seconds: i64,
    /// provider 写出的文件路径（**在 workspace 内**）。
    pub path: std::path::PathBuf,
}

/// 已落盘登记的缩略图。上游 `MediaThumbnailResource`。
///
/// # 字段名与骨架不同（骨架是错的）
///
/// | 骨架 | 上游 |
/// |---|---|
/// | `id` | `thumbnail_id` |
/// | `offset` | `offset_seconds` |
/// | `image_path` | `image`（一个带签名的 `ImageResource`）|
/// | — | `width` / `height`（可空）|
///
/// # `image` 在这一层是**原始路径**，`width`/`height` 是**整组共享**的
///
/// 签名要密钥，只有 API 层有 —— 所以这一层带 `image_id` + `image_origin`，
/// 由 `sm_api::dto::MediaThumbnailResource` 组装（同 `MediaPointValue` 的约定）。
///
/// `width`/`height` 取自**第一条**缩略图（上游 `read_dimensions(thumbnails[0])`）：
/// 同一媒体的一组缩略图来自同一个视频流，尺寸相同，逐条去解纯属浪费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaThumbnailValue {
    pub thumbnail_id: i32,
    pub media_id: i32,
    pub offset_seconds: i32,
    pub image_id: i32,
    /// 图片的相对路径，**未签名**。
    pub image_origin: String,
    /// 视频流宽度。`None` = 解不出来（**不报错**，上游只记一条 warn）。
    pub width: Option<u32>,
    pub height: Option<u32>,
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
///
/// # 形状与骨架不同
///
/// 骨架是 `pub struct ThumbnailArtifactService;` —— **无状态单元结构体**，
/// 方法都是关联函数。于是「目录在哪」只能靠猜，骨架猜成了
/// `<image_root>/thumbnails/<media_id>/`，而**上游是按媒体归属分的**（见
/// [`Self::thumbnail_directory`]）。现在持有 `Db` 与 `ConfigService`。
pub struct ThumbnailArtifactService {
    db: Db,
    config: ConfigService,
}

impl ThumbnailArtifactService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
        }
    }

    /// 该媒体的缩略图目录。
    ///
    /// # 目录随媒体归属走，**不是** `<image_root>/thumbnails/<media_id>/`
    ///
    /// ```text
    ///   JAV 媒体:   <root>/movies/<sha1 前2位>/<番号>/media/<media_id>/thumbnails
    ///   视频条目:   <root>/videos/<video_item_id>/media/<media_id>/thumbnails
    /// ```
    ///
    /// 这么排的理由与 `assets.zip` 同源：**同一部影片的全部资产在同一个目录树下**，
    /// 删影片/搬影片时是一个 `rm -rf`（而不是散在全局 `thumbnails/` 里按 id
    /// 一个个找）。骨架那个扁平布局会让「删一部影片」变成一次全表扫描。
    pub fn thumbnail_directory(
        &self,
        media: &sm_db::Media,
    ) -> Result<std::path::PathBuf, ServiceError> {
        let namespace = match media.movie_number.as_deref() {
            Some(number) => media_paths::movie_asset_relative_dir(
                &media_paths::normalize_asset_dir_name(number),
            ),
            None => {
                // 上游这里写 `Path("videos") / str(media.video_item_id)`，两者都空
                // 时会**静默**产出 `videos/None/` 这个幻影目录 —— 缩略图写进去
                // 再也没有人找得到。`media` 表上两者是 XOR（至多一个非空），
                // 全空说明数据脏了，报错比写进幻影目录好。
                let Some(video_item_id) = media.video_item_id else {
                    return Err(ServiceError::validation(
                        "thumbnail_namespace_unresolved",
                        format!(
                            "媒体 {} 既没有番号也没有视频条目，无法定位缩略图目录",
                            media.id
                        ),
                    ));
                };
                std::path::PathBuf::from("videos").join(video_item_id.to_string())
            }
        };
        Ok(media_paths::media_image_root_path(&self.config)?
            .join(namespace)
            .join(MOVIE_MEDIA_SUBDIR)
            .join(media.id.to_string())
            .join(MEDIA_THUMBNAILS_SUBDIR))
    }

    /// 该媒体的缩略图**包**路径：与 `thumbnails/` 目录**同级同名** + `.zip`。
    ///
    /// 与 `assets.zip` 同样的理由（一次传输拿到全部缩略图）。这个命名不是随手
    /// 起的：`media_paths::image_pack_relative_path` 靠「父目录叫 `thumbnails`」
    /// 反推包路径，改名会让那条约定失效。
    pub fn thumbnail_pack_file(
        &self,
        media: &sm_db::Media,
    ) -> Result<std::path::PathBuf, ServiceError> {
        let directory = self.thumbnail_directory(media)?;
        Ok(directory.with_file_name(format!(
            "{MEDIA_THUMBNAILS_SUBDIR}{MEDIA_THUMBNAILS_PACK_SUFFIX}"
        )))
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
    ///
    /// # 收 `&Media` 而不是 `media_id`（骨架是后者）
    ///
    /// 目录要按归属算（见 [`Self::thumbnail_directory`]），初始索引状态也要看
    /// `movie_number`：JAV 缩略图进向量库（`PENDING`），非 JAV 的落 `SKIPPED`
    /// 从不入库。只给一个 id 这两件事都办不到。
    ///
    /// # 待办的三段结构（上游 `persist`）
    ///
    /// ```text
    ///   1. 写临时包（STORED，按 offset 升序）
    ///   2. 有旧包就 `旧包 -> .bak`，再 `临时包 -> 正式包`；失败则回滚
    ///   3. 登记 Image + MediaThumbnail；**登记失败要把包回滚成旧包**
    /// ```
    ///
    /// 第 3 步的回滚是这里唯一的难点：DB 提交失败时包不能留在新状态（否则
    /// 「文件是新的、记录是旧的」——那种状态没有重试机会，因为下次生成会认为
    /// 「已有」）。
    pub async fn persist(
        &self,
        media: &sm_db::Media,
        artifacts: &[(ThumbnailArtifact, std::path::PathBuf)],
    ) -> Result<u32, ServiceError> {
        if artifacts.is_empty() {
            // 生成侧已保证最小数量；空输入**不触碰任何文件**（上游同款）。
            // 顺手把旧包删掉是错的：那是「用一次空输入抹掉已有产物」。
            return Ok(0);
        }

        let target_dir = self.thumbnail_directory(media)?;
        let pack_path = self.thumbnail_pack_file(media)?;
        let image_root = media_paths::media_image_root_path(&self.config)?;

        // 按 offset 升序 —— 包的内容要能按字节稳定复现，顺序必须确定。
        let mut ordered: Vec<&(ThumbnailArtifact, std::path::PathBuf)> = artifacts.iter().collect();
        ordered.sort_by_key(|(artifact, _)| artifact.offset_seconds);

        // 条目名是 `<offset>.webp`，**不是** provider 给的文件名：
        // 同一时刻点重新生成时文件名可能变（provider 的自由），而条目名必须稳定
        // —— 否则包里会同时出现 `1.webp` 与 `frame_0001.webp` 两份同一帧。
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(ordered.len());
        for (artifact, source) in &ordered {
            let bytes = std::fs::read(source).map_err(|error| {
                ServiceError::from(sm_db::DbError::business(
                    "ThumbnailArtifact",
                    format!("读取缩略图产物 {} 失败：{error}", source.display()),
                ))
            })?;
            entries.push((format!("{}.webp", artifact.offset_seconds), bytes));
        }

        cleanup_stale_temp_files(&pack_path);
        let tmp_path = temp_pack_path(&pack_path);
        if let Err(error) = write_pack(&tmp_path, &entries) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }

        // 阶段一：包替换。旧包先挪到 `.bak`（**不能直接删** —— 下一步失败要退回来）。
        let backup_path = backup_pack_path(&pack_path);
        let had_pack = pack_path.is_file();
        if let Err(error) = swap_pack(&pack_path, &tmp_path, &backup_path, had_pack) {
            restore_pack(&pack_path, &backup_path, had_pack);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }

        // 阶段二：登记。失败要把包**退回旧的那一份** —— 否则「文件是新的、记录是
        // 旧的」，而那个状态没有重试机会（下次生成会认为「已有」）。
        if let Err(error) = self
            .register_artifacts(media, &target_dir, &image_root, &ordered)
            .await
        {
            restore_pack(&pack_path, &backup_path, had_pack);
            return Err(error);
        }

        // 阶段三：清备份。删不掉只记一条 warn —— 产物与记录都已就位，此时失败
        // 不该让调用方以为「没落盘」。
        if backup_path.exists() {
            if let Err(error) = std::fs::remove_file(&backup_path) {
                tracing::warn!(
                    path = %backup_path.display(),
                    %error,
                    "删除缩略图包备份失败（产物与记录都已落盘，不影响本次结果）"
                );
            }
        }
        Ok(entries.len() as u32)
    }

    /// 登记 `Image` + `MediaThumbnail`，**在一个事务里**。
    ///
    /// # 为什么必须原子
    ///
    /// 一媒体有十几张缩略图。逐条提交的话，中途失败会留下「包里有 12 条、
    /// 数据库只有 3 条」—— 那 9 条成了**幽灵**：包分发出去客户端能看到，
    /// 而任何按数据库算的列表/清理都看不见它们。
    async fn register_artifacts(
        &self,
        media: &sm_db::Media,
        target_dir: &std::path::Path,
        image_root: &std::path::Path,
        artifacts: &[&(ThumbnailArtifact, std::path::PathBuf)],
    ) -> Result<(), ServiceError> {
        // JAV 缩略图进向量库（`PENDING`）；非 JAV 的落 `SKIPPED`，从不入库 ——
        // 图搜是影片维度的能力。
        let initial_status = if media.movie_number.is_some() {
            sm_db::playback::media::image_search_index_status::PENDING
        } else {
            sm_db::playback::media::image_search_index_status::SKIPPED
        };

        let mut records = Vec::with_capacity(artifacts.len());
        for (artifact, _source) in artifacts {
            let absolute = target_dir.join(format!("{}.webp", artifact.offset_seconds));
            let relative = absolute.strip_prefix(image_root).map_err(|_| {
                ServiceError::from(sm_db::DbError::business(
                    "ThumbnailArtifact",
                    format!(
                        "缩略图 {} 不在图片根 {} 之内",
                        absolute.display(),
                        image_root.display()
                    ),
                ))
            })?;
            // 偏移在 `ThumbnailArtifact` 里是 `i64` 而列是 `i32`。收窄而不是
            // `as` 截断：截断会让一个超大的偏移**悄悄变成另一个时刻点**，
            // 那比报错难查得多。
            let offset_seconds = i32::try_from(artifact.offset_seconds).map_err(|_| {
                ServiceError::validation(
                    "thumbnail_offset_invalid",
                    format!("缩略图偏移超出可表示范围：{}", artifact.offset_seconds),
                )
            })?;
            records.push(sm_db::repo::ThumbnailArtifactRecord {
                origin: relative.to_string_lossy().replace('\\', "/"),
                offset_seconds,
            });
        }

        let mut unit = sm_db::repo::UnitOfWork::begin(&self.db).await?;
        unit.record_thumbnail_artifacts(media.id, &records, initial_status)
            .await?;
        unit.commit().await?;
        Ok(())
    }

    /// 读图片尺寸。`None` = 解不出来（**不报错**）。
    ///
    /// 上游 `read_dimensions(image_origin)`：`read_image_bytes`（包优先、单文件
    /// 兜底）拿到字节后用 Pillow 解。这里换成 `svc_image::webp::decode`。
    ///
    /// # 为什么返回 `Option` 而不是 `Result`
    ///
    /// 上游的调用方（[`Self::list_media_thumbnails`]）把异常捕掉、只记一条 warn，
    /// 然后 `width`/`height` 留空 —— 尺寸是**锦上添花**（前端拿它做占位比例），
    /// 拿不到不该让整个列表 500。
    pub fn read_dimensions(&self, image_origin: &str) -> Option<(u32, u32)> {
        let root = media_paths::media_image_root_path(&self.config).ok()?;
        let bytes = read_image_bytes(&root, image_origin).ok()?;
        // `width` / `height` 是 `DynamicImage` 的**固有方法**，不需要引
        // `GenericImageView` 那个 trait（sm-service 也没有 `image` 这个直接依赖）。
        let image = svc_image::webp::decode(&bytes).ok()?;
        Some((image.width(), image.height()))
    }

    /// 列出该媒体的缩略图。**按 `(offset, id)` 升序**。
    ///
    /// 排序不能省：选图逻辑（见 `discovery::moment_recommendation`）依赖
    /// 「中位数」这类位置语义，无序会让选出的图不稳定。
    ///
    /// `width`/`height` 只解**第一条** —— 同一组缩略图来自同一个视频流，尺寸相同
    /// （上游同款）。逐条去解会为几十张图做几十次解码，而结果必然一样。
    pub async fn list_media_thumbnails(
        &self,
        media_id: i32,
    ) -> Result<Vec<MediaThumbnailValue>, ServiceError> {
        let thumbnails = MediaThumbnailRepository::new(self.db.clone())
            .list_all_by_media(media_id)
            .await?;
        if thumbnails.is_empty() {
            return Ok(Vec::new());
        }
        // 一次批量取图，不是逐条 —— 一部长片几十张缩略图，逐条就是 N+1。
        let mut image_ids: Vec<i32> = thumbnails.iter().map(|row| row.image_id).collect();
        image_ids.sort_unstable();
        image_ids.dedup();
        let images = ImageRepository::new(self.db.clone())
            .find_by_ids(&image_ids)
            .await?;

        // 解尺寸前先拿到第一条的 origin。缺图（外键被绕过）时留 `None`。
        let first_origin = thumbnails
            .first()
            .and_then(|row| images.get(&row.image_id))
            .map(|image| image.origin.clone());
        let (width, height) = match first_origin {
            Some(origin) => match self.read_dimensions(&origin) {
                Some((width, height)) => (Some(width), Some(height)),
                None => {
                    tracing::warn!(
                        media_id,
                        origin,
                        "缩略图尺寸解不出来，width/height 留空（上游同款：只记一条 warn）"
                    );
                    (None, None)
                }
            },
            None => (None, None),
        };

        Ok(thumbnails
            .into_iter()
            .filter_map(|row| {
                let image = images.get(&row.image_id)?;
                Some(MediaThumbnailValue {
                    thumbnail_id: row.id,
                    media_id: row.media_id,
                    offset_seconds: row.offset,
                    image_id: image.id,
                    image_origin: image.origin.clone(),
                    width,
                    height,
                })
            })
            .collect())
    }
}

/// 把临时包扶正：有旧包先挪到 `.bak`，再把临时包挪到正式位。
///
/// # 为什么旧包是「挪走」而不是「删掉」
///
/// 下一步（DB 登记）可能失败，那时要能**退回旧包**。删了就没得退 —— 而
/// 「包是新的、记录是旧的」这种状态没有重试机会：下次生成会认为「已经有产物」。
fn swap_pack(
    pack_path: &std::path::Path,
    tmp_path: &std::path::Path,
    backup_path: &std::path::Path,
    had_pack: bool,
) -> Result<(), ServiceError> {
    if had_pack {
        std::fs::rename(pack_path, backup_path).map_err(|error| {
            ServiceError::from(sm_db::DbError::business(
                "ThumbnailArtifact",
                format!("备份旧缩略图包 {} 失败：{error}", pack_path.display()),
            ))
        })?;
    }
    std::fs::rename(tmp_path, pack_path).map_err(|error| {
        ServiceError::from(sm_db::DbError::business(
            "ThumbnailArtifact",
            format!("缩略图包原子替换失败：{error}"),
        ))
    })
}

/// 回滚包：有旧包就退回去，否则删掉刚扶正的那个。
///
/// 两条分支对应「本来有包」与「本来没有」：后者不能留一个新包在那儿，否则
/// 包与（失败的）登记状态不一致。
///
/// 回滚本身失败**只记 warn**：调用方已经在处理一个错误了，再抛一个只会把
/// 真正的成因盖掉。
fn restore_pack(pack_path: &std::path::Path, backup_path: &std::path::Path, had_pack: bool) {
    let outcome = if had_pack && backup_path.exists() {
        std::fs::rename(backup_path, pack_path)
    } else {
        match std::fs::remove_file(pack_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    };
    if let Err(error) = outcome {
        tracing::warn!(
            pack = %pack_path.display(),
            %error,
            "缩略图包回滚失败 —— 包与数据库记录可能已不一致，需要人工核对"
        );
    }
}
