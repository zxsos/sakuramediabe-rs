//! 进程入口。**只做一件事**：读配置、调用 [`sm_server::run`]、把结果映射成退出码。
//!
//! 装配逻辑全在 [`sm_server::run`] 里，这样集成测试能直接调它而不必启动
//! 一个进程。`main` 里不该有任何「业务」—— 它的职责就是那三行。

use std::process::ExitCode;

use sm_server::{run, ServerConfig};

/// 默认配置文件位置。上游读 `/app/config/config.toml`（容器内挂载）。
const DEFAULT_CONFIG_PATH: &str = "/app/config/config.toml";

#[tokio::main]
async fn main() -> ExitCode {
    // 路径存在就用，不存在就只走环境变量 —— 见 `ServerConfig::load` 的说明。
    let path = std::path::Path::new(DEFAULT_CONFIG_PATH);
    let config = match ServerConfig::load(Some(path)) {
        Ok(config) => config,
        Err(err) => {
            // 此时日志系统还没装好（配置阶段先于日志初始化），只能 eprintln。
            eprintln!("配置错误：{err}");
            return ExitCode::from(2);
        }
    };

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // 日志已初始化，错误链进日志而不是只打一行。
            tracing::error!(error = %err, "启动失败");
            tracing::error!("{}", err);
            ExitCode::FAILURE
        }
    }
}
