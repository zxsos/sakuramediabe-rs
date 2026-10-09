//! 宿主侧媒体探测原语 —— 替代 **PyAV** 在宿主进程里的用法。
//!
//! # 为什么是 CLI 而不是 ffmpeg 绑定
//!
//! 上游 `playback/media_metadata_probe_service.py` 用 `import av`（PyAV，即
//! FFmpeg 的 Python 绑定）。Rust 侧的等价物有两条路：
//!
//! | 路线 | 代价 |
//! |---|---|
//! | 链接 `ffmpeg-sys-next` 一类绑定 | 构建时要 FFmpeg 头文件与静态库；产物与我们的 musl 静态目标、跨平台部署都要重新论证一遍 |
//! | **调 `ffprobe` CLI**（本模块） | 运行期需要一个可执行的 `ffprobe` |
//!
//! 选 CLI：宿主**本来就依赖**宿主机上的外部程序（`ffmpeg` 用于切片，
//! 见 `media_clip_ffmpeg_timeout_seconds`），部署里已经是一个要交代的前提。
//! 而链接绑定会把「部署有没有」变成「构建能不能过」——后者更难排查，也更难回退。
//!
//! # 与上游的两处已知差异
//!
//! 1. **超时**：上游 `av.open()` 没有显式超时。这里给 `PROBE_TIMEOUT`（见下）——
//!    探测本就要读容器头，一个卡死的 NFS 挂载不该让整轮回填停在那儿。
//!    取 60 秒：远大于任何真实容器的头部读取，又短于一次任务的容忍度。
//! 2. ⚠️ **`probe_reader` 会先落盘**：上游能直接把 file-like（含远端 Range
//!    reader）喂给 `av.open()`。`ffprobe` 读管道（`pipe:0`）时**不能 seek**，
//!    而 mp4 的 `moov` 常在文件尾、mkv 的 cue 也可能在尾 —— 走管道会**假失败**。
//!    所以这里把 reader 落到临时文件再探，代价是一次额外的磁盘写。
//!    真按流式做要等「宿主能拿到远端 Range 读」那条路（指 `PlaybackPlan`），
//!    到那时这里应该改成读**宿主自己**已经持有的字节。
//!
//! # 探测不到与探测失败是两件事
//!
//! `duration_seconds` 默认 **0**（上游 dataclass 的默认值就是 0），调用方判
//! 「有没有探测到」要用 `> 0`。而 [`ProbeError`] 只用于「连容器都打不开」——
//! 那种情况下调用方给的是空结果（与「pyav 没装」同一档），不是失败。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::NaiveDateTime;

/// 探测超时。理由见模块文档「与上游的两处已知差异」。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// 指定 `ffprobe` 可执行文件的配置键（环境变量）。
///
/// 与隔壁 `svc-hash` / `svc-image` 不同 —— 那两个是**纯算法**，这个是外部署
/// 依赖，所以必须有「用哪一个」的出口（部署里 ffprobe 可能不在 PATH 上）。
pub const FFPROBE_ENV: &str = "SAKURAMEDIA_FFPROBE";

/// 探测结果。
///
/// 字段与上游 `MediaMetadataProbeResult` 的四个一一对应（见
/// `sm_service::playback::media_metadata_probe`，那边负责转成本仓的类型）。
#[derive(Debug, Clone, PartialEq)]
pub struct VideoProbe {
    /// 分辨率，形如 `1920x1080`。`None` = 没有视频流或没读到尺寸。
    pub resolution: Option<String>,
    /// 时长（秒）。**`0` = 探测不到**（不是 `None`）。
    pub duration_seconds: i64,
    /// 完整探测结果（JSONB 透传，**不解释**）。上游把它整份存进 `media.video_info`。
    pub video_info: Option<serde_json::Value>,
    /// 创建时间。`None` = 容器里没有这个标签。
    pub creation_time: Option<NaiveDateTime>,
}

/// 探测失败。**只覆盖「打不开/解析不了」**，不覆盖「探测不到」。
#[derive(Debug)]
pub enum ProbeError {
    /// 找不到可执行的 `ffprobe`。
    BackendMissing,
    /// `ffprobe` 退出码非 0。
    Command { status: Option<i32>, stderr: String },
    /// `ffprobe` 被超时掐断。
    Timeout,
    /// `ffprobe` 的输出不是合法 JSON。
    BadJson { detail: String },
    /// 本地 I/O（含给 `probe_reader` 落盘那一步）。
    Io(std::io::Error),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BackendMissing => write!(f, "找不到 ffprobe（{FFPROBE_ENV} 或 PATH）"),
            Self::Command { status, stderr } => {
                write!(f, "ffprobe 退出码 {status:?}：{stderr}")
            }
            Self::Timeout => write!(f, "ffprobe 超时（{}s）", PROBE_TIMEOUT.as_secs()),
            Self::BadJson { detail } => write!(f, "ffprobe 输出不是 JSON：{detail}"),
            Self::Io(error) => write!(f, "探测的本地 I/O 失败：{error}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// 用哪个可执行文件。`explicit` 为 `None` 时看 [`FFPROBE_ENV`]，再退到 `PATH`。
///
/// ⚠️ 返回值**不代表文件存在**：给的是「该试哪个」。真要确认可用性调
/// [`backend_available`]（它会真的探测一次 `-version`）。
pub fn resolve_program(explicit: Option<&str>) -> PathBuf {
    if let Some(path) = explicit.filter(|value| !value.trim().is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(configured) = std::env::var(FFPROBE_ENV) {
        if !configured.trim().is_empty() {
            return PathBuf::from(configured);
        }
    }
    // Windows 上要带 `.exe`，Unix 上不能带 —— 让 `Command` 自己解析扩展名。
    PathBuf::from("ffprobe")
}

/// 部署检查：**真的跑一次** `ffprobe -version`。
///
/// 不看「PATH 里有没有这个名字」——名字在而文件坏了、架构不对、缺动态库，
/// 都过得了「存在性检查」却在真探测时失败。这一条被
/// `sm_service::playback::media_metadata_probe` 用来在任务摘要里报
/// 「未安装媒体探测依赖」，报错了会误导排查方向。
pub async fn backend_available() -> bool {
    let program = resolve_program(None);
    match tokio::process::Command::new(&program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// 探测一个**本地文件**。
pub async fn probe_file(path: &Path) -> Result<VideoProbe, ProbeError> {
    probe_path_with(&resolve_program(None), path).await
}

/// 同上，但显式指定可执行文件（测试与部署显式配置都走这条）。
pub async fn probe_path_with(program: &Path, path: &Path) -> Result<VideoProbe, ProbeError> {
    let output = tokio::time::timeout(PROBE_TIMEOUT, async {
        tokio::process::Command::new(program)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .await
    })
    .await
    .map_err(|_| ProbeError::Timeout)?
    .map_err(ProbeError::Io)?;

    if !output.status.success() {
        return Err(ProbeError::Command {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    parse_probe_json(&String::from_utf8_lossy(&output.stdout))
}

/// 探测一个「可读的源」——先落盘再探。
///
/// ⚠️ 为什么不留 `S: Read` 直接喂管道：见模块文档第 2 条差异（**不能 seek**）。
pub async fn probe_reader<R: std::io::Read + Send + 'static>(
    source: R,
    source_label: &str,
) -> Result<VideoProbe, ProbeError> {
    let temp = temp_path(source_label);
    // 落盘在 `spawn_blocking` 里：读远端 reader 可能是慢 IO，别占着运行时线程。
    let path = temp.clone();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let mut file = std::fs::File::create(&path)?;
        let mut reader = std::io::BufReader::new(source);
        std::io::copy(&mut reader, &mut file)?;
        Ok(())
    })
    .await
    .map_err(|error| ProbeError::Io(std::io::Error::other(error)))?
    .map_err(ProbeError::Io)?;

    let probed = probe_file(&temp).await;
    // 临时文件无论成败都要清掉：一回填几千个文件，留着就是磁盘泄漏。
    let _ = tokio::fs::remove_file(&temp).await;
    probed
}

/// 临时文件路径。名字里带进程号与调用方给的标签，便于出问题时认领。
fn temp_path(source_label: &str) -> PathBuf {
    let sanitized: String = source_label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .take(32)
        .collect();
    std::env::temp_dir().join(format!("svc-probe-{}-{sanitized}.bin", std::process::id()))
}

/// 解析 `ffprobe -print_format json` 的输出。**纯函数**（好测，不用装 ffprobe）。
pub fn parse_probe_json(raw: &str) -> Result<VideoProbe, ProbeError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|error| ProbeError::BadJson {
            detail: error.to_string(),
        })?;

    let streams = value
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 第一个视频流。音频/字幕流没有尺寸，拿它们会得到空分辨率。
    let video = streams.iter().find(|stream| {
        stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
    });

    let resolution = video.and_then(|stream| {
        let width = stream.get("width").and_then(serde_json::Value::as_i64)?;
        let height = stream.get("height").and_then(serde_json::Value::as_i64)?;
        // 0 是「ffprobe 没读到」，不是一帧 0×0。
        (width > 0 && height > 0).then(|| format!("{width}x{height}"))
    });

    // 时长：`format.duration` 是整段，优先；没有再看视频流自己的。
    // 两边都是**字符串**形式的浮点数（ffprobe 的 JSON 就这么给）。
    let duration_seconds = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .or_else(|| video.and_then(|stream| stream.get("duration")))
        .and_then(json_duration);

    // 创建时间：容器级标签优先，再退到视频流的标签。
    let creation_time = value
        .get("format")
        .and_then(|format| format.get("tags"))
        .or_else(|| video.and_then(|stream| stream.get("tags")))
        .and_then(|tags| tags.get("creation_time"))
        .and_then(serde_json::Value::as_str)
        .and_then(parse_creation_time);

    Ok(VideoProbe {
        resolution,
        // `0` = 探测不到（上游 dataclass 的默认值）—— **不是** `None`。
        duration_seconds: duration_seconds.unwrap_or(0),
        // 整份透传 —— 上游把它当不解释的 JSONB 存进 `media.video_info`。
        video_info: Some(value),
        creation_time,
    })
}

/// ffprobe 的时长字段：字符串浮点（`"1234.567000"`），偶尔也直接给数字。
fn json_duration(value: &serde_json::Value) -> Option<i64> {
    let seconds = match value {
        serde_json::Value::String(text) => text.parse::<f64>().ok()?,
        serde_json::Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    // 向下取整：上游存的是整数秒，向上取整会让 1.5 秒的片段变成 2 秒，
    // 与「时长 > 实际」一类的前端校验冲突。
    Some(seconds.floor() as i64)
}

/// 容器里的 `creation_time` 标签。形如 `2020-01-02T03:04:05.000000Z`。
///
/// 试两种：带 `Z` 的 RFC3339、以及去掉 `Z` 的 naive 形式（有些封装器不写时区）。
fn parse_creation_time(raw: &str) -> Option<NaiveDateTime> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(parsed.naive_utc());
    }
    let without_zulu = trimmed.trim_end_matches('Z');
    if let Ok(parsed) = NaiveDateTime::parse_from_str(without_zulu, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(parsed);
    }
    // 有些容器只给日期（`2020-01-02`）—— 当作当天 00:00，别整份丢掉。
    chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份真实的 `ffprobe -print_format json` 输出（截掉与探测无关的字段）。
    const SAMPLE: &str = r#"{
        "streams": [
            {
                "index": 0,
                "codec_type": "video",
                "codec_name": "h264",
                "width": 1920,
                "height": 1080,
                "duration": "120.500000"
            },
            { "index": 1, "codec_type": "audio", "codec_name": "aac" }
        ],
        "format": {
            "filename": "movie.mkv",
            "duration": "120.500000",
            "tags": { "creation_time": "2020-01-02T03:04:05.000000Z" }
        }
    }"#;

    #[test]
    fn a_normal_probe_yields_resolution_duration_and_creation_time() {
        let probe = parse_probe_json(SAMPLE).expect("这份输出能解析");
        assert_eq!(probe.resolution.as_deref(), Some("1920x1080"));
        // 向下取整，不是四舍五入。
        assert_eq!(probe.duration_seconds, 120);
        assert_eq!(
            probe.creation_time,
            NaiveDateTime::parse_from_str("2020-01-02T03:04:05", "%Y-%m-%dT%H:%M:%S").ok()
        );
        // 整份透传：调用方要把它存进 `media.video_info`。
        assert!(probe.video_info.as_ref().is_some_and(|v| v.is_object()));
    }

    /// 音频流没有尺寸 —— 拿第一个流当视频流会得到空分辨率。
    #[test]
    fn the_resolution_comes_from_the_video_stream_not_the_first_stream() {
        let raw = r#"{
            "streams": [
                { "codec_type": "audio", "width": 0, "height": 0 },
                { "codec_type": "video", "width": 3840, "height": 2160 }
            ],
            "format": { "duration": "10.0" }
        }"#;
        let probe = parse_probe_json(raw).expect("能解析");
        assert_eq!(probe.resolution.as_deref(), Some("3840x2160"));
    }

    /// 没有视频流：分辨率是 `None`，但**时长照给** —— 音轨也有时长，
    /// 而「探测到了时长」已经值得落库。
    #[test]
    fn an_audio_only_file_still_reports_its_duration() {
        let raw = r#"{
            "streams": [{ "codec_type": "audio", "duration": "42.900000" }],
            "format": { "duration": "42.900000" }
        }"#;
        let probe = parse_probe_json(raw).expect("能解析");
        assert!(probe.resolution.is_none());
        assert_eq!(probe.duration_seconds, 42);
    }

    /// `duration` 是字符串浮点；0 / 负数 / 非数字都算「没探测到」，不是 0 秒。
    #[test]
    fn a_non_positive_or_unparsable_duration_is_not_a_zero_length_media() {
        for raw in [
            r#"{"streams": [], "format": {"duration": "0.000000"}}"#,
            r#"{"streams": [], "format": {"duration": "-1.0"}}"#,
            r#"{"streams": [], "format": {"duration": "N/A"}}"#,
            r#"{"streams": []}"#,
        ] {
            let probe = parse_probe_json(raw).expect("都能解析");
            assert_eq!(probe.duration_seconds, 0, "raw={raw}");
        }
    }

    /// 宽度为 0 是「没读到」，不是 0×0。
    #[test]
    fn a_zero_dimension_is_not_a_resolution() {
        let raw = r#"{"streams": [{"codec_type": "video", "width": 0, "height": 0}]}"#;
        assert!(parse_probe_json(raw).expect("能解析").resolution.is_none());
    }

    /// 不是 JSON 就是错误 —— 这是「命令输出被别的提示信息污染」的典型症状，
    /// 静默当成空结果会让回填把「探测失败」记成「探测不到」。
    #[test]
    fn a_non_json_output_is_an_error() {
        assert!(matches!(
            parse_probe_json("ffprobe version 7.1\n"),
            Err(ProbeError::BadJson { .. })
        ));
    }

    /// 只给日期的 `creation_time` 也认（有些封装器不写时间）。
    #[test]
    fn a_date_only_creation_time_is_midnight() {
        assert_eq!(
            parse_creation_time("2021-05-06"),
            chrono::NaiveDate::parse_from_str("2021-05-06", "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
        );
        assert_eq!(parse_creation_time("  "), None);
    }

    /// 显式配置优先于环境变量，环境变量优先于 PATH。
    #[test]
    fn the_program_resolution_prefers_explicit_then_env() {
        assert_eq!(
            resolve_program(Some("/opt/ffprobe")),
            PathBuf::from("/opt/ffprobe")
        );
        // 空串等于没给 —— 部署里把变量设成空是很常见的「取消设置」写法。
        assert_eq!(resolve_program(Some("  ")), PathBuf::from("ffprobe"));
        assert_eq!(resolve_program(None), PathBuf::from("ffprobe"));
    }
}
