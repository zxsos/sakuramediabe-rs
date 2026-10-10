//! 参考插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。端口由宿主
//!   选定并让出来，插件只管 bind —— 不做「插件自选端口再回报」。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_DATA_DIR`：宿主给的数据目录，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置，**可选**。
//! - `argv[1]`：本地目录后端的数据根。这是**本插件自己的命令行**，不属于协议 ——
//!   协议只管「地址与 id 怎么传」，其余参数各家自定。给了就覆盖配置里的 `root`。
//!
//! # 装配不在这里
//!
//! 本文件只负责「读进程环境 → 交给 [`plugin_ref_local::serve`]」。**进程内**
//! 形态由 `sm-server` 直接调那个 `serve`，跳过这里 —— 两种形态共用同一份服务
//! 装配（`PluginControl` 控制面 + `StorageProvider` 数据面走同一个监听地址：
//! proto 里只有 `data_plane_endpoint` 是插件回给宿主的另一个地址，而它留给字节
//! 搬运，所以控制面与 provider 只能同一个端口）。

use std::net::SocketAddr;

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";
/// 宿主给的数据目录。**必填**：proto 承诺它可读写且重装时保留。
const DATA_DIR_ENV: &str = "SAKURAMEDIA_PLUGIN_DATA_DIR";
/// 宿主写好的配置文件。**可选**：没配置时宿主不给这个变量。
const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;
    // 数据目录是宿主的承诺（proto 的原话）；宿主没给就起不来，而不是自己找地方。
    let data_dir =
        std::env::var(DATA_DIR_ENV).map_err(|_| format!("缺少环境变量 {DATA_DIR_ENV}"))?;
    std::fs::create_dir_all(&data_dir)?;

    // 配置可选：读不到就当空对象，`serve` 全部回落缺省。
    let mut settings = std::env::var(SETTINGS_FILE_ENV)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    // 命令行给的本地目录覆盖配置里的 `root`（本插件自己的约定）。
    if let Some(root) = std::env::args().nth(1) {
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("root".to_owned(), serde_json::Value::String(root));
        }
    }

    // 宿主已经把这个端口让出来了；bind 不上说明端口被别人抢了，直接失败退出，
    // 宿主的探活会把它判成「起不来」。
    plugin_ref_local::serve(addr, plugin_id, settings, None).await
}
