//! 生命周期的集成测试：宿主**真的拉起**参考插件进程。
//!
//! # 为什么放这里
//!
//! 被测的是 `sm-plugins::supervisor`（宿主侧）与 `plugin-ref-local` 的可执行文件
//! （插件侧）之间的那条协议。可执行文件只有本 crate 有，而 `env!("CARGO_BIN_EXE_…")`
//! 只在**定义了这个 bin 的 crate** 的测试里可用，所以用例落在这里。
//!
//! # 逐条锁定的语义
//!
//! | 用例 | 断言 | 为什么重要 |
//! |---|---|---|
//! | 拉起并注册 | 回显 id / ABI / 声明都在 | 「宿主能拉起一个插件」的落点 |
//! | 端口是宿主给的 | 插件 bind 的正是宿主分配的地址 | 协议第 1 条的实质 |
//! | 进程崩了能被发现 | `wait()` 拿到退出状态 | 看门狗的唯一依据 |
//! | 起不来有明确错误 | `Spawn` 而不是干等到超时 | 排障时最需要区分的一类 |

use std::time::Duration;

use plugin_ref_local::fixture::scratch_root;
use sm_plugins::supervisor::{launch, LaunchError, LaunchSpec};

/// 参考插件可执行文件（cargo 在编译测试时把路径写进这个环境变量）。
const PLUGIN_BIN: &str = env!("CARGO_BIN_EXE_plugin-ref-local");

fn spec(plugin_id: &str, root: &std::path::Path) -> LaunchSpec {
    LaunchSpec {
        plugin_id: plugin_id.to_owned(),
        manifest_id: plugin_id.to_owned(),
        program: PLUGIN_BIN.to_owned(),
        args: vec![root.display().to_string()],
        // 数据目录与存储根分开：前者是宿主给插件的，后者是本插件自己的命令行参数。
        data_dir: scratch_root("lifecycle-data"),
        settings: None,
        settings_path: None,
        // 本测试不起宿主的 `PluginHost` 服务，所以插件这次**不能**回调宿主 ——
        // 这正是协议要表达的：「没有这个变量」而不是「给一个连不上的地址」。
        host_endpoint: None,
        ready_timeout: Duration::from_secs(10),
    }
}

#[tokio::test]
async fn the_host_launches_the_plugin_and_gets_its_registration() {
    let root = scratch_root("lifecycle");
    std::fs::create_dir_all(&root).expect("数据根");

    let mut launched = launch(&spec("local", &root))
        .await
        .expect("参考插件应当能拉起来并注册");

    let registration = &launched.registration;
    // 回显宿主注入的 id —— 不回显就是加载失败（proto 的原话）。
    assert_eq!(registration.plugin_id, "local");
    assert_eq!(registration.abi_major, sm_plugin_api::ABI_MAJOR);
    // ★ 本插件只 serve `StorageProvider`，所以**不声明**任何能力 ——
    // 这里曾经断言 `vec![50]`（Download），而 `DownloadProvider` 全 crate 没有
    // 实现：声明与实现不一致，宿主会以为能提交下载。
    assert!(
        registration.capabilities.is_empty(),
        "没实现的 provider 不该声明能力：{:?}",
        registration.capabilities
    );
    // 声明里带着 media.provider 扩展点，宿主据此建 provider 表。
    assert_eq!(registration.extensions.len(), 1);
    assert_eq!(registration.extensions[0].key, "media.provider");

    // 端点可用：宿主后续调用走的就是它。
    assert!(
        launched.process.endpoint().starts_with("http://127.0.0.1:"),
        "{}",
        launched.process.endpoint()
    );
    launched.process.kill().await.expect("收尾");
}

#[tokio::test]
async fn the_plugin_binds_the_address_the_host_handed_it() {
    // 协议第 1 条：端口由宿主分配，不是插件自选后回报。
    let root = scratch_root("lifecycle-addr");
    std::fs::create_dir_all(&root).expect("数据根");

    let mut launched = launch(&spec("local", &root)).await.expect("拉起");
    // 宿主分配的地址上真的有人在听 —— 连一次控制面即可证明。
    let mut client = sm_plugin_api::v1::plugin_control_client::PluginControlClient::connect(
        launched.process.endpoint(),
    )
    .await
    .expect("端点上应当能连上控制面");
    let response = sm_plugins::loader::register(&mut client, "local", "local")
        .await
        .expect("再注册一次也应当通过");
    assert_eq!(response.plugin_id, "local");

    launched.process.kill().await.expect("收尾");
}

#[tokio::test]
async fn a_crashed_plugin_is_detected_by_waiting() {
    // 看门狗的唯一依据：进程退出。这里用 kill 模拟崩溃。
    let root = scratch_root("lifecycle-crash");
    std::fs::create_dir_all(&root).expect("数据根");

    let mut launched = launch(&spec("local", &root)).await.expect("拉起");
    launched.process.kill().await.expect("杀掉");
    let status = launched.process.wait().await.expect("等退出");
    assert!(!status.success(), "被杀的进程不该是成功退出：{status}");
}

#[tokio::test]
async fn the_settings_the_host_writes_reach_the_plugin() {
    // 配置没有 rpc 可用（`RegisterRequest` 只有 id 与 ABI），所以走「宿主写文件
    // + 环境变量指路」。这条用例锁的是**整条通道**：宿主写 → 插件读 → 影响声明。
    let root = scratch_root("lifecycle-settings");
    std::fs::create_dir_all(&root).expect("数据根");

    let mut spec = spec("local", &root);
    spec.settings = Some(serde_json::json!({"provider_key": "configured_ref"}));

    let mut launched = launch(&spec).await.expect("拉起");
    let Some(sm_plugin_api::v1::extension::Data::MediaProvider(bundle)) =
        &launched.registration.extensions[0].data
    else {
        panic!("应当是 media.provider 载荷");
    };
    assert_eq!(bundle.provider_key, "configured_ref", "配置应当生效");

    launched.process.kill().await.expect("收尾");
}

#[tokio::test]
async fn a_plugin_that_cannot_serve_is_reported_as_spawn_failed() {
    // 给一个**错误**的插件 id 给启动参数里那个：进程会起来，但 `Register` 会
    // 因「回显不一致」被打回 —— 这类失败必须立刻报，而不是干等超时。
    let root = scratch_root("lifecycle-bad-id");
    std::fs::create_dir_all(&root).expect("数据根");

    let mut spec = spec("expected", &root);
    spec.manifest_id = "someone-else".to_owned();

    let error = launch(&spec).await.expect_err("id 不一致应当失败");
    assert_eq!(error.code(), "plugin_registration_invalid", "{error:?}");
    let LaunchError::Invalid(problems) = &error else {
        panic!("应当归为声明不合契约：{error:?}");
    };
    assert!(!problems.is_empty());
}
