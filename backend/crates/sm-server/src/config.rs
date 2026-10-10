//! 运行配置。
//!
//! 分两层，各管一件事：
//!
//! | 层 | 内容 | 键从哪来 |
//! |---|---|---|
//! | [`ServerConfig`] 的**进程级**字段 | `listen` / `pool` / `scheduler_tick` / `log_filter` / `slow_log` | **无上游对应物**，只有环境变量 |
//! | [`ServerConfig`] 的**域值**字段 | `database_url` / `jwt_secret` / `file_signature_secret` / `scheduler_enabled` / `log_dir` | [`sm_core::config_schema`] 的模式 + TOML |
//!
//! # 为什么不把 11 个节都做成结构体
//!
//! 之前这里**硬编码**了 `database.url` 的默认值字面量，而
//! `sm_core::config_schema` 里也有一份 —— 两份真相来源，配置改动要记得改两处。
//! 现在默认值只存在于模式表里，本模块通过 [`schema::View`] 按 `节.字段` 取，
//! 拼错键名得到 `None` 而不是编一个假值。
//!
//! 那张表覆盖全部 10 个节（62 个字段），本模块只**读**其中 5 个。读不到的不
//! 必声明：声明了却没人用，会让人以为「配了就生效」。
//!
//! # 曾经读错的一个键
//!
//! `log_dir` 之前从 `logging.log_dir` 读 —— **上游没有这个键**。它在
//! `scheduler.log_dir`（`config.py:158`）。于是写进配置文件的值被静默忽略，
//! 而 `logging.rs` 的模块文档还把它当成真的引用了一遍。两处都改了。
//!
//! # 来源优先级
//!
//! ```text
//! 环境变量（SAKURAMEDIA_*） > TOML 配置文件 > 模式默认值
//! ```
//!
//! 与上游的 `BaseSettings` 优先级一致。上游还读 `.env`，这里不读 ——
//! 容器部署里环境变量由 compose 注入，再叠一层 dotenv 只会让「配置从哪来」
//! 变难回答。
//!
//! 环境变量名沿用本仓库既有的扁平形式（`SAKURAMEDIA_JWT_SECRET` 对应
//! `auth.secret_key`），它不是 pydantic-settings 的嵌套默认写法，但已经是
//! 部署契约（`docker-compose.yml` 在用），不为了「更标准」而改。

use std::path::{Path, PathBuf};
use std::time::Duration;

use sm_core::config_schema as schema;

use crate::error::ConfigError;

/// 监听地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ListenConfig {
    fn default() -> Self {
        // 上游 uvicorn 绑 0.0.0.0:8000（docker-compose 里映射到宿主）。
        Self {
            host: "0.0.0.0".to_owned(),
            port: 8000,
        }
    }
}

impl ListenConfig {
    /// 绑定的 socket 地址。
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 连接池。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// 最大连接数。
    ///
    /// 上游是单进程（`uvicorn --workers 1`），所以这个值只需要覆盖
    /// 「单进程能并发处理多少请求」。默认 20 与本机 PG 的
    /// `max_connections=100` 留足余量（测试夹具的注释也提到它按 CPU 数
    /// 并发取连接，16 个测试各持一条）。
    pub max_connections: u32,
    /// 取连接的超时。
    ///
    /// **必须有**：没有它，一个耗尽连接池的请求会永远挂着 —— 客户端看到的是
    /// 「请求卡住」而不是 503，排查时只能去看负载均衡器的超时。
    pub acquire_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 20,
            acquire_timeout: Duration::from_secs(10),
        }
    }
}

/// 运行配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    // ---- 域值：键来自 sm_core::config_schema ----
    /// 数据库连接串（`database.url`）。
    pub database_url: String,
    /// JWT 签发密钥（`auth.secret_key`）。
    pub jwt_secret: String,
    /// 签名 URL 的 HMAC 密钥（`auth.file_signature_secret`）。
    pub file_signature_secret: String,
    /// 是否启用后台调度（`scheduler.enabled`）。
    ///
    /// 上游默认 **true**。这里同默认值，但允许关 —— 单跑 API（开发前端、跑
    /// 迁移）时不需要调度器在背后入队。
    pub scheduler_enabled: bool,
    /// 日志目录（`scheduler.log_dir`）。`None` = 只打 stderr，不落盘。
    ///
    /// # 与上游的一处刻意差异
    ///
    /// 上游 `loguru` 无条件写 `scheduler.log_dir`（默认 `/data/logs`）。本机
    /// 开发时那目录不存在，于是跑测试也会去建 `/data/logs` —— 需要 root、会在
    /// 容器外留垃圾、而 CI 上根本没人看。所以落盘做成**可选**：`None` 就只打
    /// stderr。
    ///
    /// 代价是「配了 `scheduler.log_dir` 却不落盘」不会自动发生，但反过来
    /// 「没配就静默不落盘」会 —— 所以这个字段是 `Option` 而不是 `String`，
    /// 让「有没有落盘」在类型上就是一个需要回答的问题。
    pub log_dir: Option<String>,

    // ---- 进程级：上游没有对应键，只有环境变量 ----
    /// 配置文件路径。
    ///
    /// # 为什么 `load` 收了路径还要把它留下来
    ///
    /// 因为 `PATCH /config` 要写回**同一个文件**。只在 `load` 里用完就丢的话，
    /// API 层就得自己再猜一次路径 —— 而猜错的后果是「配置写进了另一个文件，
    /// 重启后改动消失」，这种问题没有任何错误会提示你。
    pub config_path: PathBuf,
    pub listen: ListenConfig,
    pub pool: PoolConfig,
    /// 日志过滤指令。
    ///
    /// 默认 `info`。TOML 的 `logging.level`（上游默认 `INFO`）会覆盖它 ——
    /// `EnvFilter` 认得裸级别。`SAKURAMEDIA_LOG_FILTER` 才能给完整的
    /// EnvFilter 指令（`info,sqlx=warn`），因为上游的 `logging.level` 只是一个
    /// 级别字符串，没有表达模块级指令的地方。
    pub log_filter: String,
    /// 调度器 tick 间隔。
    pub scheduler_tick: Duration,
    /// 慢日志开关的**原始**环境变量值（`None` = 未设置）。
    ///
    /// 这里只做**透传**：判定规则（白名单 `1/true/yes/on`）属于 `sm-api`，
    /// 两边各判一次会漂移 —— 上游也只有一处 `slow_log_enabled()`。
    pub slow_log: Option<String>,
}

impl Default for ServerConfig {
    /// 默认值。域值字段取自 [`sm_core::config_schema`] 的模式表。
    ///
    /// 刻意**不**用 `#[derive(Default)]`：那会让 `String` 字段是空串而不是
    /// 模式里的默认值，于是「没加载配置文件」与「加载了但键缺失」长得一样，
    /// 而后者该回落到 `database.url` 的默认值。
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl ServerConfig {
    /// 默认值。域值字段来自模式表，不在本模块写字面量。
    pub fn with_defaults() -> Self {
        let view = schema::View::new(defaults_object());
        Self {
            database_url: view.str_or_default("database", "url").unwrap_or_default(),
            jwt_secret: view
                .str_or_default("auth", "secret_key")
                .unwrap_or_default(),
            file_signature_secret: view
                .str_or_default("auth", "file_signature_secret")
                .unwrap_or_default(),
            scheduler_enabled: view.bool("scheduler", "enabled").unwrap_or(true),
            log_dir: None,
            config_path: default_config_path(),
            listen: ListenConfig::default(),
            pool: PoolConfig::default(),
            log_filter: "info".to_owned(),
            scheduler_tick: Duration::from_secs(1),
            slow_log: None,
        }
    }

    /// 从环境变量 + 可选 TOML 文件装配。
    ///
    /// `path` 为 `None` 时只读环境变量。**不因文件缺失而失败** —— 容器部署
    /// 里配置全在环境变量，而「配置文件不存在」是那种一旦报错就让人反复
    /// 去创建空文件的噪声。
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config = Self::from_file(path)?;

        // 环境变量覆盖文件。
        if let Ok(url) = std::env::var("SAKURAMEDIA_DATABASE_URL") {
            config.database_url = url;
        }
        if let Ok(secret) = std::env::var("SAKURAMEDIA_JWT_SECRET") {
            config.jwt_secret = secret;
        }
        if let Ok(secret) = std::env::var("SAKURAMEDIA_FILE_SIGNATURE_SECRET") {
            config.file_signature_secret = secret;
        }
        if let Ok(host) = std::env::var("SAKURAMEDIA_HOST") {
            config.listen.host = host;
        }
        if let Ok(port) = std::env::var("SAKURAMEDIA_PORT") {
            config.listen.port = port.parse().map_err(|_| ConfigError::Invalid {
                key: "SAKURAMEDIA_PORT",
                reason: format!("{port:?} 不是合法端口"),
            })?;
        }
        if let Ok(dir) = std::env::var("SAKURAMEDIA_LOG_DIR") {
            config.log_dir = Some(dir);
        }
        if let Ok(filter) = std::env::var("SAKURAMEDIA_LOG_FILTER") {
            config.log_filter = filter;
        }
        if let Ok(enabled) = std::env::var("SAKURAMEDIA_SCHEDULER_ENABLED") {
            config.scheduler_enabled = parse_bool(&enabled);
        }
        if let Ok(tick) = std::env::var("SAKURAMEDIA_SCHEDULER_TICK_SECONDS") {
            let seconds: u64 = tick.parse().map_err(|_| ConfigError::Invalid {
                key: "SAKURAMEDIA_SCHEDULER_TICK_SECONDS",
                reason: format!("{tick:?} 不是合法秒数"),
            })?;
            config.scheduler_tick = Duration::from_secs(seconds.max(1));
        }
        // 慢日志沿用上游的环境变量名，`sm-api` 那边读的是同一组。
        config.slow_log = std::env::var("SAKURAMEDIA_SLOW_LOG").ok();

        config.validate()?;
        Ok(config)
    }

    /// 只从 TOML 文件装配，不看环境变量。
    ///
    /// 分出来是因为**测试**需要它：进程的环境变量在同一个测试进程里改不掉
    /// （`set_var` 在 Rust 2024 里是 unsafe，且会污染并行跑的其它用例）。
    pub fn from_file(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config = Self::with_defaults();
        let Some(path) = path.filter(|p| p.exists()) else {
            return Ok(config);
        };
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::file(path, e))?;
        // 解析失败必须是错误：半个 TOML 意味着运维写错了配置，而静默忽略会让
        // 服务用默认值启动，看起来「配置没生效」却查不出原因。
        let from_disk: serde_json::Value = toml::from_str(&text)
            .map_err(|e| ConfigError::file(path, format!("不是合法 TOML: {e}")))?;
        let merged = schema::overlay_defaults(from_disk.clone());
        let view = schema::View::new(merged.as_object().expect("overlay 一定产出对象"));

        if let Some(url) = view.str("database", "url") {
            config.database_url = url.to_owned();
        }
        if let Some(secret) = view.str("auth", "secret_key") {
            config.jwt_secret = secret.to_owned();
        }
        if let Some(secret) = view.str("auth", "file_signature_secret") {
            config.file_signature_secret = secret.to_owned();
        }
        if let Some(enabled) = view.bool("scheduler", "enabled") {
            config.scheduler_enabled = enabled;
        }
        // 上游的键在 `scheduler` 节，不在 `logging` 节。
        //
        // 这里读的是**磁盘原文**而不是 `view` —— 因为 `scheduler.log_dir` 在模式
        // 里有非空默认值 `/data/logs`，读叠加后的值会让「落盘」变成恒真，而
        // 落盘在本机是要 root 的（见 `log_dir` 字段文档）。「有没有落盘」必须
        // 只由运维**实际写了什么**决定。
        if let Some(dir) = disk_str(&from_disk, "scheduler", "log_dir") {
            // 空串 = 明确不要落盘。
            config.log_dir = (!dir.is_empty()).then(|| dir.to_owned());
        }
        if let Some(level) = view.str("logging", "level") {
            config.log_filter = level.to_ascii_lowercase();
        }
        Ok(config)
    }

    /// 必填项检查。
    ///
    /// # 为什么密钥「缺失」是错误，而上游默认是空串
    ///
    /// 上游 `secret_key: str = ""`，并在启动时自举生成（`ensure_runtime_config`
    /// 写盘）。那个自举会**持久化**一个随机密钥，而本仓库还没有这一步 ——
    /// 于是空密钥意味着一把所有人相同的公开密钥，任何人都能伪造 access token。
    /// 宁可不启动。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.database_url.trim().is_empty() {
            return Err(ConfigError::Missing("database_url"));
        }
        if self.jwt_secret.trim().is_empty() {
            return Err(ConfigError::Missing(
                "jwt_secret（设置 SAKURAMEDIA_JWT_SECRET；空密钥等于公开密钥）",
            ));
        }
        if self.pool.max_connections == 0 {
            return Err(ConfigError::Invalid {
                key: "pool.max_connections",
                reason: "必须 ≥ 1".to_owned(),
            });
        }
        if self.scheduler_tick.is_zero() {
            return Err(ConfigError::Invalid {
                key: "scheduler.tick",
                reason: "tick 间隔必须 ≥ 1 秒；0 会让循环空转烧 CPU".to_owned(),
            });
        }
        Ok(())
    }
}

/// 配置文件路径的默认值，抄上游 `SETTINGS_TOML_PATH` 的两分支。
///
/// ```text
/// SAKURAMEDIA_CONFIG_PATH（若设置） > /data 存在 ? /data/config/config.toml : ./config.toml
/// ```
///
/// 上游的判据是「`/data` 是不是目录」而不是「配置文件在不在」：容器里
/// entrypoint 会先 `mkdir -p /data/config`，但此刻文件还不存在 —— 这正是
/// 首次启动要走的那条路。如果改成「文件存在才用 /data」，首次启动就会回落到
/// 仓库内的路径，把配置写进镜像里。
pub fn default_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("SAKURAMEDIA_CONFIG_PATH") {
        return PathBuf::from(path);
    }
    if Path::new("/data").is_dir() {
        PathBuf::from("/data/config/config.toml")
    } else {
        PathBuf::from("config.toml")
    }
}

/// 从**磁盘原文**（未经默认值叠加）取字符串。
///
/// 存在的理由只有一个：`scheduler.log_dir` 在模式里有非空默认值，而「要不要
/// 落盘」必须只看运维实际写了什么。见 `from_file` 里那处调用。
fn disk_str<'a>(raw: &'a serde_json::Value, section: &str, field: &str) -> Option<&'a str> {
    raw.get(section)?.as_object()?.get(field)?.as_str()
}

/// 模式默认值快照（`defaults_json` 的对象视图）。
fn defaults_object() -> &'static serde_json::Map<String, serde_json::Value> {
    // `LazyLock` 里的 `Value` 一旦构建就地址稳定，所以这里能返回 `&'static`。
    // 每次调用都重新构造一个临时对象再取引用是做不到的 —— 那才是为什么
    // `sm_core::config_schema` 里的 `View` 接受调用方持有的快照。
    static DEFAULTS: std::sync::LazyLock<serde_json::Value> =
        std::sync::LazyLock::new(schema::defaults_json);
    DEFAULTS.as_object().expect("defaults_json 一定产出对象")
}

/// 上游的白名单式布尔解析：`1/true/yes/on` 为真。
///
/// 抄 `src/common/perf.py` 的 `slow_log_enabled()` —— 同一份配置里两处
/// 布尔语义不同（「非空即真」vs「白名单」）正是配置出错的来源，所以这里
/// 保持与上游一致而不是按 Rust 习惯改成「非空即真」。
fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let config = ServerConfig::with_defaults();
        assert_eq!(config.listen.port, 8000);
        assert_eq!(config.listen.host, "0.0.0.0");
        assert!(config.scheduler_enabled, "上游默认起调度器");
        assert_eq!(
            config.database_url, "postgresql://sakuramedia:sakuramedia@postgres:5432/sakuramediabe",
            "抄自上游 Database.url（config.py:38）"
        );
        assert_eq!(config.pool.max_connections, 20);
    }

    #[test]
    fn an_empty_jwt_secret_refuses_to_start() {
        let mut config = ServerConfig::with_defaults();
        assert!(config.validate().is_err(), "空密钥等于公开密钥");
        config.jwt_secret = "s3cret".to_owned();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_zero_tick_interval_is_rejected() {
        let mut config = ServerConfig::with_defaults();
        config.jwt_secret = "s3cret".to_owned();
        config.scheduler_tick = Duration::ZERO;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("tick"), "{err}");
    }

    #[test]
    fn a_zero_pool_size_is_rejected() {
        let mut config = ServerConfig::with_defaults();
        config.jwt_secret = "s3cret".to_owned();
        config.pool.max_connections = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_bind_address_is_host_colon_port() {
        let listen = ListenConfig {
            host: "127.0.0.1".to_owned(),
            port: 9123,
        };
        assert_eq!(listen.bind_address(), "127.0.0.1:9123");
    }

    #[test]
    fn booleans_use_the_upstream_whitelist() {
        for on in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(parse_bool(on), "{on:?} 应为真");
        }
        // 这些是最容易写错的一批：非空但表意是「关」。
        for off in ["0", "false", "no", "off", ""] {
            assert!(!parse_bool(off), "{off:?} 应为假");
        }
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        // 容器部署里配置全在环境变量；要求「文件必须存在」只会让人去创建一个
        // 空文件来消掉报错。
        let config = ServerConfig::load(Some(Path::new("/nonexistent/config.toml")));
        // 它会因为缺 jwt_secret 而 Err，但**不是**因为文件缺失 —— 用消息区分。
        if let Err(err) = config {
            assert!(
                !err.to_string().contains("config.toml"),
                "不该因文件缺失而失败：{err}"
            );
        }
    }

    /// 写一个临时配置文件并读它。
    fn from_toml(tag: &str, body: &str) -> ServerConfig {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("sm-server-cfg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("config.toml");
        std::fs::write(&path, body).expect("写配置");
        ServerConfig::from_file(Some(&path)).expect("读配置")
    }

    #[test]
    fn domain_values_come_from_the_file() {
        let config = from_toml(
            "domain",
            r#"
[database]
url = "postgresql://u:p@db:5432/app"

[auth]
secret_key = "from-file"
file_signature_secret = "sig-from-file"

[scheduler]
enabled = false
log_dir = "/var/log/sakura"

[logging]
level = "DEBUG"
"#,
        );
        assert_eq!(config.database_url, "postgresql://u:p@db:5432/app");
        assert_eq!(config.jwt_secret, "from-file");
        assert_eq!(config.file_signature_secret, "sig-from-file");
        assert!(
            !config.scheduler_enabled,
            "文件里的 scheduler.enabled=false 要生效"
        );
        assert_eq!(
            config.log_dir.as_deref(),
            Some("/var/log/sakura"),
            "log_dir 在 scheduler 节，不在 logging 节"
        );
        assert_eq!(config.log_filter, "debug", "logging.level 会被小写化");
    }

    #[test]
    fn a_partial_section_keeps_the_schema_defaults() {
        // 只写了一个键的那一节，其余键要回落到模式默认值 —— 而不是变成 null。
        let config = from_toml(
            "partial",
            r#"
[scheduler]
log_dir = "/var/log/sakura"
"#,
        );
        assert!(config.scheduler_enabled, "同节未写的键回落默认值 true");
        assert_eq!(
            config.database_url,
            schema::field_of("database", "url")
                .map(schema::default_of)
                .and_then(|v| v.as_str().map(str::to_owned))
                .expect("模式里有 database.url"),
            "完全没出现的节用模式默认值，且本模块不写字面量"
        );
    }

    #[test]
    fn the_log_dir_default_does_not_silently_enable_file_logging() {
        // 与上游的刻意差异：上游 loguru 无条件写 /data/logs，本机跑测试不该跟着建。
        let config = from_toml("nodefault", "[scheduler]\nenabled = true\n");
        assert_eq!(
            config.log_dir, None,
            "没显式配 scheduler.log_dir 就不落盘 —— 落盘是可选的"
        );
    }

    #[test]
    fn a_typo_in_a_key_name_is_silently_ignored_rather_than_reported() {
        // 这条测试记录的是 `View` 的**已知局限**，不是期望行为：
        //
        // 写 `[logging] log_dir = "..."`（上游没这个键）不会报错，只是被忽略。
        // 上游也有同样的性质 —— pydantic 的 `extra` 默认行为取决于配置，而
        // `Settings` 没有设 `extra="forbid"`。
        //
        // 之所以断言它，是因为**它曾经是个真 bug**：本模块读的就是
        // `logging.log_dir`，于是配置文件里的日志目录一直不生效，而没有任何
        // 迹象。现在键名改对了，但「拼错的键被静默忽略」这个性质还在 ——
        // 写测试的人看到这条就会知道，别指望它能发现拼写错误。
        let config = from_toml("typo", "[logging]\nlog_dir = \"/nope\"\n");
        assert_eq!(
            config.log_dir, None,
            "logging.log_dir 不是上游的键，必须被忽略"
        );
    }

    #[test]
    fn a_malformed_toml_is_an_error_not_a_silent_default() {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("sm-server-bad-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[database\nurl = ").expect("写坏配置");
        let err = ServerConfig::from_file(Some(&path)).expect_err("半个 TOML 必须报错");
        assert!(
            err.to_string().contains("TOML"),
            "错误消息要说清是 TOML 解析失败：{err}"
        );
    }
}
