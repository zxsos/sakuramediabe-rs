# 插件架构：契约仓承载契约，组合根拆环

- 日期：2026-10-06
- 状态：**P0–P4 已落地**（P0 `22ea79e`，P1–P4 同批）；P5（拆仓）待契约面稳定后再做
- 相关：`docs/adr/2026-10-05-plugin-lifecycle.md`（进程模型）、`docs/plugin-api-split.md`（拆仓计划）、`docs/adr/2026-10-04-tech-selection.md` §6（幽灵配置）、`docs/plugin-abi.md`（给插件作者）、`docs/plugin-author-guide.md`（作者指南）

### 落地情况

| | 内容 | 落点 |
|---|---|---|
| P0 | 拆掉 `sm-plugins → sm-scheduler` | `sm_server::plugins::job_specs` |
| P1 | 交付校验进契约仓 | `sm_plugin_api::movie_delivery`（测试随迁） |
| P2 | `ProviderError` 过线 | `sm_plugin_api::error::{to_status,from_status}`，`classify_status` 先试结构化 |
| P3 | `PluginHost` 接线 | `sm_server::plugin_host`（3/36 已接，其余登记在模块文档）+ `SAKURAMEDIA_HOST_GRPC_ADDR` |
| P4 | 贡献者物料 | 修掉参考插件虚报的 Download 能力；补 `docs/plugin-abi.md` 与 `docs/plugin-author-guide.md` |

P3 只接了 3 个 rpc 是**刻意的**：剩下 33 个大多是写操作（要走主权网关 / 业务
service），只读快照错了最多是插件拿到空值，写操作错了会改坏用户数据。按组逐个接。

## 1. 前提：上游是同进程 Python 包，本仓是 gRPC 子进程 —— 不照抄机制，照抄语义

已核实的**上游事实**（`upstream/sakuramediabe`）：

- 插件是**同进程 Python 包**：`<root>/<plugin_id>/{manifest.json,__init__.py}`，包根 `register(context) -> PluginRegistration`（`src/plugins/loader.py:204`）。
- 全仓 **0 个 `.proto`**；`grpcio` 只是 `qdrant-client` 的依赖（`requirements.txt:50-55`）。
- **没有 capabilities 声明**：能力靠鸭子类型 `supports_*` 探测（`src/plugins/provider_protocol.py:560-601`）。
- 扩展点只有三个：`catalog.metadata_source`、`discovery.ranking_source`、`media.provider`（`src/plugins/extensions/__init__.py:27-31`）。
- 契约闸门是 `HOST_API_VERSION = 9`（`src/plugins/contracts.py:20`），不是 proto 版本。
- 数据目录 `<root>/<plugin_id>/data` 宿主托管；交付目录 `data/metadata-tmp/<请求目录>/` 插件建、宿主用后即删。
- 运行期错误是 `ProviderOperationError`，**code 只有 7 个取值**（`provider_protocol.py:287-295`），调用方按它分支（`source_not_found` → 继续，`unavailable` → 重试）。

而 `docs/adr/2026-10-05-plugin-lifecycle.md` 已经定了本仓走**独立进程 + gRPC**（端口宿主分配、env 注入、探活、崩溃重启）。这是**刻意且正确的偏离**：Go/Rust 生态里进程隔离比 `import` 一个第三方包安全得多，也没法让第三方代码进宿主进程。

所以本 ADR 的原则是：**机制用 gRPC，语义照抄上游**。具体来说，下面这份清单是"Rust 侧实现到什么程度算对齐"的判据：

| 上游语义 | 本仓对应 | 现状 |
|---|---|---|
| 三个扩展点 | `extensions.rs` 的校验 + `extension_calls.rs` 的调用面 | ✅ 已落 |
| `supports_*` 能力探测 | proto 的 `Capability` 声明 + 注册期校验 | ⚠️ 声明有了，运行时"声明↔实现"未校验 |
| `PluginContext`（宿主能力出口） | `proto/host.proto` 的 `PluginHost`（28 rpc） | ❌ 生成后全仓 0 引用 |
| `data/metadata-tmp` 交付约定 | `movie_delivery.rs`（7 类判据 + 清理） | ✅ 已落（但在 sm-plugins 而非契约仓） |
| `ProviderOperationError` 的 7 个 code | `provider_calls.rs` 的 `classify_status` | ❌ **过不了线**（自陈缺口 `provider_calls.rs:14`） |
| 坏插件隔离（`PLUGIN_LOAD_ERRORS`） | 加载期 `warn` + 跳过 | ✅ 已落 |
| 配置四键 `root_dir/enabled/job_crons/settings` | `sm-core::config_schema` 逐字段对齐 | ✅ 已落 |

## 2. 分层决策

```
sm-plugin-api   ← 唯一可发布的契约仓（第三方插件只依赖它）
  ├ proto 生成物（common/storage/plugin/host）+ ABI_MAJOR
  ├ 默认实现层 StorageProviderExt / DownloadProviderExt（37 个方法都有默认体）
  ├ 交付校验规则 + 错误码表            ← P1/P2 要迁进来
  └ （可选）宿主能力出口的客户端        ← P3
      ▲                ▲                 ▲
  sm-service        sm-plugins         sm-server
```

1. **`sm-service` 只依赖契约（`sm-plugin-api`），不依赖实现（`sm-plugins`）。** 契约仓是叶子（不依赖本仓任何 crate），加这条边不成环。业务层因此可以校验交付、判错误码，而不需要组合根为每一样能力写一层胶水。
2. **组合根（`sm-server`）是唯一同时看得见「插件 / 调度器 / 配置」三方又不产生环的位置。** 凡是要同时用到三方类型的映射，都放这里。
3. **`sm-plugins` 只做宿主实现**（进程管理、gRPC 客户端、注册表），不反向依赖调度器或业务层。

## 3. P0（已落地）：拆掉 `sm-plugins → sm-scheduler`

**问题**：生产路径用的是 `sm-server/src/plugins.rs` 自己那版 `scheduler_specs()`（它支持 `cron_override`），而 `sm_plugins::scheduling::scheduler_specs` 只被 `sm-plugins/tests/plugin_job_cron.rs` 引用 —— 整条依赖边只为借一个 `JobSpec` DTO，却把 `sm-service` 锁进了环的下游：

```
sm-plugins → sm-scheduler → sm-service → (sm-plugins)   ← 成环
```

**改动**：

- 映射抽成 `sm_server::plugins::job_specs(registry, cron_override)`（`pub`，带三条单测，含"配置 cron 覆盖优先"这一条）；`Plugins::scheduler_specs()` 委托给它。
- 删 `crates/sm-plugins/src/scheduling.rs` 与其 `sm-scheduler` 依赖，并在 `sm-plugins/Cargo.toml` 里写下"这条边为什么不能再加回来"。
- `tests/plugin_job_cron.rs` 迁到 `crates/sm-server/tests/`（它同时要用插件注册表、调度器与组合根的映射）。

**收益**：环的一半消失；`sm-service → sm-plugins` 从此**可选**（不再被结构禁止）；死代码与其"组合根还没接上"的过期注释一并清理。

## 4. P1：交付校验迁进契约仓

`movie_delivery.rs`（判据 + `MovieDelivery` + `cleanup_delivery` + 8 条测试）从 `sm-plugins` 迁到 `sm-plugin-api`。

理由：那七条判据是**双方都要遵守的契约**（插件照它放文件，宿主照它验），而不是宿主的实现细节。放契约仓之后：(a) 插件作者能在自己的测试里用同一套规则自检；(b) `sm-service` 能直接校验交付，不必绕组合根。

连带闭合：番号一致性校验现在"故意不在本模块"（`movie_delivery.rs:27-32`，因为不能依赖 `sm-service`）。迁进契约仓后仍不在那里 —— 番号归一化属于业务概念，留在 `sm_service::movie_numbers`，由调用方比。

## 5. P2：错误码过线（闭合 `provider_calls.rs:14` 的自陈缺口）

上游 7 个 code 是调用方分支的依据，现在只能从 gRPC status 猜。做法：

- `proto/common.proto` 加 `ProviderError { provider_key, operation, code, safe_message }`（`code` 用 enum，含 `UNSPECIFIED` 兜底）；
- 契约仓给 `to_status()` / `from_status()`（用 `tonic::Status::with_details` + prost 编解码，**不改现有 rpc 签名**）；
- `sm-plugins::provider_calls::classify_status` 改为先试 `from_status`，解不出再回落按 status 猜；
- `retryable` 由契约仓的码表给，不由宿主猜。

## 6. P3：接上 `PluginHost`（28 rpc）

这是"把插件 API 留出来让别人贡献"缺的那一块 —— 上游插件作者真正依赖的是 `PluginContext`（`actors` / `movies` / `media` / `downloads` / `imports` / `subscriptions` / `notifications`）。proto 已生成，宿主侧无人实现。至少先把 `movies` / `actors` / `media` / `downloads` / `imports` 五个接出来。

## 7. P4：贡献者物料

- 修 `plugin-ref-local/src/bin/plugin-ref-local.rs:100`：它声明了 `Capability::Download` 但全 crate 无 `DownloadProvider` 实现 —— 照抄它的贡献者会被误导。
- 补 `docs/plugin-abi.md`（`sm-plugin-api/src/lib.rs:14` 已经引用了它，但文件不存在）。
- 补 `docs/plugin-author-guide.md`：最小插件长什么样、三个扩展点怎么声明、数据目录与交付目录约定、错误怎么报、宿主能力怎么用。
- 顺带补 P3 落地后 `plugin-ref-local` 里对应的最小实现。

## 8. P5：契约仓独立成 `sakuramedia-plugin-api`

按 `docs/plugin-api-split.md`：拆成独立仓库、按 tag 发布，第三方插件锁版本依赖。前提是 P1/P2 之后契约面稳定。**在此之前不要拆** —— 每加一条契约都要同步两个仓库。

## 9. 反选与代价

- **为什么不干脆让 `sm-service` 依赖 `sm-plugins`？** P0 之后这条边合法了，但它会把"业务逻辑"与"进程/gRPC 传输"绑在一起：任何一个 provider 调用都要拉起整个插件栈才能单测。上游的业务 service 确实是直接 import 注册表的，但那是因为 Python 里两者同进程 —— Rust 侧有更好的选择。
- **为什么映射不留在 `sm-plugins`（把 `JobSpec` 下沉到 `sm-core` 也行）？** 也行，但 `JobSpec` 是调度器的类型，下沉到 core 会让"调度声明"变成全局概念；而且 `cron_override` 读的是组合根的配置。放组合根是三者里改动最小、语义最直白的。
- **代价**：组合根会多承担若干"映射"职责。可接受 —— 它本来就是唯一能同时看见三方的地方，而这类映射是纯函数、好测。
