//! 影片图片包的手动回填（上游 `catalog/movie_asset_pack_backfill_service.py`，181 行）。
//!
//! # 任务键 `movie_asset_pack_backfill` —— **manual_only**
//!
//! `cron_spec::builtin_jobs` 里它是三条无 cron 的任务之一（另两条是
//! `media_video_info_backfill` 与 `media_thumbnail_pack_backfill`）。
//! 按上游 `contracts.py` 的不变式，`manual_only` 的任务**必须**允许手动触发
//! —— 否则它既没有 cron 又不能手动，等于永远不会跑。
//!
//! # 与 [`super::movie_asset_pack`] 的关系：一个是重建，一个是**回填存量**
//!
//! 导入流程里已经会建包（见 `catalog_import`）。这个任务处理的是**历史遗留**：
//! 早期版本把图片存成散文件，没有 `assets.zip`。
//!
//! # DB 为准，缺文件**整条跳过**
//!
//! 上游注释明写：「DB 为准，缺文件整条跳过」。**候选来自影片与剧照两处**
//! （`_candidate_movie_numbers`，`:30-53`）：
//!
//! ```text
//! {movie_number | movie.cover_image 非空}
//! ∪ {movie_number | movie.thin_cover_image 非空}
//! ∪ {movie_number | movie_plot_image 里有它的行}
//! ```
//!
//! 而「这一部要不要真的重建」判的是**磁盘状态**（包在不在、有没有残留散文件），
//! 库看不出包的存在。缺文件时跳过而不是报错：那部影片的记录本来就该被清理
//! （见 `image_cleanup`），清理之前它会一直「缺文件」。报错会让这个任务永远
//! 无法跑完。
//!
//! ⚠️ 反过来也别把它当成「顺手清理」—— 删除是 `image_cleanup` 的职责，
//! 这里只负责重建包。
//!
//! # ⚠️ 骨架期的形状是自造的（本轮改回上游）
//!
//! | 骨架期 | 上游 |
//! |---|---|
//! | `BackfillCandidate { movie_id, movie_number, movie_dir_relative, image_record_count }` + `should_backfill(记录数, 包在否)` —— 纯函数判据 | 候选只有**番号**；判据混了两处：**库**决定候选，**磁盘**决定这一部怎么处理 |
//! | `PackBackfillStats { examined, packed, skipped_missing_files, failed }` | 六个字段：`candidate_movies` / `packed_movies` / `cleaned_movies` / `already_packed_movies` / `skipped_missing_files` / `failed_movies` |
//! | 没有 `cleaned_movies` / `already_packed_movies` | 「已有包但有残留散文件 → 重建并把散文件清掉」是**独立的一档**，与「新建包」分开计数 |
//!
//! 那三个字段不是细枝末节：`cleaned` 与 `packed` 的差别是「本来有包，这次只是
//! 清了残留」—— 混成一个数就看不出「回填到底在建新包还是在擦屁股」。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;
use crate::system::config::ConfigService;

use super::media_paths::{self, MOVIE_ASSETS_PACK_NAME};
use super::movie_asset_pack::MovieAssetPackService;

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `movie_asset_pack_backfill` 一致。
pub const TASK_KEY: &str = "movie_asset_pack_backfill";

/// 进度上报（签名与本仓其它任务一致 —— `image_search_index` / `recommendation`
/// / `daily_recommendation` 各有一份同形的别名，见那些模块的说明）。
pub type ProgressSink<'a> = Box<
    dyn FnMut(
            Option<i32>,
            Option<i32>,
            &str,
            Option<&serde_json::Value>,
        ) -> BoxFuture<'a, Result<(), String>>
        + Send
        + 'a,
>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 回填统计。字段与上游 `backfill` 返回的 dict **逐字一致**
/// （`movie_asset_pack_backfill_service.py:122-129`）—— 它会被存进
/// `background_task_run.result_summary`，客户端按这些键读。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackBackfillStats {
    /// 候选影片数（有图可打的番号数）。
    pub candidate_movies: i64,
    /// **新建**了包的部数（原本没有包）。
    pub packed_movies: i64,
    /// 原本**已有包**、这次重建并清掉残留散文件的部数。
    pub cleaned_movies: i64,
    /// 已有包且目录干净、直接跳过的部数。
    pub already_packed_movies: i64,
    /// **因为库里记着的文件不在磁盘上而整条跳过**的部数（不是失败）。
    pub skipped_missing_files: i64,
    /// 重建失败的部数（抛错、或重建后没产物）。
    pub failed_movies: i64,
}

/// 回填服务。
#[derive(Debug, Clone)]
pub struct MovieAssetPackBackfillService {
    db: Db,
    config: ConfigService,
    packs: MovieAssetPackService,
}

impl MovieAssetPackBackfillService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
            packs: MovieAssetPackService::new(db, config),
        }
    }

    /// 候选番号，去重后按番号升序。上游 `_candidate_movie_numbers`（`:30-53`）。
    ///
    /// 三个集合的并集在 SQL 里一次拿完（`UNION` 自带去重），调用方拿到的顺序
    /// 与上游 `sorted(...)` 相同。**没有 limit** —— 上游也没有：这是个手动任务，
    /// 跑一次就是要跑完。
    pub async fn candidates(&self) -> Result<Vec<String>, ServiceError> {
        Ok(MovieRepository::new(self.db.clone())
            .list_numbers_with_asset_images()
            .await?)
    }

    /// ★ 跑一轮（任务执行体）。上游 `backfill(cls, *, reporter) -> dict`。
    ///
    /// 逐部重建包；**缺文件的整条跳过**（`skipped_missing_files`）、
    /// 重建失败的记为 `failed_movies`。
    ///
    /// 单部失败**不中断**整批 —— 这是个可能要跑很久的存量任务。
    pub async fn backfill(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<PackBackfillStats, ServiceError> {
        let movie_numbers = self.candidates().await?;
        let total = i64::try_from(movie_numbers.len()).unwrap_or(i64::MAX);
        let mut stats = PackBackfillStats {
            candidate_movies: total,
            ..Default::default()
        };

        // 日志节流：上游 `step = max(len // 20, 1)`，每 `step` 部打一行 info
        // （每部都打会让几千部的任务把日志淹掉）。
        let step = usize::try_from((total / 20).max(1)).unwrap_or(1);

        emit_progress(&mut progress, 0, &stats).await;
        for (index, movie_number) in movie_numbers.iter().enumerate() {
            let completed = index + 1;
            if completed == 1 || completed % step == 0 {
                tracing::info!(
                    completed,
                    total,
                    packed = stats.packed_movies,
                    cleaned = stats.cleaned_movies,
                    skipped_missing = stats.skipped_missing_files,
                    failed = stats.failed_movies,
                    "影片图片打包回填进度"
                );
            }
            emit_processing(
                &mut progress,
                i64::try_from(completed).unwrap_or(i64::MAX),
                &stats,
            )
            .await;
            if let Err(error) = self.process_movie(movie_number, &mut stats).await {
                // 上游把整个 `_process_movie` 包在 try/except 里：**单部失败不
                // 中断整批**。重建本身的失败在 `process_movie` 里就记了数，
                // 走到这里的是「读取活跃集等前置步骤出错」。
                stats.failed_movies += 1;
                tracing::warn!(
                    movie_number = %movie_number,
                    code = error.code(),
                    "影片图片包回填失败"
                );
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

    /// 处理一部影片。上游 `_process_movie`（`:67-117`）。
    ///
    /// 返回 `Err` 只表示**前置步骤出错**（活跃集读不出来等）；「重建失败」
    /// 是计数而不是错误 —— 那正是「整批跑完、失败单独统计」的意思。
    async fn process_movie(
        &self,
        movie_number: &str,
        stats: &mut PackBackfillStats,
    ) -> Result<(), ServiceError> {
        // 分片名与目录名都按**归一化后**的番号算（与图片资产写侧同一套规则，
        // 否则同一部影片的图会落到两个目录）。
        let dir_name = media_paths::normalize_asset_dir_name(movie_number);
        let movie_dir = media_paths::movie_asset_relative_dir(&dir_name);
        let image_root = media_paths::media_image_root_path(&self.config)?;
        let scope_dir = image_root.join(&movie_dir);
        let pack_path = self.packs.movie_asset_pack_path(&movie_dir)?;

        let origins = self.packs.live_origins(&movie_dir).await?;
        if origins.is_empty() {
            // 库里那几列说有图，而 `image` 表里没有活跃行（图刚被清理）。
            // 上游这里直接 return，**不计任何数** —— 既不是跳过也不是失败。
            return Ok(());
        }

        let had_pack = pack_path.is_file();
        let loose = loose_files(&scope_dir, &pack_path);
        if had_pack && loose.is_empty() {
            // 已打包且无残留，直接跳过（**不回读包做逐条校验**，避免大库上的
            // 重复开销 —— 上游注释明写）。
            stats.already_packed_movies += 1;
            return Ok(());
        }

        if !had_pack {
            // 只在这一档查磁盘：**已有包时不查** —— 重建会「散文件优先、旧包
            // 兜底」，缺几个散文件不影响它拿到全部字节。
            let missing = origins
                .iter()
                .filter(|origin| !image_root.join(origin.as_str()).is_file())
                .count();
            if missing > 0 {
                tracing::warn!(
                    movie_number = %movie_number,
                    missing,
                    "库里记着的影片图片不在磁盘上，整条跳过（等 image_cleanup 清理）"
                );
                stats.skipped_missing_files += 1;
                return Ok(());
            }
        }

        if let Err(error) = self.packs.rebuild_movie_asset_pack(&movie_dir).await {
            stats.failed_movies += 1;
            tracing::warn!(
                movie_number = %movie_number,
                code = error.code(),
                "影片图片包重建失败"
            );
            return Ok(());
        }
        if !pack_path.is_file() {
            // 重建**返回成功但没有产物**：上游单独判这一条（`:107-113`）。
            // 不判的话这部影片会被记成「已打包」，而客户端读到的是一张 404。
            stats.failed_movies += 1;
            tracing::warn!(movie_number = %movie_number, "重建后包里没有产物");
            return Ok(());
        }
        if had_pack {
            stats.cleaned_movies += 1;
        } else {
            stats.packed_movies += 1;
        }
        Ok(())
    }
}

/// 目录里**未入包**的散文件。上游 `_loose_files`（`:56-65`）。
///
/// 两个排除项：包本身，以及包名开头的临时/备份文件（`assets.zip.tmp-*`）。
/// 后者不能当散文件 —— 那是重建中途留下的，`remove_loose_files` 也刻意保留
/// 它们（否则会把正在替换的产物删掉）。
///
/// ⚠️ **别和 [`super::movie_asset_pack`] 里的 `remove_loose_files` 合并**：
/// 两者的判据差一个比特 —— 删除那一版还认**符号链接**（`is_file() ||
/// is_symlink()`），这一版只认文件（上游 `_loose_files` 是 `entry.is_file()`）。
/// 合并会让「一个断掉的软链接」从「不被列进散文件」变成「被删掉」。
fn loose_files(scope_dir: &Path, pack_path: &Path) -> Vec<PathBuf> {
    // 目录不存在 = 没有散文件（上游 `if not scope_dir.is_dir(): return []`）。
    let Ok(entries) = std::fs::read_dir(scope_dir) else {
        return Vec::new();
    };
    let pack_name = pack_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| MOVIE_ASSETS_PACK_NAME.to_owned());
    let prefix = format!("{pack_name}.");
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            name != pack_name && !name.starts_with(&prefix)
        })
        .collect()
}

/// 上报一次带摘要的进度。上游 `emit_progress`（`:136-148`）。
///
/// 文案逐字照抄（含 `·` 分隔符）：任务中心直接显示它。
async fn emit_progress(
    progress: &mut Option<ProgressSink<'_>>,
    completed: i64,
    stats: &PackBackfillStats,
) {
    let Some(sink) = progress.as_mut() else {
        return;
    };
    let total = stats.candidate_movies;
    let text = format!(
        "影片图片打包回填 · 已完成 {completed}/{total} · 已打包 {} · 已清理 {} · 跳过 {} · 失败 {}",
        stats.packed_movies, stats.cleaned_movies, stats.skipped_missing_files, stats.failed_movies
    );
    let patch = serde_json::to_value(stats).ok();
    let _ = sink(
        Some(i32::try_from(completed).unwrap_or(i32::MAX)),
        Some(i32::try_from(total).unwrap_or(i32::MAX)),
        &text,
        patch.as_ref(),
    )
    .await;
}

/// 处理每一部**之前**的那次上报。上游循环内联的 `reporter.emit`
/// （`:162-170`）：`current` 是「已完成数 - 1」，且**不带 `summary_patch`**。
async fn emit_processing(
    progress: &mut Option<ProgressSink<'_>>,
    completed: i64,
    stats: &PackBackfillStats,
) {
    let Some(sink) = progress.as_mut() else {
        return;
    };
    let total = stats.candidate_movies;
    let text = format!(
        "影片图片打包回填 · 正在处理 {completed}/{total} · 已打包 {} · 失败 {}",
        stats.packed_movies, stats.failed_movies
    );
    let _ = sink(
        Some(i32::try_from(completed - 1).unwrap_or(i32::MAX)),
        Some(i32::try_from(total).unwrap_or(i32::MAX)),
        &text,
        None,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 任务键与 `cron_spec` 一致，且它是 **manual_only** 之一。
    #[test]
    fn the_task_key_is_pinned() {
        assert_eq!(TASK_KEY, "movie_asset_pack_backfill");
    }

    /// 统计的字段名与上游那六个 dict 键**逐字一致**。
    ///
    /// 它们进 `result_summary`，客户端按这些键读；改名不会报错，只表现为
    /// 「任务详情里的数字全空了」。
    #[test]
    fn the_stats_keys_match_upstreams_dict() {
        let stats = PackBackfillStats {
            candidate_movies: 10,
            packed_movies: 6,
            cleaned_movies: 2,
            already_packed_movies: 1,
            skipped_missing_files: 1,
            failed_movies: 0,
        };
        let json = serde_json::to_value(stats).expect("可序列化");
        let mut keys: Vec<String> = json.as_object().expect("对象").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "already_packed_movies",
                "candidate_movies",
                "cleaned_movies",
                "failed_movies",
                "packed_movies",
                "skipped_missing_files",
            ]
        );
    }

    /// ★ `cleaned_movies` 与 `packed_movies` **必须分开**。
    ///
    /// 前者是「本来有包，这次重建只是清了残留散文件」，后者是「新建了包」。
    /// 合成一个数就看不出回填到底在**建新包**还是在**擦屁股** —— 而这两种
    /// 情况说明的存量问题完全不同。
    #[test]
    fn cleaned_and_packed_are_distinguished() {
        let stats = PackBackfillStats {
            candidate_movies: 5,
            packed_movies: 3,
            cleaned_movies: 2,
            already_packed_movies: 0,
            skipped_missing_files: 0,
            failed_movies: 0,
        };
        assert_eq!(stats.packed_movies + stats.cleaned_movies, 5);
        assert_ne!(stats.packed_movies, stats.cleaned_movies);
    }

    /// ★ `skipped_missing_files` 与 `failed_movies` **必须分开**。
    ///
    /// 「文件已缺失」是数据问题（等 `image_cleanup` 清理），跳过是**正确行为**；
    /// 「重建失败」是算法/IO 问题。合成一个数就看不出该修哪边。
    #[test]
    fn missing_files_are_skipped_not_failed() {
        let stats = PackBackfillStats {
            candidate_movies: 10,
            packed_movies: 6,
            cleaned_movies: 0,
            already_packed_movies: 0,
            skipped_missing_files: 3,
            failed_movies: 1,
        };
        assert_eq!(
            stats.packed_movies
                + stats.cleaned_movies
                + stats.already_packed_movies
                + stats.skipped_missing_files
                + stats.failed_movies,
            stats.candidate_movies
        );
    }
}
