//! 媒体技术信息回填（上游 `playback/media_video_info_backfill_service.py`，233 行）。
//!
//! # 任务键 `media_video_info_backfill` —— **manual_only**
//!
//! `cron_spec::builtin_jobs` 里三条无 cron 的任务之一（另两条是
//! `movie_asset_pack_backfill` 与 `media_thumbnail_pack_backfill`）。
//!
//! 手动的原因：探测要跑 ffprobe，**慢且吃 CPU**。而「回填」是那种
//! 「想起来才做一次」的事（换了新插件、升级了 ffprobe）。
//!
//! # 探测是**可选能力**，缺失时**跳过**而不是报错
//!
//! 上游用 `getattr(storage, "probe_video_info", None)` 探测能力。缺失时
//! `skip`（`:185-188`，**逐条**判，不是开跑前判一次 —— 每个库的 provider
//! 可以不同）。
//!
//! ⚠️ 报错会让这个任务在「插件没实现探测」时永远失败，而那是**合法状态**
//! —— 插件可能只做存储不做探测。
//!
//! # 回填两列：`duration_seconds` 与 `resolution`
//!
//! 还有 `video_info`（JSONB 透传的完整探测结果）。**分辨率不是从文件名的
//! 标签解析的** —— 那些标签（1080p / H264）来自发布方，可能与实际不符。

use std::sync::Arc;

use sm_db::repo::MediaRepository;
use sm_db::Db;

use crate::catalog::movie_asset_pack_backfill::ProgressSink;
use crate::error::ServiceError;
use crate::playback::operation_locks::MediaOperation;
use crate::playback::provider_helpers::{
    json_or_null, media_handle_for, MediaRecord, StorageGateway, PROVIDER_UNSUPPORTED,
};

/// 任务键。
pub const TASK_KEY: &str = "media_video_info_backfill";

/// 探测结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoInfo {
    /// 时长（秒）。`0` = 探测不到。
    pub duration_seconds: i64,
    /// 分辨率，形如 `1920x1080`。`None` = 探测不到。
    pub resolution: Option<String>,
    /// 完整探测结果（JSONB 透传，不解释）。
    pub video_info: Option<serde_json::Value>,
}

/// 回填统计。
///
/// 字段与上游 stats 字典逐键对齐（`:114-120`）：`examined` = `missing_media`、
/// `updated` = `updated_media`、`skipped_media` = `skipped_media`（锁被占 /
/// 重读时已不缺 / provider 探了个寂寞）、`incomplete_media` = `incomplete_media`
/// （写进去了但**仍有缺**）、`failed` = `failed_media`。
/// 多出的 [`VideoInfoBackfillStats::skipped_unsupported`] 是上游
/// `getattr` 探测能力缺失那一档 —— 上游把它混进 `skipped_media`，
/// 单列的理由与哈希回填的 `invalid_hash` 同款：**「插件不做」是合法状态，
/// 「插件坏了」要修**，混在一起没法排障。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct VideoInfoBackfillStats {
    pub examined: i32,
    /// 成功回填的数量。
    pub updated: i32,
    /// 锁被占 / 重读时已不缺 / provider 探了个寂寞。
    pub skipped_media: i32,
    /// ★ provider **不支持**探测而跳过的数量。**不是失败**。
    pub skipped_unsupported: i32,
    /// 写进去了但仍有缺（探测结果本身不全）。
    pub incomplete_media: i32,
    /// 探测失败的数量。
    pub failed: i32,
}

/// 是否需要回填。**纯函数**。
///
/// 判据是「**缺什么**」，不是「有没探测过」：
///
/// | 情况 | 需要回填 |
/// |---|---|
/// | `duration_seconds = 0` 且 无 `resolution` | 是 |
/// | 只有 `duration_seconds = 0` | 是（分辨率也要） |
/// | 只有 `resolution` 为空 | 是（时长也要） |
/// | 两样都有 | 否 |
///
/// ⚠️ 别用「`video_info IS NULL`」当判据 —— 一次失败的探测会写入空 JSON，
/// 那行就永远不会被重试。（候选 SQL 里它**是**三段条件之一，那是上游
/// 「一行可以只缺完整探测结果」的语义；这里这个函数只回答
/// 「时长/分辨率还缺不缺」。）
pub fn needs_backfill(duration_seconds: i64, resolution: Option<&str>) -> bool {
    duration_seconds <= 0 || resolution.map(str::trim).unwrap_or("").is_empty()
}

/// 行还缺不缺（候选 SQL 的三段条件，供重读与 `incomplete` 判定复用）。
pub fn still_missing(
    video_info: Option<&str>,
    duration_seconds: i64,
    resolution: Option<&str>,
) -> bool {
    video_info.is_none() || needs_backfill(duration_seconds, resolution)
}

/// 归一化分辨率为 `WxH`。上游 `src/common/media_formats.py` 的
/// `normalize_media_resolution`（逐条对齐）：
///
/// - 输入截断在 32 字符；
/// - 小写、按 `x` 切成两段、两段都得是纯数字；
/// - 去掉前导零后**不能为空**（`000` 不算 0）；
/// - 每段 ≤ `i32::MAX`（按**字符串**比较，上游就是这么写的）。
///
/// 与上游的一处偏差：`str.isdigit()` 对 `'²'` 也返回 true（Unicode 数字），
/// 这里只认 ASCII 数字 —— 探测器不会产出那种东西，而 Rust 的
/// `is_ascii_digit` 语义更诚实。
pub fn normalize_media_resolution(value: &str) -> Option<String> {
    if value.len() > 32 {
        return None;
    }
    let lowered = value.trim().to_lowercase();
    let parts: Vec<&str> = lowered.split('x').collect();
    if parts.len() != 2 {
        return None;
    }
    let mut dims = Vec::with_capacity(2);
    for part in parts {
        if !part.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        let trimmed = part.trim_start_matches('0');
        if trimmed.is_empty() {
            return None;
        }
        // 字符串比较：与上游一致 —— 比转数字再比更早发现「位数超了」。
        if trimmed.len() > "2147483647".len()
            || (trimmed.len() == "2147483647".len() && trimmed > "2147483647")
        {
            return None;
        }
        dims.push(trimmed);
    }
    Some(format!("{}x{}", dims[0], dims[1]))
}

/// 探测字典的「叶子数」。上游 `_info_fields`（`:41-60`）：
/// 非空字符串、正数各算一片叶子；容器递归；空值/零/负数不算。
///
/// 用途：新的探测结果**比库里已有的更全**时才覆盖 `video_info`
/// （`_save_missing_info:77-78`）—— 旧结果可能是另一台机器探的，
/// 一台低配的 ffprobe 不该把一台高配的结果顶掉。
pub fn info_leaves(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => map.values().map(info_leaves).sum(),
        serde_json::Value::Array(items) => items.iter().map(info_leaves).sum(),
        serde_json::Value::String(text) => usize::from(!text.trim().is_empty()),
        serde_json::Value::Number(number) => usize::from(number.as_f64().unwrap_or(0.0) > 0.0),
        _ => 0,
    }
}

/// 库里的 `video_info`（`text` 列，可能是**脏文本**）的叶子数。
///
/// 能解析成 JSON 就按结构数；解析不了的**非空**文本算一片叶子 ——
/// 上游把 DB 值直接递进 `_info_fields`，非空字符串就是一片叶子，同语义。
pub fn info_leaves_of_stored(raw: Option<&str>) -> usize {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(text)
        .map(|value| info_leaves(&value))
        .unwrap_or(1)
}

/// 从探测字典里取出「要写的时长 / 分辨率」。上游 `_save_missing_info:64-73`。
///
/// - 时长：`container.duration_seconds`，**整数且 > 0** 才要（上游
///   `type(duration) is int`，浮点不要 —— 浮点是「估算值」的标记）；
/// - 分辨率：`video.width x video.height`，经 [`normalize_media_resolution`]。
///
/// 返回 `(None, None)` 表示「这份结果里没有可用的时长/分辨率」，但
/// `video_info` 本身仍可能值得写。
pub fn planned_updates(info: &serde_json::Value) -> (Option<i64>, Option<String>) {
    let render = |value: Option<&serde_json::Value>| match value {
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        _ => None,
    };
    let duration = info
        .get("container")
        .and_then(|container| container.get("duration_seconds"))
        .and_then(serde_json::Value::as_i64);
    let resolution = info.get("video").and_then(|video| {
        let width = render(video.get("width"))?;
        let height = render(video.get("height"))?;
        normalize_media_resolution(&format!("{width}x{height}"))
    });
    (duration, resolution)
}

/// 视频信息回填服务。
///
/// # 依赖是注入的
///
/// 与 [`super::media_file_hash_backfill`] 同一取向：[`StorageGateway`] 是
/// trait，组合根注入真的 `ProviderGateway`，测试注入可编程的桩。
pub struct MediaVideoInfoBackfillService {
    db: Db,
    storage: Arc<dyn StorageGateway>,
    media: MediaRepository,
}

impl MediaVideoInfoBackfillService {
    /// 构造。
    pub fn new(db: &Db, storage: Arc<dyn StorageGateway>) -> Self {
        Self {
            db: db.clone(),
            storage,
            media: MediaRepository::new(db.clone()),
        }
    }

    /// 候选 media id。上游 `_candidate_ids`。
    pub async fn candidates(&self) -> Result<Vec<i32>, ServiceError> {
        Ok(self.media.list_missing_video_info_ids().await?)
    }

    /// ★ 跑一轮。任务执行体。上游 `backfill_missing_video_infos(cls, *, reporter)`。
    ///
    /// 单条失败不中断；provider 不支持探测时**逐条跳过**（不是失败）；
    /// 锁被占是跳过不是排队；每条处理前重读（候选快照会过期）——
    /// 与哈希回填同一套不变式，判据不同而已。
    pub async fn backfill_missing_video_infos(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<VideoInfoBackfillStats, ServiceError> {
        let media_ids = self.candidates().await?;
        let total = i32::try_from(media_ids.len()).unwrap_or(i32::MAX);
        let mut stats = VideoInfoBackfillStats {
            examined: total,
            updated: 0,
            skipped_media: 0,
            skipped_unsupported: 0,
            incomplete_media: 0,
            failed: 0,
        };

        for (index, media_id) in media_ids.iter().enumerate() {
            let completed = i32::try_from(index + 1).unwrap_or(i32::MAX);

            let lock = match MediaOperation::try_media(&self.db, *media_id).await {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    stats.skipped_media += 1;
                    Self::emit(progress.as_mut(), completed, total, &stats).await;
                    continue;
                }
                // 取锁失败是 DB 层的问题：传播出去让任务以失败结束。
                Err(error) => return Err(error),
            };

            let outcome = self.process_one(*media_id, &mut stats).await;
            // 锁在 `process_one` 之后、`stats` 记完之后释放。
            lock.release().await;

            if let Err(error) = outcome {
                stats.failed += 1;
                tracing::warn!(
                    media_id,
                    code = error.code(),
                    message = %error.api.message,
                    "媒体信息回填失败"
                );
            }
            Self::emit(progress.as_mut(), completed, total, &stats).await;
        }

        Ok(stats)
    }

    /// 处理一条候选。返回 `Err` 只表示「这一条没成」，调用方记 `failed` 后继续。
    async fn process_one(
        &self,
        media_id: i32,
        stats: &mut VideoInfoBackfillStats,
    ) -> Result<(), ServiceError> {
        // 重读 + 重验条件：候选快照可能已过期。
        let media = self.media.find_by_id(media_id).await?.filter(|media| {
            media.valid
                && still_missing(
                    media.video_info.as_deref(),
                    i64::from(media.duration_seconds),
                    media.resolution.as_deref(),
                )
        });
        let Some(media) = media else {
            stats.skipped_media += 1;
            return Ok(());
        };

        // handle 要带库的 provider 配置（与哈希回填同一套拼法）。
        let library = sm_db::repo::MediaLibraryRepository::new(self.media.pool().clone())
            .find_by_id(media.library_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_library_not_found",
                    "Media library not found",
                    "library_id",
                    media.library_id,
                )
            })?;
        let handle = media_handle_for(&MediaRecord {
            id: i64::from(media.id),
            library_id: i64::from(media.library_id),
            storage_ref: json_or_null(media.storage_ref.as_deref()),
            provider_config: json_or_null(library.provider_config.as_deref()),
            provider_key: library.provider_key.clone(),
            account_key: library.account_key.clone(),
            file_name: media.file_name.clone(),
            file_size_bytes: media.file_size_bytes,
            duration_seconds: media.duration_seconds,
        });

        let info = match self.storage.probe_video_info(&handle).await {
            Ok(info) => info,
            Err(failure) if failure.code == PROVIDER_UNSUPPORTED => {
                // 「插件不做探测」是合法状态：跳过，不算失败。
                stats.skipped_unsupported += 1;
                return Ok(());
            }
            Err(failure) => {
                stats.failed += 1;
                tracing::warn!(media_id = media.id, code = %failure.code, "provider 探测失败");
                return Ok(());
            }
        };

        // 空 / 非对象 = provider 没给出可用信息。上游是
        // `ValueError("provider returned no valid media video info")`（`:190-196`）。
        if !info.is_object() || info.as_object().is_some_and(|map| map.is_empty()) {
            stats.failed += 1;
            tracing::warn!(media_id = media.id, "provider 返回的探测结果不可用");
            return Ok(());
        }

        let (duration, resolution) = planned_updates(&info);
        let existing = media.video_info.as_deref();
        // 覆盖判据：还没有 → 写；有，但新的**更全** → 也写（见 `info_leaves`）。
        let write_info = existing.is_none() || info_leaves(&info) > info_leaves_of_stored(existing);
        let updated = self
            .media
            .save_missing_video_info(
                media.id,
                existing,
                write_info.then_some(&info),
                duration,
                resolution.as_deref(),
            )
            .await?;

        // 「写完还缺吗」要重读 —— 三列的条件化更新各自可能落空。
        let after = self.media.find_by_id(media.id).await?.ok_or_else(|| {
            ServiceError::not_found("media_not_found", "媒体不存在", "media_id", media.id)
        })?;
        let incomplete = still_missing(
            after.video_info.as_deref(),
            i64::from(after.duration_seconds),
            after.resolution.as_deref(),
        );
        if updated {
            stats.updated += 1;
            if incomplete {
                stats.incomplete_media += 1;
            }
        } else if incomplete {
            // 什么都没写成、却还缺着：探测结果没有可用的部分 —— 与上游同判
            // （`ValueError("provider returned no usable missing media info")`）。
            stats.failed += 1;
            tracing::warn!(
                media_id = media.id,
                "provider 的探测结果里没有可用的缺失字段"
            );
        } else {
            stats.skipped_media += 1;
        }
        Ok(())
    }

    /// 进度上报。上游 `emit_progress`（`:125-137`）的形状；sink 失败只 warn。
    async fn emit(
        progress: Option<&mut ProgressSink<'_>>,
        current: i32,
        total: i32,
        stats: &VideoInfoBackfillStats,
    ) {
        let Some(progress) = progress else {
            return;
        };
        let summary = serde_json::to_value(stats).ok();
        let text = format!(
            "媒体信息回填 · 已完成 {current}/{total} · 已更新 {} · 跳过 {} · 失败 {} · 仍缺 {}",
            stats.updated, stats.skipped_media, stats.failed, stats.incomplete_media
        );
        if let Err(error) = progress(Some(current), Some(total), &text, summary.as_ref()).await {
            tracing::warn!(error = %error, "媒体信息回填的进度上报失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ★ 判据是「缺什么」而非「探测过没有」。
    #[test]
    fn the_criterion_is_what_is_missing() {
        // 两样都缺
        assert!(needs_backfill(0, None));
        // 只缺时长
        assert!(needs_backfill(0, Some("1920x1080")));
        // 只缺分辨率
        assert!(needs_backfill(120, None));
        // 空白字符串**等于**缺
        assert!(needs_backfill(120, Some("   ")));
        // 两样都有 -> 不需要
        assert!(!needs_backfill(120, Some("1920x1080")));
    }

    /// `duration_seconds <= 0` 判为缺 —— 负数是脏数据，同样要重算。
    #[test]
    fn a_negative_duration_counts_as_missing() {
        assert!(needs_backfill(-1, Some("1920x1080")));
    }

    /// `skipped_unsupported` 与 `failed` 分开 —— 插件不做探测是**合法状态**。
    #[test]
    fn unsupported_is_skipped_not_failed() {
        let stats = VideoInfoBackfillStats {
            examined: 5,
            updated: 3,
            skipped_media: 0,
            skipped_unsupported: 2,
            incomplete_media: 0,
            failed: 0,
        };
        assert_eq!(stats.skipped_unsupported, 2);
        assert_eq!(stats.failed, 0, "不支持探测不算失败");
    }

    /// 归一化：大小写、空白、前导零都收；空段、超限、非数字都拒。
    #[test]
    fn normalization_follows_upstream_media_formats() {
        assert_eq!(
            normalize_media_resolution(" 1920X1080 "),
            Some("1920x1080".to_owned()),
            "小写化 + 去空白"
        );
        assert_eq!(
            normalize_media_resolution("0001x0001"),
            Some("1x1".to_owned()),
            "前导零剥掉，但不留空段"
        );
        assert_eq!(normalize_media_resolution("0x1080"), None, "空段不算 0");
        assert_eq!(normalize_media_resolution("1920"), None, "不是两段");
        assert_eq!(normalize_media_resolution("19x2 0"), None, "非数字");
        assert_eq!(
            normalize_media_resolution("99999999999x1"),
            None,
            "超 i32::MAX"
        );
        // &str 的 len() 是字节数；上游是字符数 —— 对纯 ASCII 场景等价。
        assert_eq!(
            normalize_media_resolution(&"1".repeat(33)),
            None,
            "输入超长"
        );
    }

    /// 叶子数：空值 / 零 / 负数不算信息 —— 这是「更全才覆盖」判据的地基。
    #[test]
    fn info_leaves_counts_only_informative_leaves() {
        assert_eq!(info_leaves(&json!({})), 0);
        assert_eq!(info_leaves(&json!({"a": null})), 0, "null 不是信息");
        assert_eq!(info_leaves(&json!({"a": 0})), 0, "零不是信息");
        assert_eq!(info_leaves(&json!({"a": ""})), 0, "空白不是信息");
        assert_eq!(info_leaves(&json!({"a": 1, "b": "x"})), 2);
        assert_eq!(
            info_leaves(
                &json!({"container": {"duration_seconds": 120}, "video": {"width": 1920, "height": 1080}})
            ),
            3
        );
        // 脏文本：库里可能存着一段不可解析的字符串，它也是「一段信息」。
        assert_eq!(info_leaves_of_stored(Some("not-json{")), 1);
        assert_eq!(
            info_leaves_of_stored(Some("{\"video\":{\"width\":1920,\"height\":1080}}")),
            2
        );
        assert_eq!(info_leaves_of_stored(Some("   ")), 0);
        assert_eq!(info_leaves_of_stored(None), 0);
    }

    /// 取值段：`container.duration_seconds` 要**整数且 > 0**；分辨率由
    /// `video.width/height` 拼出来再归一化。
    #[test]
    fn planned_updates_extracts_duration_and_resolution() {
        let full = json!({
            "container": {"duration_seconds": 120},
            "video": {"width": 1920, "height": 1080}
        });
        assert_eq!(
            planned_updates(&full),
            (Some(120), Some("1920x1080".to_owned()))
        );

        // 浮点时长不要（上游 `type(duration) is int`）。
        let float_duration = json!({"container": {"duration_seconds": 120.5}});
        assert_eq!(planned_updates(&float_duration), (None, None));

        // 分辨率字段缺失 → 拼不出 → None，但时长照取。
        let no_resolution = json!({"container": {"duration_seconds": 30}, "video": {}});
        assert_eq!(planned_updates(&no_resolution), (Some(30), None));

        // 完全没有可用的字段。
        assert_eq!(planned_updates(&json!({"foo": 1})), (None, None));
    }

    /// `still_missing` 与候选 SQL 的三段条件一致：`video_info IS NULL` 也算缺。
    #[test]
    fn still_missing_includes_a_missing_video_info_column() {
        assert!(still_missing(None, 120, Some("1920x1080")));
        assert!(!still_missing(Some("{}"), 120, Some("1920x1080")));
        assert!(still_missing(Some("{}"), 0, Some("1920x1080")));
    }
}
