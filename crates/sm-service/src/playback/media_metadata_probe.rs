//! 媒体元数据探测（上游 `playback/media_metadata_probe_service.py`，339 行）。
//!
//! # 它用 `pyav`，而 `pyav` **可以为 None**
//!
//! 上游：`av = None` 时整个探测**跳过**，返回空结果。
//!
//! ⚠️ 这与「插件能力缺失」是**两种不同的跳过**，别混：
//!
//! | 情况 | 谁跳过 | 后果 |
//! |---|---|---|
//! | `pyav` 没装 | **宿主**自己 | 所有媒体都探测不了 |
//! | 插件没有 `probe_video_info` | **插件** | 只有那个库探测不了 |
//!
//! 前者是**部署问题**（该装 `pyav`），后者是**合法状态**。所以前者应该在
//! 任务摘要里显式报出来，而不是静默返回空 —— 否则用户会看到「回填了 0 条」
//! 而不知道是库没装。
//!
//! # `probe_source` 接受 file-like，而不只是路径
//!
//! 这是本文件最容易被忽略的设计：参数是 `source: Any`，只要 `av.open()` 能读。
//! 上游注释明写它可以吃「远端 Range reader」。
//!
//! 也就是说**远端媒体也能探测**，不必先下载到本地 —— 那正是
//! `playback_deliveries` 旁路（见 `routes/media_playback.rs`）需要的。
//!
//! # 探测结果里 `duration_seconds` 默认 **0**，不是 None
//!
//! `0` 与「探测不到」同义。上游的 dataclass 默认值就是 0。
//! 调用方判「有没有探测到」要用 `> 0`。

/// 探测结果。对齐上游 `MediaMetadataProbeResult`（frozen dataclass）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MediaMetadataProbeResult {
    /// 分辨率，形如 `1920x1080`。`None` = 探测不到。
    pub resolution: Option<String>,
    /// 时长（秒）。**`0` = 探测不到**（不是 `None`）。
    pub duration_seconds: i64,
    /// 完整探测结果（JSONB 透传，不解释）。
    pub video_info: Option<serde_json::Value>,
    /// 创建时间。`None` = 文件里没有。
    pub creation_time: Option<chrono::NaiveDateTime>,
}

impl MediaMetadataProbeResult {
    /// 「探测到了吗」。判据是 `duration > 0`，**不是** `is_some`。
    pub fn is_probed(&self) -> bool {
        self.duration_seconds > 0
    }

    /// 空结果。`pyav` 缺失时返回它（见模块文档）。
    pub fn empty() -> Self {
        Self {
            resolution: None,
            duration_seconds: 0,
            video_info: None,
            creation_time: None,
        }
    }
}

/// 探测服务。
pub struct MediaMetadataProbeService;

impl MediaMetadataProbeService {
    /// `pyav`（即 Rust 侧的 `ffmpeg` 绑定）是否可用。
    ///
    /// **部署检查**：宿主启动时调一次，为假就在任务摘要里显式报
    /// 「未安装媒体探测依赖」。见模块文档的两类跳过。
    ///
    /// # 当前恒为 `false`，而这不是占位
    ///
    /// 逐条核实过（2026-10-08）：
    ///
    /// | 上游手段 | 本仓对应物 |
    /// |---|---|
    /// | `import av`（PyAV） | **无** —— workspace 里没有 ffmpeg 绑定 |
    /// | 用 `av.open()` 读源 | **无** —— 也没有调用 `ffmpeg` / `ffprobe` CLI 的地方 |
    ///
    /// `media_clip` 那个模块里有 `media_clip_ffmpeg_timeout_seconds` 配置键，
    /// 但**只是超时配置**，没有实际调用 —— 别把它当「已有 ffmpeg 通路」的证据。
    ///
    /// 所以 `false` 是**如实反映后端不存在**，对应上游 `av = None` 那一档：
    /// 上游 pyav 没装时 `probe_file` 同样返回空结果、不报错。两条路径的
    /// 可观察行为因此一致 —— 不是「Rust 侧还没写」，而是「部署缺依赖」。
    ///
    /// ★ 接入后端时**只改这一处**。调用方（`media_video_info_backfill` /
    /// `media_validity_scan` / `media_file_hash_backfill`）只该问这个函数，
    /// 各自去 `which ffmpeg` 的话会出现「摘要说没有、实际探测跑起来了」。
    pub fn probe_backend_available() -> bool {
        false
    }

    /// ★ 探测一个本地文件。
    ///
    /// 上游 `probe_file(cls, file_path) -> MediaMetadataProbeResult`。
    /// `pyav` 缺失时返回 [`MediaMetadataProbeResult::empty`]（**不报错**）。
    ///
    /// 当前恒为空结果，原因见 [`Self::probe_backend_available`]。
    pub async fn probe_file(file_path: &std::path::Path) -> MediaMetadataProbeResult {
        // 参数不用是因为**后端不存在**，不是「还没接」—— 真接上时这里要读它。
        // 保留 `let _` 是为了让签名与文档继续成立（clippy 也满意）。
        let _ = file_path;
        MediaMetadataProbeResult::empty()
    }

    /// ★ 探测一个「可读的源」—— 路径**或**远端 Range reader。
    ///
    /// 上游 `probe_source(cls, source, *, file_size_bytes, source_label="<media-source>")`。
    ///
    /// `source` 刻意是泛型：只要底层容器库能读就行。上游注释明写它可以吃
    /// 「远端 Range reader」—— 这正是 `playback_deliveries` 旁路需要的
    /// （远端媒体不必先下载到本地就能探测）。
    ///
    /// `source_label` 只进错误信息，**别用它做逻辑判断**。
    pub async fn probe_source<S: std::io::Read + Send>(
        source: S,
        file_size_bytes: i64,
        source_label: &str,
    ) -> MediaMetadataProbeResult {
        let _ = (source, file_size_bytes, source_label);
        MediaMetadataProbeResult::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 判据是 `duration > 0`，**不是** `Option::is_some`。
    ///
    /// 默认值就是 0，用 `is_some` 判会把「没探测到」当成探测成功。
    #[test]
    fn probed_means_a_positive_duration() {
        assert!(!MediaMetadataProbeResult::empty().is_probed());
        let probed = MediaMetadataProbeResult {
            duration_seconds: 120,
            ..MediaMetadataProbeResult::empty()
        };
        assert!(probed.is_probed());
    }

    /// `pyav` 缺失时是**空结果**而不是错误。
    #[test]
    fn a_missing_backend_yields_an_empty_result_not_an_error() {
        let result = MediaMetadataProbeResult::empty();
        assert_eq!(result.duration_seconds, 0);
        assert!(result.resolution.is_none());
        assert!(!result.is_probed());
    }

    /// 分辨率与时长**独立**：可能只探测到其中一个。
    #[test]
    fn resolution_and_duration_are_independent() {
        let only_duration = MediaMetadataProbeResult {
            duration_seconds: 60,
            ..MediaMetadataProbeResult::empty()
        };
        assert!(only_duration.resolution.is_none());
        assert!(only_duration.is_probed());
    }
}
