# 插件作者指南

本仓的插件是**独立进程**（不是上游那种同进程 Python 包）：宿主拉起你的可执行文件，
你 serve 一个 gRPC 端口，宿主来连你。协议见
`docs/adr/2026-10-05-plugin-lifecycle.md`，契约面见 `docs/plugin-abi.md`。

**照着 `crates/plugin-ref-local` 抄最快** —— 它是全仓唯一被宿主真的拉起过、
走完 `Register` 的样本（`crates/plugin-ref-local/tests/lifecycle.rs`）。

## 1. 最小插件

```rust
// Cargo.toml: sm-plugin-api = { ... }
use sm_plugin_api::v1::plugin_control_server::{PluginControl, PluginControlServer};
use sm_plugin_api::v1::{RegisterRequest, RegisterResponse};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ★ 地址由宿主分配，不要自选端口。
    let addr: std::net::SocketAddr = std::env::var("SAKURAMEDIA_PLUGIN_GRPC_ADDR")?.parse()?;
    let plugin_id = std::env::var("SAKURAMEDIA_PLUGIN_ID")?;

    tonic::transport::Server::builder()
        .add_service(PluginControlServer::new(Control { plugin_id }))
        .serve(addr)
        .await?;
    Ok(())
}
```

`Register` 必须**原样回显**宿主注入的 `plugin_id`，并给出 `abi_major`：

```rust
#[tonic::async_trait]
impl PluginControl for Control {
    async fn register(
        &self,
        request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        let injected = request.into_inner().plugin_id;
        if injected != self.plugin_id {
            return Err(tonic::Status::invalid_argument("plugin_id 不一致"));
        }
        Ok(tonic::Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: "我的插件".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            ..Default::default()
        }))
    }
}
```

## 2. 三个扩展点

宿主只认三个 key（`sm-plugins/src/extensions.rs`）：

| key | 载荷 |
|---|---|
| `media.provider` | `MediaProviderBundle`（存储 / 下载 provider） |
| `catalog.metadata_source` | 元数据来源（`FetchMovie`） |
| `discovery.ranking_source` | 榜单来源（`FetchRanking`） |

声明"我提供了什么"，写在 `RegisterResponse.extensions` 里。

## 3. 写 provider：用默认实现层，别手写 37 个方法

生成的 `StorageProvider` trait 有 30+ 个方法。契约仓给了一层
**默认实现**：只实现你关心的那几个，其余自动返回 `Unimplemented`。

```rust
#[tonic::async_trait]
impl sm_plugin_api::StorageProviderExt for MyProvider {
    async fn browse(
        &self,
        request: tonic::Request<sm_plugin_api::v1::BrowseRequest>,
    ) -> Result<tonic::Response<sm_plugin_api::v1::BrowsePage>, tonic::Status> {
        // 只写这一个就够了
        todo!()
    }
}
```

⚠️ **不要声明没实现的能力**：`Capability::Download` 之类一旦声明，宿主就会以为
能调 `DownloadProvider`，而注册期**不会**校验它是否真的被 serve（这是已知缺口）。
只声明你 serve 了的 service。

## 4. 数据目录与交付目录

- `<SAKURAMEDIA_PLUGIN_DATA_DIR>`：宿主保证存在、可读写、**重装插件时保留**。
  缓存与状态放这里。
- 元数据图片放进 `<data_dir>/metadata-tmp/<请求目录>/`：宿主从
  `FetchMovieRequest.delivery_dir` 知道边界，并在你返回后**清理**它。
  判据（必须在目录内、必须是普通文件、必须再深一层、同一结果同一请求目录、
  `release_date` 严格 `YYYY-MM-DD`、`duration_minutes > 0`）由
  `sm_plugin_api::movie_delivery::validate_movie_delivery` 给出 ——
  **你自己的测试里就能跑同一套规则**。

## 5. 失败怎么报

用结构化错误，不要只给一个 gRPC 码：

```rust
use sm_plugin_api::v1::{ProviderError, ProviderErrorCode};

return Err(sm_plugin_api::error::to_status(
    &ProviderError {
        provider_key: "my-provider".to_owned(),
        operation: "scan_import_source".to_owned(),
        code: ProviderErrorCode::SourceNotFound as i32,
        // ★ 对外展示的文案：不要放 Cookie、密码或内部路径
        safe_message: "导入来源不存在".to_owned(),
        retryable: false,
    },
    tonic::Code::NotFound,
));
```

`code` 只有 7 个取值（`invalid_config` / `authentication_failed` /
`source_not_found` / `task_not_managed` / `source_blacklisted` / `unsupported` /
`unavailable`）。宿主按它分支：`source_not_found` 会让它继续清理本地记录，
`unavailable` 且 `retryable` 会让它稍后重试。

只给 `Status::not_found(...)` 的话，宿主只能按码**猜**（而同一个 gRPC 码也被
"媒体库不存在"用），`retryable` 则完全拿不到。

## 6. 回调宿主（可选）

宿主注入 `SAKURAMEDIA_HOST_GRPC_ADDR` 时，你可以连 `PluginHost` 查影片 / 演员：

```rust
let mut client =
    sm_plugin_api::v1::plugin_host_client::PluginHostClient::connect(host_addr).await?;
let movie = client
    .get_movie(sm_plugin_api::v1::GetMovieRequest { movie_id: 42 })
    .await?;
```

⚠️ 目前 36 个 rpc 里**只有 3 个接了线**：`GetMovie` / `FindMoviesByNumbers` /
`GetActor`，其余返回 `Unimplemented`。没有这个环境变量时**不要**去连 ——
那表示宿主这次没起这个服务。

## 7. 自检清单

- [ ] `Register` 原样回显 `plugin_id`，`abi_major` 用 `sm_plugin_api::ABI_MAJOR`
- [ ] 能力声明与**实际 serve 的 service** 一致（没实现的别声明）
- [ ] 只写 `<data_dir>` 之下的文件（临时交付物放 `metadata-tmp/<请求目录>/`）
- [ ] 失败走 `to_status`，`safe_message` 不含凭据与内部路径
- [ ] 没有 `SAKURAMEDIA_HOST_GRPC_ADDR` 时不回调宿主
- [ ] 崩溃会被宿主发现并**退避重启**（`sm-plugins/src/supervisor.rs`），
      所以启动要做成幂等的 —— 别假设"只会被拉起一次"
