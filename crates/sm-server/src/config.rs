//! 运行配置。
//!
//! 对应上游 `src/config/config.py` 的 `Settings` —— 但**只取组合根用得到的
//! 那一部分**：`database` / `auth` / `logging` / `scheduler.enabled`。
//!
//! # 为什么不全量搬
//!
//! 上游有 11 个配置类，其中 `metadata` / `plugins` / `qdrant` / `downloads` 的
//! 字段全部服务于**尚未移植**的域。它们的字段在这里占位只会有两种结果：
//! 配置写错时静默忽略，或者读者以为「配了就生效」。所以本模块只声明
//! 真正被读取的键，其余随对应域一起加。
//!
//! # 来源优先级
//!
//! ```text
//! 环境变量（SAKURAMEDIA_*） > TOML 配置文件 > 内置默认值
//! ```
//!
//! 与上游的 `BaseSettings` 优先级一致。上游还读 `.env`，这里不读 ——
//! 容器部署里环境变量由 compose 注入，再叠一层 dotenv 只会让「配置从哪来」
//! 变难回答。

use std::time::Duration;

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    /// 数据库连接串。
    pub database_url: String,
    /// JWT 签发密钥。
    pub jwt_secret: String,
    /// 签名 URL 的 HMAC 密钥。
    pub file_signature_secret: String,
    pub listen: ListenConfig,
    pub pool: PoolConfig,
    /// 日志目录。非空且为绝对路径时**额外**写滚动文件。
    pub log_dir: Option<String>,
    /// 日志过滤指令（`tracing` 的 `EnvFilter` 语法，如 `info,sqlx=warn`）。
    pub log_filter: String,
    /// 是否启用后台调度。
    ///
    /// 上游 `scheduler.enabled` 默认 **true**。这里同默认值，但允许关 ——
    /// 单跑 API（开发前端、跑迁移）时不需要调度器在背后入队。
    pub scheduler_enabled: bool,
    /// 调度器 tick 间隔。
    pub scheduler_tick: Duration,
    /// 慢日志开关的**原始**环境变量值（`None` = 未设置）。
    ///
    /// 这里只做**透传**：判定规则（白名单 `1/true/yes/on`）属于 `sm-api`，
    /// 两边各判一次会漂移 —— 上游也只有一处 `slow_log_enabled()`。
    pub slow_log: Option<String>,
}

impl ServerConfig {
    /// 默认值。`database_url` 抄自上游 `Database.url`。
    pub fn with_defaults() -> Self {
        Self {
            database_url: "postgresql://sakuramedia:sakuramedia@postgres:5432/sakuramedia"
                .to_owned(),
            jwt_secret: String::new(),
            file_signature_secret: String::new(),
            listen: ListenConfig::default(),
            pool: PoolConfig::default(),
            log_dir: None,
            log_filter: "info".to_owned(),
            scheduler_enabled: true,
            scheduler_tick: Duration::from_secs(1),
            slow_log: None,
        }
    }

    /// 从环境变量 + 可选 TOML 文件装配。
    ///
    /// `path` 为 `None` 时只读环境变量。**不因文件缺失而失败** —— 容器部署
    /// 里配置全在环境变量，而「配置文件不存在」是那种一旦报错就让人反复
    /// 去创建空文件的噪声。
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let mut config = Self::with_defaults();

        if let Some(path) = path {
            if path.exists() {
                let file = config::Config::builder()
                    .add_source(config::File::from(path))
                    .build()
                    .map_err(|err| ConfigError::file(path, err))?;
                // 单键缺失不算错：默认值继续生效。
                let _ = file.get_string("database.url").map(|url| {
                    config.database_url = url;
                });
                let _ = file
                    .get_string("auth.secret_key")
                    .map(|secret| config.jwt_secret = secret);
                let _ = file.get_string("auth.file_signature_secret").map(|secret| {
                    config.file_signature_secret = secret;
                });
                let _ = file.get_string("logging.level").map(|level| {
                    config.log_filter = level;
                });
                let _ = file.get_string("logging.log_dir").map(|dir| {
                    config.log_dir = Some(dir);
                });
                let _ = file.get_bool("scheduler.enabled").map(|enabled| {
                    config.scheduler_enabled = enabled;
                });
            }
        }

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
            config.database_url, "postgresql://sakuramedia:sakuramedia@postgres:5432/sakuramedia",
            "抄自上游 Database.url"
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
        let config = ServerConfig::load(Some(std::path::Path::new("/nonexistent/config.toml")));
        // 它会因为缺 jwt_secret 而 Err，但**不是**因为文件缺失 —— 用消息区分。
        if let Err(err) = config {
            assert!(
                !err.to_string().contains("config.toml"),
                "不该因文件缺失而失败：{err}"
            );
        }
    }
}
