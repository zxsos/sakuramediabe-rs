//! 匿名遥测心跳（上游 `system/telemetry_service.py`，189 行）。
//!
//! # 它是什么
//!
//! 受环境变量 `SAKURAMEDIA_TELEMETRY_ENABLED` 控制（[`is_enabled`]，**默认开**），
//! 每小时一次把一份**匿名**载荷 POST 到一个外部端点
//! （Supabase functions，见 [`ENDPOINTS`]）。
//!
//! ```text
//! {schema_version, instance_id, backend_version, plugins[],
//!  platform, cpu_architecture, managed_media_file_count,
//!  managed_media_total_bytes, cpu_model?, memory_total_bytes?}
//! ```
//!
//! **没有 HTTP 路由，也不产生 `background_task_run`** —— 它不是队列任务，
//! 直接发一次 HTTP 就结束了（上游把它挂成 APScheduler 的 `interval` job，
//! `start/aps.py:365-374`，job id `telemetry_heartbeat`、`hours=1`、
//! `next_run_time=runtime_now()` 即**启动即跑一次**）。所以本仓的接线是
//! **组合根里一个独立循环**，不是塞进 [`crate::system`] 的任务队列。
//!
//! # 隐私取舍（env **默认开** = 默认向第三方上报）
//!
//! 这是照上游 (`:32-33`) 的选择：`os.getenv(..., "").strip().lower() != "false"`，
//! 也就是「只有显式设成 `false` 才关闭」。载荷里**没有**媒体内容、路径、库名 ——
//! 只有计数、字节量、CPU / 内存与后端版本这些部署画像。
//!
//! # 与上游的偏差（逐条登记，都是宿主模型决定的）
//!
//! 1. **`platform`**：上游是 `platform.system().lower()`（`"Darwin"` → `"darwin"`）；
//!    Rust 的 [`std::env::consts::OS`] 对 macOS 给的是 `"macos"`，所以
//!    [`platform_name`] 把 `"macos"` 映射回 `"darwin"`，其余原样透传。
//! 2. **`cpu_architecture`**：上游是 `platform.machine()`（取 `os.uname().machine`），
//!    这里是 [`std::env::consts::ARCH`]（编译目标架构）。Linux 上两者一致，
//!    Windows 上 Python 可能是 `"AMD64"` 而这里是 `"x86_64"` —— 按上游「不归一化」
//!    的口径，这条差异如实保留而不做猜测性对齐。
//! 3. **`cpu_model` / `memory_total_bytes`**：上游用平台分支探测（Linux
//!    `/proc/cpuinfo`、Darwin `sysctl`、Windows `winreg`）+ `psutil.virtual_memory()`；
//!    这里用 [`sysinfo`]（ADR `2026-10-04-tech-selection.md` 选定的替代品）。
//!    探测失败两个键**整体不携带**（上游 `:67-73`：它们是 v2 协议的可选增量）。
//! 4. **`plugins`**：上游 `PluginManager().list_plugins()` 扫 `root_dir` 下的**全部**
//!    插件目录（含未启用 / 加载失败的），版本取自 `manifest.json`。本仓还没有
//!    manifest 解析，所以只报**已成功启动**的插件、版本取注册响应（见
//!    `sm_server::plugins::Plugins::plugin_heartbeats`）。
//! 5. **[`TelemetryService::with_endpoints`] 只为测试存在**：上游把端点写成类常量，
//!    测试 monkeypatch `httpx.post`；Rust 侧留一个注入缝更直接。
//!
//! ⚠️ 骨架期本文件是一个**上游不存在的概念**（`TaskTelemetry` / `success_rate` /
//! `TelemetrySnapshot` —— 上游 grep `success_rate` / `task_stats` 零命中），
//! 已按「照上游重写成心跳」的取舍整体替换。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::System;

use crate::error::ServiceError;

/// 上报协议版本。上游 `_build_payload` 里写死的 `"schema_version": 2`。
pub const SCHEMA_VERSION: u32 = 2;

/// 匿名心跳的接收端点。**上游硬编码**（`telemetry_service.py:26-28`）。
pub const ENDPOINTS: &[&str] =
    &["https://pswhnebzlzdcdljzvrqa.supabase.co/functions/v1/telemetry/v1/heartbeats"];

/// 开关环境变量。上游 `TelemetryService.ENABLED_ENV_KEY`。
pub const ENABLED_ENV_KEY: &str = "SAKURAMEDIA_TELEMETRY_ENABLED";

/// CPU 型号的最大长度（**字符**，不是字节）。上游 `CPU_MODEL_MAX_LENGTH`。
pub const CPU_MODEL_MAX_LENGTH: usize = 128;

/// 实例状态文件名。与配置文件**同目录**（上游
/// `Path(Settings.model_config["toml_file"]).with_name("telemetry.json")`）。
pub const INSTANCE_STATE_FILE: &str = "telemetry.json";

/// 单次上报的超时。上游 `httpx.post(..., timeout=10.0)`。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 一条插件心跳项（上游 `{"id": plugin["plugin_id"], "version": plugin["version"]}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHeartbeat {
    pub id: String,
    pub version: String,
}

/// 心跳载荷。字段顺序与上游 `_build_payload` 的字典字面量一致。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TelemetryPayload {
    pub schema_version: u32,
    pub instance_id: String,
    pub backend_version: String,
    pub plugins: Vec<PluginHeartbeat>,
    pub platform: String,
    pub cpu_architecture: String,
    pub managed_media_file_count: i64,
    pub managed_media_total_bytes: i64,
    /// 硬件探测失败时**不携带**该键（上游 `:67-70`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// 同上（上游 `:71-73`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
}

/// 开关语义：**去空白 + 大小写不敏感，只有等于 `"false"` 才关闭**；未设置算开。
///
/// 上游 `is_enabled`（`:31-33`）：`os.getenv(key, "").strip().lower() != "false"`。
/// 所以 `""` / `"true"` / `"0"` / 任意其他值都是**开**。
pub fn enabled_from(raw: Option<&str>) -> bool {
    match raw {
        Some(value) => !value.trim().eq_ignore_ascii_case("false"),
        None => true,
    }
}

/// 读环境变量判定是否启用。
pub fn is_enabled() -> bool {
    enabled_from(std::env::var(ENABLED_ENV_KEY).ok().as_deref())
}

/// 把 [`std::env::consts::OS`] 的值映射成上游 `platform.system().lower()` 的取值。
///
/// 只有 macOS 需要映射（`"macos"` → `"darwin"`）；`"linux"` / `"windows"` 等
/// 本来就是小写、与上游一致。
pub fn platform_name(os: &'static str) -> &'static str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

/// 运行时平台（上游 `_runtime_platform`）。
pub fn runtime_platform() -> &'static str {
    platform_name(std::env::consts::OS)
}

/// CPU 架构（上游 `_cpu_architecture` = `platform.machine()`）。
pub fn cpu_architecture() -> &'static str {
    std::env::consts::ARCH
}

/// 折叠内部空白并截断到 [`CPU_MODEL_MAX_LENGTH`] 个字符；空串返回 `None`。
///
/// 上游 `_cpu_model`（`:76-84`）：`" ".join(candidate.split())[:CPU_MODEL_MAX_LENGTH]`
/// 之后再 `if model` 判空。
pub fn normalize_cpu_model(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed.chars().take(CPU_MODEL_MAX_LENGTH).collect())
}

/// 探测 CPU 型号与内存总量。任一失败返回 `None`（协议把两者当可选增量）。
///
/// 上游分别走平台分支 + `psutil`；这里统一交给 [`sysinfo`]。`0` 字节按「拿不到」
/// 处理（上游 `total or None`）。
fn hardware() -> (Option<String>, Option<u64>) {
    let system = System::new_all();
    let cpu_model = system
        .cpus()
        .first()
        .and_then(|cpu| normalize_cpu_model(cpu.brand()));
    let memory_total_bytes = match system.total_memory() {
        0 => None,
        bytes => Some(bytes),
    };
    (cpu_model, memory_total_bytes)
}

/// 实例 id 状态文件的位置：与配置文件同目录。
fn instance_state_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(INSTANCE_STATE_FILE)
}

/// 读取或创建实例 id。
///
/// 上游 `_load_or_create_instance_id`（`:176-188`）：能读到合法 UUID 就**规范化**
/// 返回（`str(uuid.UUID(...))`）；读不到 / 坏了 / 不是 UUID 就新建一个 v4 并写回
/// （先 `mkdir(parents=True)`）。**必须是幂等的** —— 每次心跳都换号会让外部统计
/// 把同一台实例算成很多台。
fn load_or_create_instance_id(state_path: &Path) -> Result<String, ServiceError> {
    if let Ok(text) = std::fs::read_to_string(state_path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(previously) = value.get("instance_id").and_then(|v| v.as_str()) {
                if let Ok(parsed) = uuid::Uuid::parse_str(previously) {
                    return Ok(parsed.to_string());
                }
            }
        }
    }
    let instance_id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = state_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(instance_io_error)?;
    }
    let body = format!("{}\n", serde_json::json!({ "instance_id": instance_id }));
    std::fs::write(state_path, body).map_err(instance_io_error)?;
    Ok(instance_id)
}

/// 状态文件读写的错误映射。遥测没有 HTTP 面，这个 code 只进日志 ——
/// 复用与 `From<DbError>` 相同的 `internal_error`，不新造契约码。
fn instance_io_error(error: std::io::Error) -> ServiceError {
    ServiceError::from_status(
        500,
        "internal_error",
        format!("遥测实例文件读写失败：{error}"),
    )
}

/// 遥测服务。
pub struct TelemetryService {
    db: sm_db::Db,
    state_path: PathBuf,
    endpoints: Vec<String>,
}

impl TelemetryService {
    /// 构造：绑定数据库（查受管媒体计数）与配置文件路径（定位实例文件）。
    pub fn new(db: sm_db::Db, config_path: &Path) -> Self {
        Self {
            db,
            state_path: instance_state_path(config_path),
            endpoints: ENDPOINTS.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    /// 覆盖上报端点。**只为测试**（见模块文档第 5 条偏差）。
    pub fn with_endpoints(mut self, endpoints: Vec<String>) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// 实例状态文件的位置（诊断 / 测试断言用）。
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// 组装一次载荷。DB / 文件失败返回 `Err`（HTTP 失败不在这里）。
    pub async fn build_payload(
        &self,
        plugins: Vec<PluginHeartbeat>,
    ) -> Result<TelemetryPayload, ServiceError> {
        let repo = sm_db::repo::MediaRepository::new(self.db.clone());
        let (managed_media_file_count, managed_media_total_bytes) =
            repo.valid_media_metrics().await?;
        let (cpu_model, memory_total_bytes) = hardware();
        Ok(TelemetryPayload {
            schema_version: SCHEMA_VERSION,
            instance_id: load_or_create_instance_id(&self.state_path)?,
            backend_version: crate::system::status::resolve_backend_version(),
            plugins,
            platform: runtime_platform().to_owned(),
            cpu_architecture: cpu_architecture().to_owned(),
            managed_media_file_count,
            managed_media_total_bytes,
            cpu_model,
            memory_total_bytes,
        })
    }

    /// 上报一次。
    ///
    /// * 开关关闭 → **直接返回**，不组装载荷、**不创建实例文件**
    ///   （上游 `report` 的第一句 + `test_disabled_report_does_not_send_or_create_instance_id`）。
    /// * 每个端点独立发；HTTP 失败**只记 warning，不向上传播**（上游 `:41-46`
    ///   只捕获 `httpx.HTTPError`）。DB / 文件失败才返回 `Err`，由调用方记日志。
    pub async fn report(&self, plugins: Vec<PluginHeartbeat>) -> Result<(), ServiceError> {
        if !is_enabled() {
            return Ok(());
        }
        let payload = self.build_payload(plugins).await?;
        let client = reqwest::Client::new();
        for endpoint in &self.endpoints {
            match client
                .post(endpoint)
                .json(&payload)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
            {
                Ok(response) => {
                    if let Err(error) = response.error_for_status() {
                        tracing::warn!(
                            endpoint = endpoint.as_str(),
                            error = %error,
                            "遥测心跳收到非成功响应"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        endpoint = endpoint.as_str(),
                        error = %error,
                        "遥测心跳请求失败"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开关：只有显式 `"false"`（去空白 + 大小写不敏感）才关闭。
    #[test]
    fn telemetry_is_on_unless_explicitly_false() {
        assert!(enabled_from(None), "未设置 = 开");
        assert!(enabled_from(Some("")), "空串 = 开");
        assert!(enabled_from(Some("true")));
        assert!(enabled_from(Some("0")), "只有 false 才关，别的值都算开");
        assert!(!enabled_from(Some("false")));
        assert!(!enabled_from(Some("FALSE")), "大小写不敏感");
        assert!(!enabled_from(Some("  false  ")), "两侧空白要去掉");
    }

    /// macOS 的 OS 常量是 `"macos"`，上游报的是 `"darwin"`。
    #[test]
    fn macos_is_reported_as_darwin() {
        assert_eq!(platform_name("macos"), "darwin");
        assert_eq!(platform_name("linux"), "linux");
        assert_eq!(platform_name("windows"), "windows");
    }

    /// CPU 型号：折叠空白 + 按**字符**截断到 128 + 空串为 None。
    #[test]
    fn cpu_model_is_collapsed_and_truncated() {
        assert_eq!(
            normalize_cpu_model("AMD   Ryzen 7\n5800X"),
            Some("AMD Ryzen 7 5800X".to_owned())
        );
        assert_eq!(normalize_cpu_model("   "), None);

        let long = "x".repeat(CPU_MODEL_MAX_LENGTH + 50);
        let normalized = normalize_cpu_model(&long).expect("非空");
        assert_eq!(normalized.chars().count(), CPU_MODEL_MAX_LENGTH);
    }

    /// 载荷形状：八个必填键都在、可选硬件键缺失时**不序列化**。
    #[test]
    fn optional_hardware_fields_are_omitted_when_absent() {
        let payload = TelemetryPayload {
            schema_version: SCHEMA_VERSION,
            instance_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            backend_version: "v0.5.3".to_owned(),
            plugins: vec![PluginHeartbeat {
                id: "local".to_owned(),
                version: "1.2.3".to_owned(),
            }],
            platform: "linux".to_owned(),
            cpu_architecture: "x86_64".to_owned(),
            managed_media_file_count: 3,
            managed_media_total_bytes: 900,
            cpu_model: None,
            memory_total_bytes: None,
        };
        let value = serde_json::to_value(&payload).unwrap();
        for key in [
            "schema_version",
            "instance_id",
            "backend_version",
            "plugins",
            "platform",
            "cpu_architecture",
            "managed_media_file_count",
            "managed_media_total_bytes",
        ] {
            assert!(value.get(key).is_some(), "缺必填键 {key}");
        }
        assert!(value.get("cpu_model").is_none(), "缺失的硬件键不应出现");
        assert!(value.get("memory_total_bytes").is_none());
        assert_eq!(value["schema_version"], serde_json::json!(2));
    }

    /// 实例 id 幂等：同一路径读两次得到同一个号，且**不重复写**。
    #[test]
    fn the_instance_id_is_created_once() {
        let dir = std::env::temp_dir().join(format!("sm-telemetry-{}", uuid::Uuid::new_v4()));
        let state_path = dir.join(INSTANCE_STATE_FILE);
        // 目录尚不存在也要能建出来（上游 mkdir(parents=True)）。
        let first = load_or_create_instance_id(&state_path).expect("首次创建");
        let second = load_or_create_instance_id(&state_path).expect("第二次读取");
        assert_eq!(first, second, "同一实例两次心跳必须同号");
        let body = std::fs::read_to_string(&state_path).unwrap();
        assert!(body.contains(&first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件（非法 UUID）会被重建，而不是把脏值当号用。
    #[test]
    fn a_corrupt_state_file_is_recreated() {
        let dir = std::env::temp_dir().join(format!("sm-telemetry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join(INSTANCE_STATE_FILE);
        std::fs::write(&state_path, "{\"instance_id\": \"not-a-uuid\"}").unwrap();
        let id = load_or_create_instance_id(&state_path).expect("重建");
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "重建出的是合法 UUID");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
