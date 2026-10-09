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
//! `skip`。
//!
//! ⚠️ 报错会让这个任务在「插件没实现探测」时永远失败，而那是**合法状态**
//! —— 插件可能只做存储不做探测。
//!
//! # 回填两列：`duration_seconds` 与 `resolution`
//!
//! 还有 `video_info`（JSONB 透传的完整探测结果）。**分辨率不是从文件名的
//! 标签解析的** —— 那些标签（1080p / H264）来自发布方，可能与实际不符。

use crate::error::ServiceError;

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
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct VideoInfoBackfillStats {
    pub examined: i32,
    /// 成功回填的数量。
    pub updated: i32,
    /// ★ provider **不支持**探测而跳过的数量。**不是失败**。
    pub skipped_unsupported: i32,
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
/// 那行就永远不会被重试。
pub fn needs_backfill(duration_seconds: i64, resolution: Option<&str>) -> bool {
    duration_seconds <= 0 || resolution.map(str::trim).unwrap_or("").is_empty()
}

/// 视频信息回填服务。
pub struct MediaVideoInfoBackfillService;

impl MediaVideoInfoBackfillService {
    /// ★ 跑一轮。任务执行体。
    ///
    /// 单条失败不中断；provider 不支持探测时**整批跳过**（不是失败）。
    pub async fn backfill_missing_video_infos(
        &self,
    ) -> Result<VideoInfoBackfillStats, ServiceError> {
        todo!("骨架：查缺时长的媒体 -> 探测能力存在性(getattr 式) -> 逐个 probe_video_info -> 回填")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            skipped_unsupported: 2,
            failed: 0,
        };
        assert_eq!(stats.skipped_unsupported, 2);
        assert_eq!(stats.failed, 0, "不支持探测不算失败");
    }
}
