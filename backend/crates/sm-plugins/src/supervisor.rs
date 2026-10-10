//! 插件进程的生命周期：拉起、等就绪、发现崩溃。
//!
//! # 上游对应：没有
//!
//! 上游是**进程内 import**（`src/plugins/loader.py` 用 `importlib`），根本没有
//! 子进程这件事。本模块依据的是 `docs/adr/2026-10-05-plugin-lifecycle.md` 里那套
//! 候选协议 —— 它是本仓库为了「插件拆进程」新定的，没有上游可对照：
//!
//! 1. 宿主 bind `127.0.0.1:0` 拿到端口，**放开**后交给子进程 bind；
//! 2. 地址与插件 id 走环境变量 `SAKURAMEDIA_PLUGIN_GRPC_ADDR` /
//!    `SAKURAMEDIA_PLUGIN_ID` 注入；
//! 3. 宿主以 `Register` 探活，探活用的就是 [`crate::loader::register`] ——
//!    顺带把「回显 / ABI / 能力」三条契约一起验了；
//! 4. 子进程退出 = 崩溃，由 [`PluginProcess::wait`] 发现。
//!
//! # 看门狗循环**不**在这里
//!
//! 重启要连带重建三张注册表（provider / 任务 / 扩展点），那是组合根的事，不在
//! 本模块的职责里。这里只给齐零件：拉起（[`launch`]）、发现崩溃
//! （[`PluginProcess::wait`]）、退避（[`restart_backoff`]）。
//!
//! # 探活失败分两种，处置不同
//!
//! - 连不上 / 调用失败 → **重试**，插件可能还在 listen；
//! - 声明不合契约（[`crate::registration::RegistrationProblem`]）→ **立刻失败**，
//!   重试只会得到同样的答案。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::str::FromStr;
use std::time::Duration;

use sm_plugin_api::v1::plugin_control_client::PluginControlClient;
use sm_plugin_api::v1::RegisterResponse;
use tonic::transport::Endpoint;

use crate::loader::{register, RegisterError};
use crate::registration::RegistrationProblem;

/// 宿主分配的监听地址（`127.0.0.1:端口`）。
pub const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
pub const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";
/// 宿主给插件的数据目录。**必给**：proto 承诺「宿主保证可读写且重装插件时保留」。
pub const DATA_DIR_ENV: &str = "SAKURAMEDIA_PLUGIN_DATA_DIR";
/// 宿主写好的配置文件路径。**可选**：插件没声明 `settings_schema` 时不给。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";
/// 宿主的**能力出口**端点（`PluginHost`）。**可选**：宿主还没起这个服务时不给，
/// 插件据此知道自己这次不能回调宿主（而不是连一个必然失败的地址）。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

/// 配置文件在数据目录下的默认名字。
///
/// 落在 `data_dir` 里而不是另找地方：`data_dir` 是宿主唯一保证存在的目录，
/// 而且它「重装插件时保留」—— 配置跟着数据一起留下，不必再维护一个目录。
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// 探活的重试间隔。
///
/// 50ms：插件进程从 exec 到 listen 通常是几毫秒，而宿主启动时要拉起全部插件 ——
/// 间隔太粗会让「拉 5 个插件」平白多等，太细则空转。
const PROBE_INTERVAL: Duration = Duration::from_millis(50);

/// 怎么拉起一个插件。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// 宿主注入的 `plugin_id`。
    pub plugin_id: String,
    /// manifest 声明的 id —— `Register` 的回显值必须同时等于它。
    pub manifest_id: String,
    /// 可执行文件路径。
    pub program: String,
    /// 命令行参数（**插件自有的**，如参考插件的存储根目录 —— 不属于协议）。
    pub args: Vec<String>,
    /// 插件数据目录。宿主负责建出来并保证可写。
    pub data_dir: PathBuf,
    /// 宿主注入的配置（来自 `plugins.<id>.settings`）。`None` 时不写配置文件。
    pub settings: Option<serde_json::Value>,
    /// 配置文件落点。缺省 [`SETTINGS_FILE_NAME`] 放在 `data_dir` 下。
    pub settings_path: Option<PathBuf>,
    /// 宿主能力出口（`PluginHost`）的端点。`None` 时**不注入**环境变量 ——
    /// 插件据此判断「这次不能回调宿主」，而不是拿到一个必然连不上的地址。
    pub host_endpoint: Option<String>,
    /// 等就绪的上限。超时按「起不来」处理。
    pub ready_timeout: Duration,
}

/// 拉起失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// 进程没起来，或起来了但在就绪之前就退出了。
    Spawn(String),
    /// 数据目录建不出来或配置文件写不进去 —— 进程还没起就已经注定失败。
    Prepare(String),
    /// 进程活着，但到时限也没能通过 `Register` 探活。
    NotReady { waited_ms: u128 },
    /// 探活通了，声明不合契约 —— **不重试**：重试还是同样的答案。
    Invalid(Vec<RegistrationProblem>),
}

impl LaunchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "plugin_spawn_failed",
            Self::Prepare(_) => "plugin_prepare_failed",
            Self::NotReady { .. } => "plugin_not_ready",
            Self::Invalid(_) => "plugin_registration_invalid",
        }
    }
}

/// 一个被宿主拉起来的插件进程。
#[derive(Debug)]
pub struct PluginProcess {
    plugin_id: String,
    addr: SocketAddr,
    child: tokio::process::Child,
}

impl PluginProcess {
    /// 控制面端点（`http://127.0.0.1:端口`）。
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// 宿主分配的那个地址。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 等到进程退出 —— 看门狗靠它发现崩溃。
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// 进程退出了吗。**不阻塞** —— 看门狗要轮询多个插件，`wait` 只能守一个。
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 杀掉进程。测试与「停用插件」都要它。
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        // 句柄被丢弃时把进程一起带走：否则一次 `?` 提前返回就会留下野进程，
        // 而它占着宿主分配的那个端口，下一次拉起永远起不来。
        //
        // 用 `start_kill` 而不是 `kill`：`Drop` 里不能 await。
        let _ = self.child.start_kill();
    }
}

/// 拉起成功的结果：进程句柄 + 注册声明。
#[derive(Debug)]
pub struct LaunchedPlugin {
    /// `Register` 的回显 —— 调用方据此建 provider / 任务 / 扩展点三张表。
    pub registration: RegisterResponse,
    pub process: PluginProcess,
}

/// 拉起一个插件并等它就绪。
pub async fn launch(spec: &LaunchSpec) -> Result<LaunchedPlugin, LaunchError> {
    let addr = reserve_addr().map_err(|err| LaunchError::Spawn(err.to_string()))?;
    let settings_path = prepare(spec)?;
    let child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .env(ADDR_ENV, addr.to_string())
        .env(ID_ENV, &spec.plugin_id)
        .env(DATA_DIR_ENV, &spec.data_dir)
        .envs(
            settings_path
                .as_ref()
                .map(|path| (SETTINGS_FILE_ENV, path.display().to_string())),
        )
        // 能力出口端点同样走"宿主先备好、进程去拿" —— 插件不需要为「宿主的
        // 地址是什么」再开一个 rpc。没有就**不给**这个变量。
        .envs(
            spec.host_endpoint
                .as_ref()
                .map(|endpoint| (HOST_ADDR_ENV, endpoint.clone())),
        )
        // 插件不该从宿主继承 stdin；stdout/stderr 留给运维看（宿主自己的日志是
        // tracing，插件的输出混进来反而难读，所以这里**不**接管）。
        .stdin(Stdio::null())
        .spawn()
        .map_err(|err| LaunchError::Spawn(err.to_string()))?;

    let mut process = PluginProcess {
        plugin_id: spec.plugin_id.clone(),
        addr,
        child,
    };

    let started = std::time::Instant::now();
    loop {
        // 插件若立刻崩（可执行文件格式不对、缺依赖），不必干等到超时。
        if let Ok(Some(status)) = process.child.try_wait() {
            return Err(LaunchError::Spawn(format!(
                "插件进程在就绪前退出，状态 {status}"
            )));
        }
        match probe(&spec.plugin_id, &spec.manifest_id, addr).await {
            Ok(registration) => {
                return Ok(LaunchedPlugin {
                    registration,
                    process,
                })
            }
            Err(RegisterError::Invalid(problems)) => return Err(LaunchError::Invalid(problems)),
            // 还没 listen 起来 —— 下一轮再试。
            Err(RegisterError::Call(_)) => {}
        }
        let waited = started.elapsed();
        if waited >= spec.ready_timeout {
            return Err(LaunchError::NotReady {
                waited_ms: waited.as_millis(),
            });
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// 拉起之前备好数据目录与配置文件。返回配置文件的落点（有配置时）。
///
/// 与端口同一条路子：**宿主先把东西备好，再让进程去拿** —— 插件不需要向宿主
/// 反问「我的配置是什么」，也就不需要为这件事再开一个 rpc。
fn prepare(spec: &LaunchSpec) -> Result<Option<PathBuf>, LaunchError> {
    std::fs::create_dir_all(&spec.data_dir).map_err(|err| {
        LaunchError::Prepare(format!(
            "数据目录 {} 无法创建：{err}",
            spec.data_dir.display()
        ))
    })?;

    let Some(settings) = &spec.settings else {
        return Ok(None);
    };
    let path = spec
        .settings_path
        .clone()
        .unwrap_or_else(|| spec.data_dir.join(SETTINGS_FILE_NAME));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            LaunchError::Prepare(format!("配置目录 {} 无法创建：{err}", parent.display()))
        })?;
    }
    // 每次拉起**重写**：配置的唯一来源是宿主，插件在 data_dir 里留下的旧文件
    // 不该继续生效（否则「改了配置没反应」会变成一个极难查的问题）。
    let text = serde_json::to_string_pretty(settings)
        .map_err(|err| LaunchError::Prepare(format!("配置无法序列化：{err}")))?;
    std::fs::write(&path, text).map_err(|err| {
        LaunchError::Prepare(format!("配置文件 {} 无法写入：{err}", path.display()))
    })?;
    Ok(Some(path))
}

/// 一次探活：连上控制面并走完整的 `Register` 校验。
async fn probe(
    plugin_id: &str,
    manifest_id: &str,
    addr: SocketAddr,
) -> Result<RegisterResponse, RegisterError> {
    let Ok(endpoint) = Endpoint::from_str(&format!("http://{addr}")) else {
        // 地址是宿主自己刚生成的，构造不出来属于本模块的缺陷。
        return Err(RegisterError::Call("endpoint 无法构造".to_owned()));
    };
    let mut client = match PluginControlClient::connect(endpoint).await {
        Ok(client) => client,
        Err(err) => return Err(RegisterError::Call(err.to_string())),
    };
    register(&mut client, plugin_id, manifest_id).await
}

/// 占一个端口再放开，给子进程用。
///
/// **公开**：宿主自己也要占一个端口来 serve `PluginHost`（插件回调宿主的那一侧），
/// 同一个"先占后放"的窗口与取舍对它同样成立。
///
/// **有一个极短的窗口**：放开到子进程 bind 之间，别的进程可能抢走同一个端口。
/// 后果是插件起不来，被探活判成 `NotReady`，重拉一次即可；换成「插件自选端口
/// 再回报」需要第二条通道（stdout 或临时文件），代价更大 —— 见 ADR 文档第 3 节。
pub fn reserve_addr() -> std::io::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

/// 重启退避：`attempt` 从 0 起，指数增长并封顶。
///
/// 与 [`LaunchSpec::ready_timeout`] 分开：**等就绪**是毫秒级的（进程只是慢），
/// **重启退避**是秒级起步的（插件反复崩，别把 CPU 烧在重启上）。
pub fn restart_backoff(attempt: u32, base: Duration, cap: Duration) -> Duration {
    // 位移封在 5 位：再往上 `1 << attempt` 会溢出，而 32 倍 base 通常早已超过
    // 封顶值 —— 封顶才是这里真正的约束。
    let factor = 1u32.checked_shl(attempt.min(5)).unwrap_or(32);
    std::cmp::min(base.saturating_mul(factor), cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_grows_and_stops_at_the_cap() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        assert_eq!(restart_backoff(0, base, cap), Duration::from_secs(1));
        assert_eq!(restart_backoff(1, base, cap), Duration::from_secs(2));
        assert_eq!(restart_backoff(3, base, cap), Duration::from_secs(8));
        // 封顶：再多重试几次也不会越过 30 秒。
        assert_eq!(restart_backoff(10, base, cap), cap);
        assert_eq!(restart_backoff(u32::MAX, base, cap), cap);
    }

    #[test]
    fn a_smaller_cap_than_the_base_wins() {
        // 配置写反了（cap < base）时不该静默变成「不封顶」。
        assert_eq!(
            restart_backoff(4, Duration::from_secs(10), Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn reserved_ports_are_loopback_and_free() {
        let first = reserve_addr().expect("应当拿得到端口");
        let second = reserve_addr().expect("应当拿得到端口");
        assert!(first.ip().is_loopback(), "{first}");
        // 端口是内核分配的：同一个进程连拿两个不该一样。
        assert_ne!(first.port(), second.port());
    }

    #[test]
    fn prepare_creates_the_data_dir_and_writes_the_settings() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            std::env::temp_dir().join(format!(
                "sm-plugins-prepare-{nanos}-{}",
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
        };

        let spec = LaunchSpec {
            plugin_id: "p".to_owned(),
            manifest_id: "p".to_owned(),
            program: "ignored".to_owned(),
            args: Vec::new(),
            data_dir: dir.join("data"),
            settings: Some(serde_json::json!({"timeout_seconds": 5})),
            settings_path: None,
            host_endpoint: None,
            ready_timeout: Duration::from_secs(1),
        };

        let path = prepare(&spec).expect("备好").expect("有配置就该有落点");
        // 缺省落在 data_dir 下。
        assert_eq!(path, dir.join("data").join(SETTINGS_FILE_NAME));
        assert!(spec.data_dir.is_dir(), "数据目录要被建出来");
        // 每次拉起重写：第二遍的内容以宿主为准，不留插件改过的旧值。
        std::fs::write(&path, "{\"timeout_seconds\": 999}").expect("写脏");
        prepare(&spec).expect("再备一次");
        let text = std::fs::read_to_string(&path).expect("读回");
        assert!(text.contains("\"timeout_seconds\": 5"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_settings_means_no_settings_file() {
        let dir = std::env::temp_dir().join("sm-plugins-prepare-no-settings");
        let spec = LaunchSpec {
            plugin_id: "p".to_owned(),
            manifest_id: "p".to_owned(),
            program: "ignored".to_owned(),
            args: Vec::new(),
            data_dir: dir.join("data"),
            settings: None,
            settings_path: None,
            host_endpoint: None,
            ready_timeout: Duration::from_secs(1),
        };
        assert_eq!(prepare(&spec).expect("备好"), None);
        assert!(spec.data_dir.is_dir(), "没有配置也要有数据目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_launch_error_has_a_code() {
        assert_eq!(
            LaunchError::Spawn("x".to_owned()).code(),
            "plugin_spawn_failed"
        );
        assert_eq!(
            LaunchError::Prepare("x".to_owned()).code(),
            "plugin_prepare_failed"
        );
        assert_eq!(
            LaunchError::NotReady { waited_ms: 1 }.code(),
            "plugin_not_ready"
        );
        assert_eq!(
            LaunchError::Invalid(vec![]).code(),
            "plugin_registration_invalid"
        );
    }
}
