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

## 字节怎么过线：三种投递

大文件**不走 gRPC 消息**，只走数据面。插件在 `Register` 里声明
`data_plane_endpoint`（`proto/plugin.proto:63`）—— 那是它自己起的 HTTP 服务，与
gRPC 控制面端口**是两个**。宿主按插件的 `PlaybackPlan`（`proto/storage.proto`）里
那三种 `oneof delivery` 选一种：

| 计划 | 谁持有字节 | 宿主做什么 | 什么时候用 |
|---|---|---|---|
| `redirect(url)` | 远端存储 | 302 + 计划自带的头 | 支持直链的网盘（115 的直链） |
| `proxy(endpoint, path_prefix)` | 远端 HTTP 源（经插件） | 转发到插件的 HTTP 端点，`Range` 原样透传 | 存储不给自己签直链 |
| `local_path(path)` | **宿主本机的文件系统** | 宿主自己 `open` 这个路径、自己算 206/416、自己定 `Content-Type`（provider 声明了就用它的） | 本地库 / 挂载盘 / 与宿主同机 |

`local_path` 给的是**路径不是 URL**：provider 不做 URL 转义，宿主也不做反转义，
路径一个字节都别改。拼一个 `file://` 交给客户端是错的 —— 浏览器不认那种 scheme，
表现是「点了没反应」，而宿主侧看起来一切正常。

### ★ 字节搬运必须走 `data_plane_endpoint`

反例（会静默丢功能）：把文件内容塞进某个 rpc 的 `bytes` 字段。它与本 ABI 的三条
前提同时冲突：

1. gRPC 默认消息上限 4 MiB，而一部片子是 GB 级 —— 要么抬上限（一次无上限的内存
   分配），要么自己分块（那就是手搓一个更差的 HTTP）；
2. 控制面是一问一答、没有背压的：拖进度条要能随时掐断，塞在 `bytes` 里做不到
   「客户端断开就停」；
3. `Accept-Ranges` / `Content-Range` / `206` 这套语义过一遍 gRPC 全丢了，宿主还得
   重算一遍 —— 而 Range 在本仓只在 `sm-api/src/range.rs` 写了一次。

所以：**小产物**（缩略图写进宿主给的 workspace、图片走交付目录）走文件系统；
**影片字节**一律走 302 或数据面。

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

- `PluginHost`（`proto/host.proto`）37 个 rpc 里**接了 11 个**
  （`GetMovie` / `FindMoviesByNumbers` / `ListMovies` / `PatchMovie` /
  `GetActor` / `ListActors` / `PatchActor` / `ImportSubtitle` /
  `GetJavdbRankNumbers` / `SyncRankingSources` / `SyncRankingBoard`），
  其余返回 `Unimplemented`。缺口表见
  `crates/sm-server/src/plugin_host.rs` 模块文档。
- 注册期**不**校验「声明的能力 ↔ 是否 serve 了对应 service」——
  所以**虚报能力不会被发现**，请勿声明没实现的东西。
- **写操作的身份是「端点」而不是请求字段**：`PatchMovieRequest` 里没有
  `plugin_id`，而写入口要带 `owner = plugin:{id}`（`sm-db/src/repo/gateway.rs`
  的 `patch_plugin`）。所以宿主为**每个启用的插件各起一个** `PluginHost`
  （`sm-server/src/plugin_host.rs` 的 `serve_for`），插件连的那个端点定义了它是
  谁 —— 宿主是**分配**身份，不是**相信**声明。往请求里加字段那条路要动契约仓与
  版本闸门，留到「真要跨进程复用同一个端点」时再说。
- **`MovieSnapshot.owners` 是去重后的 owner 列表**（「谁动过这行」），不是
  「字段 → owner」。要判「某个字段归谁」的插件拿不到字段级信息 —— 那件事由
  写入口（主权网关）兜住，不靠插件读快照。值的这一侧已经给全：快照带上
  `PROTECTED_MOVIE_FIELDS` 的**全部 6 个字段**（含 `is_collection` /
  `is_blacklisted`），判据是「**可写就得可读**」。
- 契约仓还没拆成独立 git 仓库（`docs/plugin-api-split.md`），
  第三方暂时按 workspace 路径依赖。
