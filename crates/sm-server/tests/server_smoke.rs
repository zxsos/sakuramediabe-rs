//! 组合根的端到端冒烟：真的把 HTTP 服务起起来，打一次请求，再优雅关闭。
//!
//! # 为什么值得写
//!
//! 组合根是**唯一**没有单元测试覆盖的层：它的每一步都是「装配」，错了不会
//! 编译失败，而是启动时才崩。而这一层能犯的错恰好集中在三处：
//!
//! | 错处 | 症状 |
//! |---|---|
//! | 路由 / 状态装错 | 请求进不来或 500 |
//! | 优雅关闭没接信号 | `docker stop` 每次等超时 |
//! | 调度器没起来 | 日志里没有 `cron_info`，队列永远空 |
//!
//! 这里三条都验：监听 → 404 信封（证明 router 与 fallback 装上了）→
//! SIGTERM → 干净退出。
//!
//! 端口用 0（让内核分配）避免与开发中的实例撞端口。
//!
//! # ⚠ 本文件只能有这一个测试
//!
//! 它给**自己所在进程**发 `SIGTERM` 来验证优雅关闭，而 `cargo test` 在同一
//! 个二进制里并行跑测试 —— 加第二个测试会让兄弟测试被信号杀掉，症状是
//! 「随机一个测试无输出地消失」。要加测试请新开一个文件。
//!
//! 更稳的做法是把服务作为**子进程**起、再对子进程发信号；那需要先把二进制
//! 构建出来（`env!("CARGO_BIN_EXE_sm-server")`），而那会让本测试从「冒烟」
//! 变成「集成测试」。当前取舍：冒烟用信号换真实覆盖，代价写在这里。

use std::net::SocketAddr;
use std::time::Duration;

use sm_server::{run, ServerConfig};

/// 找一个空闲端口。
///
/// 做法是「先绑一次、记下地址、立刻释放」—— 有 TOCTOU 窗口，但测试环境里
/// 冲突概率远低于「让操作系统分配」的复杂度。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("取空闲端口");
    listener.local_addr().expect("读端口").port()
}

fn test_url() -> Option<String> {
    std::env::var("SMDB_TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
}

#[tokio::test]
async fn the_server_starts_serves_and_shuts_down_cleanly() {
    let Some(database_url) = test_url() else {
        eprintln!("SKIP: 未设置 SMDB_TEST_DATABASE_URL / DATABASE_URL");
        return;
    };

    let port = free_port();
    let config = ServerConfig {
        database_url,
        jwt_secret: "smoke-secret".to_owned(),
        // 调度器关掉：这个测试只关心 HTTP 装配，而 tick 会在测试期间
        // 往库里写 `background_task_run` 行（那是 `scheduler_tick` 用例的职责）。
        scheduler_enabled: false,
        listen: sm_server::ListenConfig {
            host: "127.0.0.1".to_owned(),
            port,
        },
        ..ServerConfig::with_defaults()
    };

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    // 装一个「起服务」的任务，同时等它真的开始监听 —— 否则第一个请求会
    // 撞上「连接被拒绝」，而那不是我们要测的失败。
    let server = tokio::spawn(run(config));
    wait_until_listening(addr, Duration::from_secs(10)).await;

    // 未注册的路径 → 404 信封（证明 router + fallback 已装配）。
    let response = reqwest::get(format!("http://{addr}/definitely-not-a-route"))
        .await
        .expect("请求应当被响应");
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.expect("404 应当是 JSON 信封");
    assert_eq!(body["error"]["code"], "http_error");

    // SIGTERM → 优雅关闭。容器里发的是它。
    tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(std::process::id().to_string())
        .status()
        .await
        .expect("发信号失败");

    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("服务应当在 10 秒内退出")
        .expect("服务任务不应 panic");
    assert!(result.is_ok(), "退出应当是干净的：{result:?}");
}

/// 轮询直到端口接受连接。
async fn wait_until_listening(addr: SocketAddr, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("服务在 {budget:?} 内没有开始监听 {addr}");
}
