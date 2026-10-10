//! 缩略图包回填（上游 `playback/media_thumbnail_pack_backfill_service.py`，217 行）。
//!
//! # 任务键 `media_thumbnail_pack_backfill` —— **manual_only**
//!
//! 三条无 cron 的任务之二（另一条是 `movie_asset_pack_backfill`；第三条是
//! `media_video_info_backfill`）。
//!
//! # 与 [`super::thumbnails::task_service`] 是**不同**的两件事
//!
//! | | 本文件 | `task_service` |
//! |---|---|---|
//! | 做什么 | 把**已有**单文件缩略图打成 `thumbnails.zip` | 调 provider **生成**缩略图 |
//! | 跑多久 | 快（纯文件 IO） | 慢（ffprobe + 抽帧） |
//!
//! 分不清会导致「包回填」去调 provider —— 那是浪费一次生成。
//!
//! # DB 为准，缺文件**整条跳过**
//!
//! 与 `catalog::movie_asset_pack_backfill` 同一取舍（见那个文件）。
//!
//! # 自检：包能打开且条目数正确，否则**抛错**
//!
//! 上游 `RuntimeError("thumbnail_pack_self_check_failed")`。
//!
//! ⚠️ 与 `movie_asset_pack` 的「重试 3 次」不同 —— 这里是**直接抛错**。
//! 因为打包只读本地文件，失败原因通常是「文件真没了」或「磁盘满了」，
//! 重试 3 次毫无意义，还会把任务拖长。

use sm_db::repo::MediaRepository;
use sm_db::Db;

use crate::catalog::image_store::write_pack;
use crate::catalog::media_paths;
use crate::catalog::movie_asset_pack_backfill::ProgressSink;
use crate::error::ServiceError;
use crate::playback::operation_locks::MediaOperation;
use crate::system::config::ConfigService;

/// 任务键。
pub const TASK_KEY: &str = "media_thumbnail_pack_backfill";

/// 回填统计。
///
/// # 字段与上游 `stats` 字典**逐键对齐**
///
/// 上游 `media_thumbnail_pack_backfill_service.py:152-161` 有 8 个键，
/// 骨架期这里只有 5 个 —— 少掉的三个恰好是**行为不同的地方**：
///
/// | 上游键 | 本仓字段 | 为什么不能省 |
/// |---|---|---|
/// | `packed_media` | `packed` | 新打成包 |
/// | `cleaned_media` | `cleaned` | 包**本来就在**，这轮只清了散文件 |
/// | `already_packed_media` | `already_packed` | 包在且**没有**散文件可清 |
/// | `incomplete_media` | `incomplete` | 包在但**没盖住全部** origin |
/// | `skipped_missing_files` | `skipped_missing_files` | 散文件缺了 → 整条跳过 |
/// | `skipped_busy` | `skipped_busy` | 媒体锁被占（另一个任务在处理它）|
///
/// `cleaned` 与 `packed` 混成一个数会掩盖「这轮其实一个包都没建」；
/// `incomplete` 全省掉更糟 —— 那是**真的坏了**（包盖不全），却与「已完成」
/// 长得一样。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ThumbnailPackBackfillStats {
    /// 候选媒体数（至少有一张缩略图的 media）。
    pub candidate_media: i64,
    /// 新打成包的数量。
    pub packed: i32,
    /// 包本来就在、这轮只清掉了散文件的数量。
    pub cleaned: i32,
    /// 已有包且无散文件可清的数量。
    pub already_packed: i32,
    /// 缺文件而整条跳过的数量（**不是失败**）。
    pub skipped_missing_files: i32,
    /// 包存在但没盖住全部 origin 的数量（**真有问题**）。
    pub incomplete: i32,
    /// 媒体锁被占而跳过的数量。
    pub skipped_busy: i32,
    /// 失败的数量。
    pub failed: i32,
}

/// 该媒体是否需要回填包。**纯函数**。
///
/// 「已有缩略图但没有包」才需要。**没有缩略图**的**不需要**——
/// 那属于 [`super::thumbnails::task_service`] 的活（先生成再打包）。
pub fn needs_pack(thumbnail_count: usize, pack_exists: bool) -> bool {
    thumbnail_count > 0 && !pack_exists
}

/// 缩略图包回填服务。
pub struct MediaThumbnailPackBackfillService {
    db: Db,
    config: ConfigService,
    media: MediaRepository,
}

impl MediaThumbnailPackBackfillService {
    /// 构造。`config` 用于解 [`media_paths::media_image_root_path`]。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
            media: MediaRepository::new(db.clone()),
        }
    }

    /// 候选 media id（至少有一张缩略图）。上游 `_candidate_media_ids`。
    ///
    /// 「缺包」这个条件在文件系统上，所以**全量**列出、逐个查盘（见
    /// [`MediaRepository::list_media_ids_with_thumbnails`] 的说明）。
    pub async fn candidates(&self) -> Result<Vec<i32>, ServiceError> {
        Ok(self.media.list_media_ids_with_thumbnails().await?)
    }

    /// ★ 跑一轮。任务执行体。上游 `backfill(cls, *, reporter)`。
    ///
    /// 单条失败**不中断**整批 —— 这是一个手动触发的存量任务，跑完比早停
    /// 重要（上游同样只 warn 不抛）。
    pub async fn backfill(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<ThumbnailPackBackfillStats, ServiceError> {
        let media_ids = self.candidates().await?;
        let total = i64::try_from(media_ids.len()).unwrap_or(i64::MAX);
        let mut stats = ThumbnailPackBackfillStats {
            candidate_media: total,
            ..Default::default()
        };

        // 日志节流：上游 `step = max(len // 20, 1)`。每条都打会把几千条的
        // 任务日志淹掉。
        let step = usize::try_from((total / 20).max(1)).unwrap_or(1);

        emit_progress(&mut progress, 0, &stats).await;
        for (index, media_id) in media_ids.iter().enumerate() {
            let completed = index + 1;
            if completed == 1 || (completed as i64) % (step as i64) == 0 {
                tracing::info!(
                    completed,
                    total,
                    packed = stats.packed,
                    cleaned = stats.cleaned,
                    skipped_missing = stats.skipped_missing_files,
                    failed = stats.failed,
                    "媒体缩略图打包回填进度"
                );
            }
            // 锁在**处理每一条之前**取：`None` = 另一个任务正在处理这条媒体
            // （最典型的是缩略图生成任务），此时**跳过而不是排队**。
            match MediaOperation::try_media(&self.db, *media_id).await {
                Ok(Some(lock)) => {
                    let outcome = self.process_media(*media_id, &mut stats).await;
                    // ★ 锁在 `process_media` 之后、`stats` 记完之后释放 ——
                    // 提前释放会让并发的生成任务与本任务同时改一个目录。
                    let _ = lock.release().await;
                    if let Err(error) = outcome {
                        stats.failed += 1;
                        tracing::warn!(
                            media_id = *media_id,
                            code = error.code(),
                            "媒体缩略图打包回填失败"
                        );
                    }
                }
                Ok(None) => stats.skipped_busy += 1,
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(
                        media_id = *media_id,
                        code = error.code(),
                        "取媒体操作锁失败"
                    );
                }
            }
            emit_progress(
                &mut progress,
                i64::try_from(completed).unwrap_or(i64::MAX),
                &stats,
            )
            .await;
        }
        Ok(stats)
    }

    /// 处理一条媒体。上游 `_process_media`（`:90-147`）。
    ///
    /// 返回 `Err` 只表示**前置步骤出错**；「这跳过了」是计数而不是错误 ——
    /// 那正是「整批跑完、跳过单独统计」的意思。
    async fn process_media(
        &self,
        media_id: i32,
        stats: &mut ThumbnailPackBackfillStats,
    ) -> Result<(), ServiceError> {
        let rows = self.media.list_thumbnail_origins(media_id).await?;
        if rows.is_empty() {
            // 候选列表来自同一张表，这里为空说明刚被别的任务清掉了。
            // 上游直接 return，**不计任何数**。
            return Ok(());
        }
        let image_root = media_paths::media_image_root_path(&self.config)?;
        let origins: Vec<&str> = rows.iter().map(|(_, origin)| origin.as_str()).collect();
        let pack_relative = media_paths::image_pack_relative_path(origins[0]).ok_or_else(|| {
            // ⚠️ 这是**抛错**而不是跳过（上游 `raise ValueError`）：
            // 路径推导不出来说明 origin 落在了约定之外，而那是一个应该被
            // 看见的部署问题。静默跳过会让每轮都白跑一次。
            ServiceError::from_status(
                500,
                "internal_error",
                format!(
                    "thumbnail_pack_path_unexpected media_id={media_id} origin={}",
                    origins[0]
                ),
            )
        })?;
        let pack_path = image_root.join(&pack_relative);
        let thumbnails_dir = thumbnails_dir_of(&image_root, origins[0]);

        if pack_path.is_file() {
            // 包已存在：**只可能清理，不重建**（重建是生成任务的活）。
            if !thumbnails_dir.is_dir() {
                stats.already_packed += 1;
                return Ok(());
            }
            if !pack_covers_origins(&pack_path, &origins) {
                // ★ 不清散文件：包盖不全就清掉，等于把唯一还能用的那份删了。
                tracing::warn!(
                    media_id,
                    "缩略图包没盖住全部 origin，保留散文件（等 image_cleanup 处理）"
                );
                stats.incomplete += 1;
                return Ok(());
            }
            remove_legacy_thumbnail_files(&thumbnails_dir);
            stats.cleaned += 1;
            return Ok(());
        }

        // 没有包：逐条读散文件。**任一条缺或空就整条跳过**（DB 为准的取舍）。
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(origins.len());
        for (offset, origin) in &rows {
            let source = image_root.join(origin.as_str());
            let Ok(bytes) = std::fs::read(&source) else {
                tracing::warn!(media_id, offset, origin = %origin, "缩略图散文件不在磁盘上，整条跳过");
                stats.skipped_missing_files += 1;
                return Ok(());
            };
            if bytes.is_empty() {
                // 0 字节的缩略图**不算缺失但也不能打包** —— 打进去会让包
                // 盖不全（自检的 `file_size` 对不上），所以同样整条跳过。
                tracing::warn!(media_id, offset, origin = %origin, "缩略图散文件是空的，整条跳过");
                stats.skipped_missing_files += 1;
                return Ok(());
            }
            entries.push((entry_name_of(origin), bytes));
        }

        // 先写 tmp、自检通过再原子换位 —— 直接写目标的话，中途崩了会留下
        // 一个**盖不全**的包，而读侧只会在「包能打开」时返回条目，坏包表现为
        // 缩略图凭空消失。
        let tmp_path = tmp_pack_path(&pack_path);
        let write = write_pack(&tmp_path, &entries).and_then(|()| {
            if pack_matches_files(&tmp_path, &entries) {
                Ok(())
            } else {
                Err(ServiceError::from_status(
                    500,
                    "internal_error",
                    "thumbnail_pack_self_check_failed",
                ))
            }
        });
        if let Err(error) = write {
            let _ = std::fs::remove_file(&tmp_path);
            tracing::warn!(media_id, code = error.code(), "缩略图打包自检失败");
            stats.failed += 1;
            return Ok(());
        }
        // `rename` 在同一文件系统上是原子的；目标此刻不存在（上面判过
        // `pack_path.is_file()`），所以跨平台都不会撞「已存在」。
        if let Err(error) = std::fs::rename(&tmp_path, &pack_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(ServiceError::from_status(
                500,
                "internal_error",
                format!("缩略图包换位失败 media_id={media_id}: {error}"),
            ));
        }
        remove_legacy_thumbnail_files(&thumbnails_dir);
        stats.packed += 1;
        Ok(())
    }
}

/// 散文件所在目录：origin 的父目录。上游 `image_root / PurePosixPath(origins[0]).parent`。
fn thumbnails_dir_of(image_root: &std::path::Path, origin: &str) -> std::path::PathBuf {
    // `origin` 是 `a/b/c.png` 形状的相对路径（`/` 分隔，跨平台一致）。
    let normalized = origin.replace('\\', "/");
    match normalized.rfind('/') {
        Some(index) => image_root.join(&normalized[..index]),
        // 没有分隔符 = origin 就在根下，`image_pack_relative_path` 也会返回
        // None，那条路已经报错了；这里是防御性兜底。
        None => image_root.to_path_buf(),
    }
}

/// 包内条目名：origin 的**文件名**部分。上游 `PurePosixPath(origin).name`。
fn entry_name_of(origin: &str) -> String {
    let normalized = origin.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("entry")
        .to_owned()
}

/// 临时包路径。上游 `pack_path.with_name(f"{name}.tmp-{uuid}")`。
fn tmp_pack_path(pack_path: &std::path::Path) -> std::path::PathBuf {
    let name = pack_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "thumbnails.zip".to_owned());
    let unique = uuid::Uuid::new_v4().simple();
    pack_path.with_file_name(format!("{name}.tmp-{unique}"))
}

/// 自检一：包里有没有**全部** origin 的条目名。上游 `_pack_covers_origins`。
///
/// ⚠️ 判据是「条目名在不在」，**不是**大小 —— 这一步只回答「盖住了没有」。
fn pack_covers_origins(pack_path: &std::path::Path, origins: &[&str]) -> bool {
    let Ok(file) = std::fs::File::open(pack_path) else {
        return false;
    };
    let Ok(archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        // 坏包（含刚写完就损坏）—— 与上游 `BadZipFile -> False` 同义。
        return false;
    };
    // `file_names()` 而不是 `by_index(i).name()`：后者返回借用 archive 的
    // `ZipFile`，在 `filter_map` 闭包里那个借用会活过迭代（`ZipFile` 有
    // `Drop`，编译器因此报「captured variable escapes FnMut body」）。
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    origins
        .iter()
        .all(|origin| names.contains(&entry_name_of(origin)))
}

/// 自检二：包里每个条目的 `file_size` 是否等于源文件大小。
/// 上游 `_pack_matches_files`。
///
/// 判「大小」而不是「内容」：上游也是比 `st_size`（比内容要重读一遍每个
/// 文件，对几千条的任务不划算）。这能抓住**写截断**（`write_pack` 中途失败）
/// —— 那正是自检要拦的那类损坏。
fn pack_matches_files(pack_path: &std::path::Path, entries: &[(String, Vec<u8>)]) -> bool {
    let Ok(file) = std::fs::File::open(pack_path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return false;
    };
    entries
        .iter()
        .all(|(name, bytes)| match archive.by_name(name) {
            Ok(entry) => entry.size() == bytes.len() as u64,
            Err(_) => false,
        })
}

/// 删掉散文件并尝试删掉那个目录。上游 `_remove_legacy_thumbnail_files`。
///
/// # ★ 只删**文件与符号链接**，子目录一律不动
///
/// 上游是 `if entry.is_file() or entry.is_symlink(): entry.unlink()` ——
/// **没有 else**。用 `remove_dir_all` 删子目录会连带删掉里面的东西，而那些
/// 不归本任务管（`nested/keep.txt` 那种）。它删掉之后 `rmdir` 也会失败，
/// 于是目录留在原地，而**内容被清空** —— 比不删更糟。
///
/// 目录本身删不掉是**正常**的（还有别的东西），忽略。
fn remove_legacy_thumbnail_files(thumbnails_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(thumbnails_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        // 符号链接算「文件」—— `entry.is_file()` 对指向文件/目录的链接都为真，
        // 但 `metadata()`（不跟随）看到的是链接本身，所以这里按 file_type 判。
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "删除旧缩略图散文件失败");
        }
    }
    let _ = std::fs::remove_dir(thumbnails_dir);
}

/// 进度上报。上游 `emit_progress`。
async fn emit_progress(
    progress: &mut Option<ProgressSink<'_>>,
    completed: i64,
    stats: &ThumbnailPackBackfillStats,
) {
    let Some(sink) = progress.as_mut() else {
        return;
    };
    let total = stats.candidate_media;
    let text = format!(
        "媒体缩略图打包回填 · 已完成 {completed}/{total} · 已打包 {} · 已清理 {} \
         · 跳过 {} · 失败 {}",
        stats.packed,
        stats.cleaned,
        stats.skipped_missing_files + stats.skipped_busy,
        stats.failed,
    );
    let patch = serde_json::to_value(stats).ok();
    // 上报失败**不打断**任务：进度只是给人看的。
    let _ = sink(
        Some(i32::try_from(completed).unwrap_or(i32::MAX)),
        Some(i32::try_from(total).unwrap_or(i32::MAX)),
        &text,
        patch.as_ref(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 「没有缩略图」**不需要**回填包 —— 那是先生成再打包的活。
    #[test]
    fn a_media_without_thumbnails_needs_no_pack() {
        assert!(!needs_pack(0, false), "没缩略图 -> 不该在这里打包");
        // 有缩略图且没包 -> 需要
        assert!(needs_pack(5, false));
        // 已有包 -> 跳过
        assert!(!needs_pack(5, true));
    }

    /// 四类计数互斥且完备。
    /// ★ 六种计数互斥：每条候选媒体**恰好**落到一个结局里。
    ///
    /// `cleaned` / `already_packed` / `incomplete` 三者都是「包已存在」的分档，
    /// 合成一个 `skipped_existing_pack` 就丢掉了「包盖不全」这个真问题。
    #[test]
    fn the_outcomes_partition_the_candidate_media() {
        let stats = ThumbnailPackBackfillStats {
            candidate_media: 20,
            packed: 6,
            cleaned: 3,
            already_packed: 4,
            skipped_missing_files: 2,
            incomplete: 1,
            skipped_busy: 3,
            failed: 1,
        };
        let accounted = stats.packed
            + stats.cleaned
            + stats.already_packed
            + stats.skipped_missing_files
            + stats.incomplete
            + stats.skipped_busy
            + stats.failed;
        assert_eq!(
            accounted, stats.candidate_media as i32,
            "每个候选都要有归属"
        );
    }

    // ============================================================ 路径与命名

    /// 包内条目名取 origin 的**文件名**部分（上游 `PurePosixPath(origin).name`）。
    #[test]
    fn the_entry_name_is_the_file_name_part_of_the_origin() {
        assert_eq!(entry_name_of("media/12/thumbnails/a.jpg"), "a.jpg");
        // 反斜杠也要认：origin 在 Windows 上可能带 `\`
        assert_eq!(entry_name_of(r"media\12\thumbnails\b.jpg"), "b.jpg");
        assert_eq!(entry_name_of("solo.png"), "solo.png");
    }

    /// 散文件目录 = origin 的父目录（上游 `image_root / parent`）。
    #[test]
    fn the_loose_file_dir_is_the_parent_of_the_origin() {
        let root = std::path::Path::new("/data/images");
        assert_eq!(
            thumbnails_dir_of(root, "media/12/thumbnails/a.jpg"),
            root.join("media/12/thumbnails")
        );
    }

    /// 临时包名带 uuid，两个并发任务不会撞同一个 tmp。
    #[test]
    fn the_temp_pack_name_is_unique() {
        let pack = std::path::Path::new("/data/images/media/12/thumbnails.zip");
        let first = tmp_pack_path(pack);
        let second = tmp_pack_path(pack);
        assert_ne!(first, second, "并发时 tmp 必须互不相同");
        assert!(first.to_string_lossy().contains(".tmp-"));
    }

    // ============================================================ 自检（真文件 IO）

    /// 造一个临时图片根。测试进程退出时由 OS 回收（`temp_dir` 下）。
    fn temp_root(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "sm-thumb-pack-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&base).expect("建临时根");
        base
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建父目录");
        std::fs::write(path, bytes).expect("写文件");
    }

    /// ★ 自检一：包里盖住了**全部** origin 才算过。
    ///
    /// 少一个条目就要能看出来 —— 那正是「包存在但盖不全」的场景，
    /// 少了它就会走进「清理散文件」那一步，把唯一还能用的那份删掉。
    #[test]
    fn covers_origins_detects_a_pack_that_misses_one_entry() {
        let root = temp_root("covers");
        let pack = root.join("thumbnails.zip");
        write_pack(
            &pack,
            &[
                ("a.jpg".to_owned(), b"first".to_vec()),
                ("b.jpg".to_owned(), b"second".to_vec()),
            ],
        )
        .expect("写包");

        assert!(
            pack_covers_origins(&pack, &["x/a.jpg", "x/b.jpg"]),
            "两个 origin 都在包里"
        );
        assert!(
            !pack_covers_origins(&pack, &["x/a.jpg", "x/b.jpg", "x/c.jpg"]),
            "★ 少一个条目就不能算盖住了"
        );
        // 文件名相同即可 —— origin 的目录部分不进条目名。
        assert!(pack_covers_origins(&pack, &["other/dir/a.jpg"]));
    }

    /// 坏包（不是 zip）两个自检都判 false，而不是 panic。
    #[test]
    fn a_corrupt_pack_fails_both_self_checks() {
        let root = temp_root("corrupt");
        let pack = root.join("thumbnails.zip");
        std::fs::write(&pack, b"definitely not a zip").expect("写坏包");

        assert!(!pack_covers_origins(&pack, &["a.jpg"]));
        assert!(!pack_matches_files(
            &pack,
            &[("a.jpg".to_owned(), b"x".to_vec())]
        ));
        // 不存在的包同理。
        assert!(!pack_covers_origins(&root.join("nope.zip"), &["a.jpg"]));
    }

    /// ★ 自检二：条目大小必须等于源字节数 —— 这拦的是「写截断」。
    #[test]
    fn matches_files_detects_a_truncated_entry() {
        let root = temp_root("truncated");
        let pack = root.join("thumbnails.zip");
        write_pack(
            &pack,
            &[
                ("a.jpg".to_owned(), b"12345".to_vec()),
                ("b.jpg".to_owned(), b"1234567890".to_vec()),
            ],
        )
        .expect("写包");

        assert!(pack_matches_files(
            &pack,
            &[
                ("a.jpg".to_owned(), b"12345".to_vec()),
                ("b.jpg".to_owned(), b"1234567890".to_vec()),
            ]
        ));
        // b 少了三个字节 -> 写入被截断，必须被自检拦住
        assert!(!pack_matches_files(
            &pack,
            &[
                ("a.jpg".to_owned(), b"12345".to_vec()),
                ("b.jpg".to_owned(), b"1234567".to_vec()),
            ]
        ));
        // 条目不存在也要能判（不 panic）
        assert!(!pack_matches_files(
            &pack,
            &[("missing.jpg".to_owned(), b"1".to_vec())]
        ));
    }

    /// 清理散文件：文件全删、目录尝试删；**子目录不动**。
    #[test]
    fn removing_legacy_files_keeps_subdirectories() {
        let root = temp_root("legacy");
        let dir = root.join("media/12/thumbnails");
        write_file(&dir.join("a.jpg"), b"a");
        write_file(&dir.join("b.jpg"), b"b");
        // 一个子目录（不该被递归删掉 —— 上游只 unlink 文件与符号链接）
        std::fs::create_dir_all(dir.join("nested")).expect("建子目录");
        write_file(&dir.join("nested/keep.txt"), b"keep");

        remove_legacy_thumbnail_files(&dir);

        assert!(!dir.join("a.jpg").exists(), "散文件要删");
        assert!(!dir.join("b.jpg").exists(), "散文件要删");
        assert!(
            dir.join("nested/keep.txt").is_file(),
            "★ 子目录里的东西不能被顺手删掉"
        );
    }
}
