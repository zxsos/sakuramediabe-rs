//! 进程内插件宿主：在本进程里跑插件的 `serve`，不起子进程。
//!
//! # 与进程式的关系
//!
//! 两种形态**共用同一个契约**：每个插件 crate 都导出
//!
//! ```ignore
//! pub async fn serve(
//!     addr: SocketAddr,
//!     plugin_id: String,
//!     settings: serde_json::Value,
//!     host_endpoint: Option<String>,
//! ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
//! ```
//!
//! 进程式的 bin 只负责「读 env → 调 `serve`」；这里由组合根直接调 `serve`，把
//! 配置作为**参数**给进去。为什么必须走参数：进程内多插件共用一份进程环境，从
//! `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` / `SAKURAMEDIA_HOST_GRPC_ADDR` 读会互相
//! 覆盖 —— 那正是 `serve` 收显式参数、而不是自己读 env 的原因。
//!
//! # 为什么仍然占一个端口
//!
//! 插件 `serve` 起的是真 gRPC 服务，监听宿主 `reserve_addr` 分配的
//! `127.0.0.1:<port>`。这样**下游调用面一行都不用改**：provider 数据面
//! （`crate::provider_gateway`）、排行取数（`crate::ranking_gateway`）、元数据
//! 搜索（`sm_service::catalog::metadata_source`）、`PluginHost` 回调 —— 全都还是
//! 对着 `http://127.0.0.1:<port>` 发 gRPC，只是那个服务在同一个进程里而已。
//! 省掉的是 10 个子进程的地址空间，不是「网络那一层」。
//!
//! # ⚠️ release 下 `panic = "abort"`：插件 panic 会带走整个后端
//!
//! 根 `Cargo.toml` 的 `[profile.release]` 是 `panic = "abort"`。进程式下插件 panic
//! 死的是它自己的子进程，看门狗按退避重拉；进程内化之后，**任一插件 panic 即整个
//! `sm-server` 退出**（由外层 supervisord / 容器编排拉起）。这是「全部进程内
//! （最省内存）」的代价，必须知情。
//!
//! [`is_inprocess`] 列出的插件才走这条路；其余的仍由
//! [`sm_plugins::supervisor::launch`] 起子进程。

use std::net::SocketAddr;

use serde_json::Value;
use sm_plugin_api::v1::RegisterResponse;
use sm_plugins::supervisor::{await_ready, reserve_addr, LaunchError, LaunchSpec};

/// 进程内插件的分派表：`plugin_id => 那个 crate 的 serve 入口`。
///
/// # 一张表派生两件事
///
/// [`is_inprocess`]（闸门：`Plugins::load` / 看门狗据此决定进程内还是子进程）与
/// [`serve_plugin`]（派发到具体的 `serve`）**都由这一张表生成** —— 少写一条、
/// 或者某条只出现在一处，都不可能：两者不是两份手抄的清单。
///
/// # 一个 crate 两个 id
///
/// `plugin-more-movies` 一个 crate 导出两个 id（`..._more_movies` 与
/// `..._more_rank_movies`，见它 `lib.rs` 的 `MORE_MOVIES_PLUGIN_ID` /
/// `RANK_MOVIES_PLUGIN_ID`），两条都在表里。
///
/// # 只认长 id
///
/// `plugin_id` 来自 `plugins.enabled`，写法必须是这里的长 id。short 写法
/// （如 `javbus`）不在表里 —— 那样它会去走子进程那条路并失败，与今天一致。
macro_rules! inprocess_plugins {
    ($($id:literal => $serve:path),+ $(,)?) => {
        /// 全部进程内插件的 `plugin_id`（顺序 = 表里声明的顺序）。
        pub const INPROCESS_PLUGIN_IDS: &[&str] = &[$($id),+];

        /// 这个 `plugin_id` 是否走进程内。
        ///
        /// 它是**闸门**：`Plugins::load` 与看门狗据此判断该走进程内还是
        /// `supervisor::launch`。判断错误的表现是「插件加载失败」（就绪前探活到
        /// 超时，或找不到可执行文件），而不是编译错误。
        pub fn is_inprocess(plugin_id: &str) -> bool {
            matches!(plugin_id, $($id)|+)
        }

        /// 按 `plugin_id` 分派到对应插件 crate 的 `serve`。
        async fn serve_plugin(
            plugin_id: String,
            addr: SocketAddr,
            settings: Value,
            host_endpoint: Option<String>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            match plugin_id.as_str() {
                $($id => $serve(addr, plugin_id, settings, host_endpoint).await,)+
                other => Err(format!(
                    "未知的进程内插件 id：{other}（见 inprocess_host::is_inprocess）"
                )
                .into()),
            }
        }
    };
}

inprocess_plugins! {
    "sakuramedia_javdb_ranking" => plugin_javdb_ranking::serve,
    "sakuramedia_javbus_metadata" => plugin_javbus_metadata::serve,
    "sakuramedia_actor_metadata" => plugin_actor_metadata::serve,
    "sakuramedia_115_provider" => plugin_115_provider::serve,
    "sakuramedia_judge_collecttion_movie" => plugin_judge_collection::serve,
    "sakuramedia_more_movies" => plugin_more_movies::serve_more_movies,
    "sakuramedia_more_rank_movies" => plugin_more_movies::serve_rank_movies,
    "sakuramedia_movie_scrape_translate" => plugin_scrape_translate::serve,
    "sakuramedia_subtitlecat" => plugin_subtitlecat::serve,
    "plugin_ref_local" => plugin_ref_local::serve,
}

/// 一个已在本进程内拉起的插件。
#[derive(Debug)]
pub struct InProcessPlugin {
    endpoint: String,
    /// `serve` 的任务句柄。正常情况它永不结束；结束即「这个插件挂了」。
    task: tokio::task::JoinHandle<()>,
}

impl InProcessPlugin {
    /// 控制面端点（`http://127.0.0.1:端口`）。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// `serve` 任务结束了吗 —— 看门狗据此把它当成一次崩溃。
    ///
    /// ⚠️ **抓不到 panic**：release 是 `panic = "abort"`，插件 panic 即整个进程
    /// 退出，轮不到看门狗。这里只能发现「任务正常返回」或「`serve` 返回了 Err」。
    /// 见模块文档。
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for InProcessPlugin {
    fn drop(&mut self) {
        // 句柄被替换 / 丢弃时把 `serve` 任务一起收掉，否则旧端口会一直占着，
        // 下一次拉起（看门狗重启）可能又撞上它。
        self.task.abort();
    }
}

/// 进程内拉起成功的结果：注册声明 + 插件句柄（与
/// [`sm_plugins::supervisor::LaunchedPlugin`] 对称）。
#[derive(Debug)]
pub struct LaunchedInProcess {
    /// `Register` 的回显 —— 与进程式走**同一份**契约校验（回显 / ABI / 能力）。
    pub registration: RegisterResponse,
    pub plugin: InProcessPlugin,
}

/// 在本进程里拉起一个插件并等它就绪。
///
/// 与 [`sm_plugins::supervisor::launch`] 的差别只有三处：不 spawn 子进程、配置走
/// 参数而不是环境变量、早期退出看的是 `JoinHandle` 而不是 `Child`。
/// **等就绪的判据与探活都是同一条**（[`await_ready`]）。
pub async fn launch(spec: &LaunchSpec) -> Result<LaunchedInProcess, LaunchError> {
    let addr = reserve_addr().map_err(|err| LaunchError::Spawn(err.to_string()))?;

    // 数据目录：与进程式同一条契约（`Plugins::admit` 要把 `settings-schema.json`
    // 落到这里，插件自己也把它当工作目录）。
    if let Err(err) = std::fs::create_dir_all(&spec.data_dir) {
        return Err(LaunchError::Prepare(format!(
            "数据目录 {} 无法创建：{err}",
            spec.data_dir.display()
        )));
    }

    // 配置作为**参数**给进去，不写进程环境（理由见模块文档）。
    let plugin_id = spec.plugin_id.clone();
    let settings = spec.settings.clone().unwrap_or(Value::Null);
    let host_endpoint = spec.host_endpoint.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = serve_plugin(plugin_id.clone(), addr, settings, host_endpoint).await {
            // 正常情况 `serve` 永不返回（它在 serve 一个不设超时的 server）。
            // 返回 = 端口被抢 / server 出错 / 插件自己收尾了 —— 记下来，
            // 看门狗下一轮会按 `is_finished` 发现并重启。
            tracing::error!(plugin_id, %error, "进程内插件的 serve 提前结束");
        }
    });

    let registration = match await_ready(
        &spec.plugin_id,
        &spec.manifest_id,
        addr,
        spec.ready_timeout,
        || task.is_finished().then(|| "插件任务在就绪前结束".to_owned()),
    )
    .await
    {
        Ok(registration) => registration,
        Err(error) => {
            task.abort();
            return Err(error);
        }
    };

    Ok(LaunchedInProcess {
        registration,
        plugin: InProcessPlugin {
            endpoint: format!("http://{addr}"),
            task,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 闸门必须认下每一个登记的 id —— 漏一个，那个插件会在就绪前探活到超时，
    /// 日志只说「插件加载失败」，看不出是漏登记。
    #[test]
    fn every_registered_id_is_recognized() {
        for id in INPROCESS_PLUGIN_IDS {
            assert!(is_inprocess(id), "{id} 该被认成进程内插件");
        }
        assert!(!is_inprocess("sakuramedia_unknown"));
        // 只认长 id：short 写法不在表里（与 `plugins.enabled` 的约定一致）。
        assert!(!is_inprocess("javbus"));
    }

    /// 表里不该有重复 —— 重复会让 `matches!` 的第二条永远不可达（无害但说明抄错了）。
    #[test]
    fn the_table_has_no_duplicate_ids() {
        let mut seen = std::collections::BTreeSet::new();
        for id in INPROCESS_PLUGIN_IDS {
            assert!(seen.insert(*id), "{id} 在分派表里出现了两次");
        }
    }

    /// 未登记的 id 派发出去要**明确报错**，而不是静默起一个空服务。
    #[tokio::test]
    async fn dispatching_an_unknown_id_fails_loudly() {
        let addr = reserve_addr().expect("占一个端口");
        let error = serve_plugin("nope".to_owned(), addr, Value::Null, None)
            .await
            .expect_err("未登记的 id 必须报错");
        assert!(
            error.to_string().contains("未知的进程内插件 id"),
            "报错要说清原因：{error}"
        );
    }
}
