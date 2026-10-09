# 插件 ABI

`sm-plugin-api` 是**唯一**需要第三方插件依赖的 crate。本文档说明它的边界、
版本闸门，以及几个容易踩错的点。

## 三层与边界

| 层 | crate | 职责 | 第三方需要吗 |
|---|---|---|---|
| 契约 | `sm-plugin-api` | proto 生成物、ABI 常量、默认实现层、交付校验、错误码 | **是（唯一）** |
| 宿主实现 | `sm-plugins` | 进程管理、注册表、gRPC 调用面 | 否 |
| 装配 | `sm-server` | 拉起插件、注入环境变量、serve `PluginHost` | 否 |

判据：**双方都要遵守的规则**（交付校验、错误码、版本闸门）进契约仓；
**只有宿主做的事**（进程管理、注册表）留在 `sm-plugins`；**同时要用三方类型**
的映射放组合根。理由见 `docs/adr/2026-10-06-plugin-architecture.md`。

## 与上游（Python）的关系：**照抄语义，不照抄机制**

上游是**同进程 Python 包**（包根 `register(context)`，全仓 0 个 `.proto`）；
本仓是**独立进程 + gRPC**（`docs/adr/2026-10-05-plugin-lifecycle.md`）。
所以：

- ✅ 照抄：三个扩展点、能力探测、数据目录与 `metadata-tmp` 交付约定、
  `ProviderOperationError` 的 7 个码、坏插件隔离（不 fail-fast）。
- ❌ 不照抄：`import` 机制、`HOST_API_VERSION`（proto 侧改用 `ABI_MAJOR`）、
  异常（改成 gRPC `Status`）。

## 版本闸门

`ABI_MAJOR`（`src/lib.rs`）。宿主在 `Register` 时校验回显值，不一致即**拒绝加载**
（`sm-plugins/src/registration.rs`）。不兼容变更递增它；**不要**为了兼容旧插件
而跳过校验 —— 旧插件会以"看起来能跑"的方式产生错误数据。

## 进程协议（速查）

宿主拉起插件时注入（`sm-plugins/src/supervisor.rs`）：

| 环境变量 | 必给 | 含义 |
|---|---|---|
| `SAKURAMEDIA_PLUGIN_GRPC_ADDR` | 是 | 插件要 bind 的地址（**宿主分配**，插件不可自选） |
| `SAKURAMEDIA_PLUGIN_ID` | 是 | 宿主注入的 id，`Register` 必须**原样回显** |
| `SAKURAMEDIA_PLUGIN_DATA_DIR` | 是 | 数据目录，宿主保证可读写、重装插件时保留 |
| `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` | 否 | 宿主写好的配置文件（插件声明了 settings 才有） |
| `SAKURAMEDIA_HOST_GRPC_ADDR` | 否 | 宿主能力出口（`PluginHost`）。**没有 = 这次不能回调宿主** |

## 错误怎么过线

`ProviderError`（`proto/common.proto:322`）经 `Status` 的 `details` 传递：

- 插件侧：`sm_plugin_api::error::to_status(&error, grpc_code)`
- 宿主侧：`sm_plugin_api::error::from_status(&status)` —— 解不出返回 `None`，
  宿主回落按 gRPC 码猜（**老插件与手写 `Status` 的插件走这条路**）。

`code` 只有 7 个取值（另有 `UNSPECIFIED` 兜底）；`retryable` 是**独立字段**，
不是从 `code` 推出来的。

## 元数据交付

插件把图片放进 `<data_dir>/metadata-tmp/<请求目录>/`，宿主从
`FetchMovieRequest.delivery_dir` 知道边界，并用
`sm_plugin_api::movie_delivery::validate_movie_delivery` 校验：

- 图片必须在交付目录内（挡 `../`），且是普通文件；
- 必须再深一层（`<delivery_dir>/<请求目录>/<文件>`）；
- 同一结果的图片必须来自同一请求目录；
- `release_date` 严格 `YYYY-MM-DD`；`duration_minutes > 0`。

这套规则住在**契约仓**，所以你能在自己的测试里直接验一遍。

## 已知缺口（贡献前请先看）

- `PluginHost`（`proto/host.proto`）36 个 rpc 里**只有 3 个接了线**
  （`GetMovie` / `FindMoviesByNumbers` / `GetActor`），其余返回
  `Unimplemented`。缺口表见 `crates/sm-server/src/plugin_host.rs` 模块文档。
- 注册期**不**校验「声明的能力 ↔ 是否 serve 了对应 service」——
  所以**虚报能力不会被发现**，请勿声明没实现的东西。
- 契约仓还没拆成独立 git 仓库（`docs/plugin-api-split.md`），
  第三方暂时按 workspace 路径依赖。
