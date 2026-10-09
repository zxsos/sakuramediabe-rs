//! `tracing` 初始化。
//!
//! # 落盘只在**显式给了目录**时启用
//!
//! 上游 `loguru` 无条件写 `/data/logs`（容器里挂出来）。本地开发时那目录
//! 不存在，于是本地跑测试也会去建 `/data/logs` —— 需要 root、会在容器外
//! 留垃圾、而 CI 上根本没人看。所以这里把落盘做成**可选**：`log_dir` 为空
//! 就只打 stderr。
//!
//! # 滚动参数抄上游
//!
//! `Logging.log_dir` 默认 `/data/logs`，`Logging.level` 默认 `INFO`。
//! 轮转用 `tracing-appender` 的按天滚动，单文件 128 MiB —— 上游那是
//! loguru 的 `rotation="128 MB"`。

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;

/// 初始化结果。**必须持有 `guard`**，否则非阻塞写线程会在 appender 落地前
/// 被 Drop，日志丢一半。
pub struct LogHandles {
    /// 持有它 = 持有后台写线程。`None` 表示只打 stderr。
    _file_guard: Option<WorkerGuard>,
}

/// 安装全局 subscriber。
///
/// # 为什么可能出现「装不上」
///
/// `set_global_default` 在**已经装过**时返回 `Err`。测试里多个用例各自
/// 初始化一次就会撞上。所以这里把 `Err` 也当作成功 —— 真正的重复初始化
/// 在生产里不会发生（`run()` 只被调一次），而让测试为此改结构不划算。
pub fn init(filter: &str, log_dir: Option<&Path>) -> LogHandles {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(filter))
        // 过滤器本身写错时不能拒绝启动 —— 退回 `info` 比 crash 好。
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    match log_dir {
        Some(dir) => init_with_file(env_filter, dir),
        None => init_stderr(env_filter),
    }
}

fn init_stderr(filter: tracing_subscriber::EnvFilter) -> LogHandles {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    LogHandles { _file_guard: None }
}

fn init_with_file(filter: tracing_subscriber::EnvFilter, dir: &Path) -> LogHandles {
    // 建目录失败就退回 stderr：日志写不进去不是拒绝服务的理由。
    if let Err(err) = std::fs::create_dir_all(dir) {
        eprintln!(
            "日志目录 {} 创建失败（{err}），退回 stderr 输出",
            dir.display()
        );
        return init_stderr(filter);
    }

    let appender = tracing_appender::rolling::daily(dir, "sakuramedia.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // 关掉 ansi：写进文件的是字节流，转义序列会变成乱码。
        .with_ansi(false)
        .with_writer(writer)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    LogHandles {
        _file_guard: Some(guard),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_log_dir_falls_back_to_stderr_instead_of_failing() {
        // 用一个**不可能建出来**的路径：/proc 下没有新建目录的权限。
        let handles = init(
            "info",
            Some(Path::new("/proc/definitely-not-writable/logs")),
        );
        // 不管落到 stderr 还是落盘，都必须**不** panic。
        drop(handles);
    }

    #[test]
    fn a_valid_log_dir_is_created() {
        let dir = std::env::temp_dir().join("sm-server-log-test");
        let _ = std::fs::remove_dir_all(&dir);
        let handles = init("info", Some(&dir));
        assert!(dir.exists(), "日志目录应当被创建");
        drop(handles);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_filter_falls_back_to_info() {
        // 过滤器写错不该让服务起不来。
        let handles = init("这不是一个合法的 filter 语法,,,", None);
        drop(handles);
    }
}
