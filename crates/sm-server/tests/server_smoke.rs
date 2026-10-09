//! 组合根的端到端冒烟：把**真实的 `sm-server` 二进制**起起来，打一次请求，
//! 再发 SIGTERM 验证优雅关闭。
//!
//! # 为什么跑子进程而不是在测试进程里 `run()`
//!
//! 第一版在测试进程内直接调 [`sm_server::run`]，然后给**自己**发 SIGTERM。
//! 那在 `cargo test -p sm-server` 下能过，但在 `cargo test --workspace` 下
//! 偶发把整次运行打断（signal 落在 harness 认为「还在跑」的那个二进制上，
//! 于是那次 workspace run 只报出几十个用例 + 1 个失败）。
//!
//! 改成子进程后有两个额外好处：
//!
//! - 验的是**真正的 `main()`**，包括配置加载与退出码映射 —— 上一版绕过了它们；
//! - 信号只发给子进程，测试进程不可能被自己杀掉，于是这个文件可以放多个
//!   测试（上一版必须在文件头警告「只能有一个」）。
//!
//! # 它能抓到的三类错
//!
//! | 错处 | 症状 |
//! |---|---|
//! | 路由 / 状态装错 | 请求进不来或 500 |
//! | 优雅关闭没接信号 | `docker stop` 每次等超时才被杀 |
//! | 配置读错 | 进程起不来（日志里有「配置错误」与退出码 2） |
//!
//! 端口用内核分配（先绑 0 拿端口再释放）避免与开发中的实例撞端口。

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

const STARTUP_BUDGET: Duration = Duration::from_secs(20);
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

fn test_url() -> Option<String> {
    std::env::var("SMDB_TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
}

/// 借一个空闲端口：先绑 0 读出端口，立刻释放。
///
/// 有 TOCTOU 窗口，但测试环境里冲突概率远低于「让操作系统在子进程里分配」
/// 的复杂度 —— 后者拿不到端口号，没法断言。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("取空闲端口");
    listener.local_addr().expect("读端口").port()
}

fn wait_until_listening(addr: SocketAddr, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务在 {budget:?} 内没有开始监听 {addr}");
}

#[tokio::test]
async fn the_binary_serves_the_error_envelope_and_exits_cleanly_on_sigterm() {
    let Some(database_url) = test_url() else {
        eprintln!("SKIP: 未设置 SMDB_TEST_DATABASE_URL / DATABASE_URL");
        return;
    };

    let port = free_port();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sm-server"))
        .env("SAKURAMEDIA_DATABASE_URL", &database_url)
        .env("SAKURAMEDIA_JWT_SECRET", "smoke-secret")
        .env("SAKURAMEDIA_HOST", "127.0.0.1")
        .env("SAKURAMEDIA_PORT", port.to_string())
        // 调度器关掉：这个测试只关心 HTTP 装配，而 tick 会在测试期间往库里
        // 写 `background_task_run` 行（那是 `scheduler_tick` 用例的职责）。
        .env("SAKURAMEDIA_SCHEDULER_ENABLED", "0")
        // 日志走 stderr：stdout 留空，管道读端不会被日志塞满而卡住子进程。
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 sm-server");

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_until_listening(addr, STARTUP_BUDGET);

    // 未注册的路径 → 404 信封（证明 router 与 fallback 已装配）。
    let response = reqwest::get(format!("http://{addr}/definitely-not-a-route"))
        .await
        .expect("请求应当被响应");
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.expect("404 应当是 JSON 信封");
    assert_eq!(body["error"]["code"], "http_error");
    assert!(
        body["error"]["message"].is_string(),
        "信封必须带 message：{body}"
    );

    // SIGTERM → 优雅关闭。容器里发的是它。
    let killed = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().expect("子进程有 pid").to_string())
        .status()
        .expect("发信号失败");
    assert!(killed.success(), "kill 应当成功");

    let status = tokio::time::timeout(SHUTDOWN_BUDGET, child.wait())
        .await
        .unwrap_or_else(|_| panic!("服务应当在 {SHUTDOWN_BUDGET:?} 内退出"))
        .expect("等待子进程结束");
    assert!(
        status.success(),
        "退出码应当是 0（SIGTERM 走的是优雅关闭路径，不是被信号杀掉）：{status}"
    );
}

#[tokio::test]
async fn a_missing_jwt_secret_refuses_to_start_with_exit_code_two() {
    // 组合根能犯的错误里，配置缺失最容易被「默认值兜住」而悄悄放过。
    // 退出码 2 与运行期失败（1）分开，便于容器编排区分「配置错」与「跑挂了」。
    let port = free_port();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sm-server"))
        .env("SAKURAMEDIA_DATABASE_URL", "postgresql://127.0.0.1:1/none")
        .env_remove("SAKURAMEDIA_JWT_SECRET")
        .env("SAKURAMEDIA_HOST", "127.0.0.1")
        .env("SAKURAMEDIA_PORT", port.to_string())
        .output()
        .await
        .expect("运行 sm-server");

    assert_eq!(output.status.code(), Some(2), "配置错误应当是退出码 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("jwt_secret") || stderr.contains("SAKURAMEDIA_JWT_SECRET"),
        "错误信息要指向缺失的那一项：{stderr}"
    );
}

#[tokio::test]
async fn an_invalid_port_is_rejected_before_binding() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sm-server"))
        .env("SAKURAMEDIA_DATABASE_URL", "postgresql://127.0.0.1:1/none")
        .env("SAKURAMEDIA_JWT_SECRET", "x")
        .env("SAKURAMEDIA_PORT", "not-a-port")
        .output()
        .await
        .expect("运行 sm-server");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("SAKURAMEDIA_PORT"));
}
