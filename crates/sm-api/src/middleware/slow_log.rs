//! 开发用慢请求日志中间件。
//!
//! 对应上游 `src/api/middleware/slow_requests.py`（41 行），开关与阈值
//! 语义取自 `src/common/perf.py`。
//!
//! # 开关的读法是**白名单**，不是「非空即真**
//!
//! ```python
//! enabled = os.getenv(ENABLED_ENV_KEY, "").strip().lower()
//! return enabled in {"1", "true", "yes", "on"}
//! ```
//!
//! 所以 `SAKURAMEDIA_SLOW_LOG=0` 是**关闭**，而 `SAKURAMEDIA_SLOW_LOG=false`
//! 也是关闭。反过来 `SAKURAMEDIA_SLOW_LOG=` （空值）同样关闭。用
//! `if let Ok(v) = env::var(...)` 那种「存在即真」的写法会让 `=0` 变成启用，
//! 而 `=0` 在 compose 文件里是最自然的写法。
//!
//! # 关闭时是**结构上的零开销**，不是「测了再决定不记」
//!
//! 上游是 `if slow_log_enabled(): app.add_middleware(...)` —— 关闭时中间件
//! 根本不存在。这里照抄：[`SlowLogConfig::from_env`] 返回 `None` 时调用方
//! **不挂层**，于是没有 future 包装、没有 `Instant::now()`、没有 span。
//! 写成「总是挂层、内部判断」的话，每个请求都要付两次系统时钟读加一次
//! `Response` 包装 —— 那正是上游注释里「普通用户零开销」要避免的东西。
//!
//! # 与上游日志行的差异：`db_ms` / `db_queries` 没有对应物
//!
//! 上游靠 peewee 的 `query_hooks` + `ContextVar` 把「这个请求花了多久在
//! 数据库上」归因到请求上。sqlx **没有**全局查询钩子；最接近的是
//! `tracing` span，而 `sm-db` 目前不发 span。所以本中间件只记
//! `method` / `path` / `status` / `duration_ms` / `request_id`，
//! 慢 SQL 的归因留到 `sm-db` 引入 span 之后 —— 记一个恒为 0 的
//! `db_ms=0` 只会让人误以为「查了、很快」，比不记更糟。
//!
//! # 不打完整 query string
//!
//! 上游记的是 `scope["path"]`，**不含** query string。这里保持一致：
//! `?token=...` 之类的参数不该进日志。

use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

/// 开关环境变量名。
pub const ENABLED_ENV_KEY: &str = "SAKURAMEDIA_SLOW_LOG";
/// 慢请求阈值（毫秒）。
pub const REQUEST_MS_ENV_KEY: &str = "SAKURAMEDIA_SLOW_REQUEST_MS";

/// 慢请求阈值默认值，与上游 `DEFAULT_SLOW_REQUEST_MS` 一致。
pub const DEFAULT_SLOW_REQUEST_MS: u64 = 500;

/// 慢日志配置。
///
/// 拆成独立结构体而不是把逻辑塞进中间件：开关与阈值的**解析**才是有分支、
/// 值得测的部分，而中间件那层只是「记不记」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowLogConfig {
    /// 超过这个毫秒数就记一条 warning。`0` 表示关闭。
    pub threshold_ms: u64,
}

impl SlowLogConfig {
    /// 从环境变量读配置。**返回 `None` 表示不启用**。
    ///
    /// 刻意不传 `&mut` 之类可注入参数：环境变量是进程级的，读两遍得到不同
    /// 结果会让测试变得 nondeterministic。测试直接 [`SlowLogConfig::from_env_with`]。
    pub fn from_env() -> Option<Self> {
        Self::from_env_with(
            std::env::var(ENABLED_ENV_KEY).ok().as_deref(),
            std::env::var(REQUEST_MS_ENV_KEY).ok().as_deref(),
        )
    }

    /// 解析逻辑本体，两个入参分别是开关值与阈值值的**原始字符串**。
    ///
    /// 拆出来是为了可测：改环境变量的测试会互相干扰（进程级状态、并行执行），
    /// 而这里可以直接喂 `"  YES "`、`"0"`、`"abc"` 这类边界值。
    pub fn from_env_with(enabled: Option<&str>, threshold: Option<&str>) -> Option<Self> {
        if !is_enabled(enabled) {
            return None;
        }
        Some(Self {
            threshold_ms: parse_ms(threshold).unwrap_or(DEFAULT_SLOW_REQUEST_MS),
        })
    }

    /// 该不该记这条请求。
    fn should_log(&self, elapsed: Duration) -> bool {
        // 阈值 0 = 单独关闭这个日志（上游 `if threshold_ms > 0 and ...`）。
        self.threshold_ms > 0 && elapsed.as_millis() as u64 >= self.threshold_ms
    }
}

/// 开关判定。**白名单**：`1` / `true` / `yes` / `on`，忽略大小写与首尾空白。
fn is_enabled(raw: Option<&str>) -> bool {
    let Some(value) = raw else {
        return false;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// 阈值解析。空值/非数字 → `None`（调用方回退默认值）。
///
/// 上游的 `_env_ms` 在无法解析时也回退默认值，**不**报错：一个写错的阈值
/// 不该让服务起不来。
fn parse_ms(raw: Option<&str>) -> Option<u64> {
    let value = raw?.trim();
    if value.is_empty() {
        return None;
    }
    value.parse::<u64>().ok()
}

/// 8 位十六进制请求 id，与上游 `uuid.uuid4().hex[:8]` 同形。
///
/// 目的是把「一条慢请求日志」与「同一请求里的其它日志」串起来，所以只需要
/// 短且唯一 —— 不需要 uuid 本身。
///
/// # 格式固定 12 位，别用 `{:x}`
///
/// 写成 `format!("{:x}{:x}", nanos, seq)` 的话，`{:x}` **会丢掉前导零** ——
/// 低 32 位纳秒时间戳的十六进制长度在 1..8 之间浮动，于是 id 长度不定。
/// 依赖固定宽度的下游（日志 grep、仪表盘按 id 聚合）会随机失效，而这种失效
/// 只在低 32 位恰好有小值时才显形。
///
/// 所以两个字段都写死宽度：8 位时间戳 + 4 位进程内序号。
///
/// 序号取 `& 0xffff` 是有意的：同一纳秒内最多 65536 个请求，而纳秒分辨率下
/// 一个进程要在同一纳秒里处理 6.5 万个请求才会撞 —— 那时日志本身也已经
/// 淹没了。序号超过 4 位就回绕，不影响正确性。
fn request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    // 进程内自增，避免同一纳秒内的并发请求撞 id。
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:08x}{:04x}", nanos as u32, seq & 0xffff)
}

/// 慢请求日志中间件体。
///
/// 做成 `axum::middleware::from_fn` 而不是手写 `tower::Layer`：
/// 这里的全部状态就是一个 `SlowLogConfig`（`Copy`），而 `Layer` 那套
/// `Service` 泛型只会把「包装一个 future」这件事写长三倍。
///
/// 真正的逻辑在 [`SlowLogConfig`] 与下面的纯函数里，那部分都有单测。
pub async fn slow_request_logger(config: SlowLogConfig, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    // 只取 path，不取 query string，也不取 `?` 之后的部分。
    let path = request.uri().path().to_owned();
    let id = request_id();
    let started = Instant::now();

    let response = next.run(request).await;

    let elapsed = started.elapsed();
    if config.should_log(elapsed) {
        tracing::warn!(
            method = %method,
            path = %path,
            status = response.status().as_u16(),
            duration_ms = elapsed.as_secs_f64() * 1000.0,
            request_id = %id,
            "slow request"
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_a_whitelist_not_presence() {
        for on in ["1", "true", "TRUE", " yes ", "On", "on"] {
            assert!(
                is_enabled(Some(on)),
                "{on:?} 应当启用 —— 上游白名单是 1/true/yes/on"
            );
        }
        // 这些是最容易写错的一批：存在但表示「关」。
        for off in ["0", "false", "no", "off", "", "  ", "enabled", "2"] {
            assert!(!is_enabled(Some(off)), "{off:?} 应当关闭");
        }
        assert!(!is_enabled(None), "未设置时关闭");
    }

    #[test]
    fn a_disabled_switch_yields_no_config_at_all() {
        // 上游是 `if slow_log_enabled(): add_middleware(...)` ——
        // 关闭时中间件不存在，所以「阈值是多少」无关紧要。
        assert_eq!(SlowLogConfig::from_env_with(Some("0"), Some("1")), None);
        assert_eq!(SlowLogConfig::from_env_with(None, Some("1")), None);
        // 开关开着但没给阈值 → 默认 500。
        assert_eq!(
            SlowLogConfig::from_env_with(Some("1"), None),
            Some(SlowLogConfig {
                threshold_ms: DEFAULT_SLOW_REQUEST_MS
            })
        );
    }

    #[test]
    fn an_unparsable_threshold_falls_back_to_the_default() {
        // 写错的阈值不该让服务起不来（上游 `_env_ms` 同样回退默认值）。
        for bad in ["abc", "1.5", "-1", " ", ""] {
            assert_eq!(
                SlowLogConfig::from_env_with(Some("1"), Some(bad)),
                Some(SlowLogConfig {
                    threshold_ms: DEFAULT_SLOW_REQUEST_MS
                }),
                "{bad:?} 应当回退默认值"
            );
        }
        assert_eq!(
            SlowLogConfig::from_env_with(Some("1"), Some("250")),
            Some(SlowLogConfig { threshold_ms: 250 })
        );
    }

    #[test]
    fn a_zero_threshold_disables_the_log_without_disabling_the_switch() {
        let config = SlowLogConfig::from_env_with(Some("1"), Some("0")).unwrap();
        assert_eq!(config.threshold_ms, 0);
        assert!(
            !config.should_log(Duration::from_secs(3600)),
            "阈值为 0 时即使慢到一小时也不记（上游 `threshold_ms > 0` 才记）"
        );
    }

    #[test]
    fn the_threshold_is_inclusive() {
        // 上游是 `duration_ms >= threshold_ms`。
        let config = SlowLogConfig { threshold_ms: 100 };
        assert!(config.should_log(Duration::from_millis(100)));
        assert!(config.should_log(Duration::from_millis(101)));
        assert!(!config.should_log(Duration::from_millis(99)));
    }

    #[test]
    fn request_ids_differ_between_calls_and_have_a_fixed_width() {
        assert_ne!(request_id(), request_id());
        // 宽度是**约定**（见 `request_id` 的文档），不是巧合。多采样几次，
        // 因为 `{:x}` 丢前导零这件事只在低 32 位有小值时才显形。
        for _ in 0..64 {
            assert_eq!(
                request_id().len(),
                12,
                "8 位时间戳 + 4 位序号，长度必须恒定"
            );
        }
    }
}
