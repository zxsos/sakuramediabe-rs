//! 路由层的横切关注点。
//!
//! 目前只有慢请求日志一个。它由组合根（`sm-server`）按环境变量决定是否
//! 挂上，见 [`slow_log::SlowLogConfig::from_env`]。

pub mod slow_log;
