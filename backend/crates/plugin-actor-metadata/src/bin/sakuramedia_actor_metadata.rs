//! 可执行文件：宿主按生命周期协议拉起的那个进程。
//!
//! 与 `plugin-ref-local/src/bin/plugin-ref-local.rs` 同一套骨架，只少一个数据面
//! service —— 本插件**只有**控制面（`PluginControl`：`register` + `run_job`），
//! 不声明任何扩展点（见 `src/service.rs` 的 `register`）。
//!
//! 装配本身不在这里：本文件只负责「读进程环境 → 交给
//! [`plugin_actor_metadata::serve`]」。**进程内**形态由 `sm-server` 直接调
//! 那个 `serve`，跳过这里 —— 两种形态共用同一份服务装配。
//!
//! 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）：
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**；
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**；
//! - `SAKURAMEDIA_PLUGIN_DATA_DIR`：数据目录，**必填**（proto 承诺可读写）；
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置，**可选**；
//! - `SAKURAMEDIA_HOST_GRPC_ADDR`：宿主能力出口，**可选**（任务运行时才用）。

use std::net::SocketAddr;

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";
/// 宿主给的数据目录。
const DATA_DIR_ENV: &str = "SAKURAMEDIA_PLUGIN_DATA_DIR";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;
    let data_dir =
        std::env::var(DATA_DIR_ENV).map_err(|_| format!("缺少环境变量 {DATA_DIR_ENV}"))?;
    std::fs::create_dir_all(&data_dir)?;

    // 配置与宿主端点都是可选的：读不到就用默认 / 不留宿主端点（任务运行时
    // 会因缺少宿主而失败，但注册阶段照常）。
    let settings = std::env::var(plugin_actor_metadata::settings::SETTINGS_FILE_ENV)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    let host_endpoint = std::env::var(plugin_actor_metadata::service::HOST_ADDR_ENV).ok();

    // 宿主已经把这个端口让出来了；bind 不上说明被抢了，直接失败退出，
    // 宿主的探活会把它判成「起不来」。
    plugin_actor_metadata::serve(addr, plugin_id, settings, host_endpoint).await
}
