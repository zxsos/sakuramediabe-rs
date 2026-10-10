# Provider 缝的扩展：按能力分 trait，能力缺失独立成码

- 日期：2026-10-08
- 状态：**已决定，落地中**（第一刀：播放计划）
- 相关：`docs/adr/2026-10-06-plugin-architecture.md`（插件架构总纲）、
  `docs/adr/2026-10-05-plugin-lifecycle.md`（进程模型）、
  `docs/plugin-abi.md`（给插件作者）、`docs/handoff.md` §7.3

## 0. 这份 ADR 解决什么

插件 ABI 本身**不需要设计**：gRPC + 独立进程的架构在 `2026-10-06-plugin-architecture.md`
已定、P0–P4 已落地；`proto/storage.proto` 里播放、浏览、导入暂存、指纹、探测、
空间、转存源/目标**全都有消息定义**（`PlaybackPlan` 的 `oneof` 就在
`storage.proto:41-52`）。

但 `docs/handoff.md` §7.3 把 **~32 条 `todo!()`** 归因于「缺插件 provider ABI」。
核过之后的真实归因是：**宿主侧的「缝」（seam）没接满**，不是缺设计。

现有这根缝的模式记在 `sm-server/src/provider_gateway.rs:1-29`：

| 层 | 谁 | 做什么 |
|---|---|---|
| 业务层 | `sm-service` | 声明窄 trait（现在只有 `StorageGateway`，3 个方法）|
| 装配 | `sm-server` | 实现它，把宿主类型接到 `sm-plugins` 的调用面 |
| 宿主实现 | `sm-plugins` | 发 gRPC、把 `tonic::Status` 归类成上游七码 |

`sm-service` **不能**依赖 `sm-plugins`（`sm-plugins → sm-scheduler → sm-service`
成环），所以 trait 只声明能力、实现只能落在组合根 —— 这是依赖倒置的接线点。

本 ADR 只定**怎么把这个模式扩展到剩下五类能力**。

## 1. D1：按**能力**分 trait，不做一个 fat `StorageGateway`

**决定**：新增兄弟 trait（`PlaybackGateway` / `ImportGateway` / `TransferGateway`
/ `ProbeGateway` / `DownloadGateway`），而不是把 `StorageGateway` 长到十几个方法。

**理由**：上游的可选能力就是 `supports_*` 鸭子探测
（`provider_protocol.py:560-601`，见 `2026-10-06` ADR §1 的核实清单），业务层
**本来就要按「这个 provider 支不支持」分支** —— 例如转存候选在缺目标能力时给
`blocked_reason`，**不是** 503；`blocks` 是业务概念，不该由 trait 的缺失来表达。

fat trait 的另一个代价是测试替身：`tests/support/mod.rs:134` 的 `NoopGateway`
已经在 `generate_thumbnails` 上写 `unimplemented!()`。方法越多，这个 panic 面
越大 —— 而它恰好把「不支持」编成了 panic，掩盖了本该分支的情况。

**代价**：`AppState` 多几个字段、组合根多几次 `with_*`。可接受 —— 实现者仍是
**同一个** `ProviderGateway`，只是多 impl 几个 trait（它的构造与「活的注册表」
纪律不变）。

## 2. D2：能力缺失走**上游既有的** `unsupported`，不新造字面量

**决定**：能力缺失用 `unsupported` 表达（`ProviderFailure.code == "unsupported"`，
`retryable = false`）。**不**新增 `provider_not_supported` 之类的码。

> 初稿这里写的是「新增 `provider_not_supported`」。那是在读
> `provider_calls.rs:20-21` 之前写的 —— 上游七码里**本来就有 `unsupported`**，
> 再发明一个会让调用方要同时认识两套码。**照抄语义，就别改字面量。**

三者的分工（这是本决定的实质内容）：

| 码 | 含义 | `retryable` | 调用方该做什么 |
|---|---|---|---|
| `unsupported` | provider 在，但不做这件事 | `false` | **换行为**（跳过 / `blocked_reason` / 拒绝）|
| `unavailable` | 暂时不可达 | 通常 `true` | 退避重试 |
| `provider_not_installed` | **宿主**侧：插件没装（非上游码，`provider_gateway.rs:47`）| `false` | 503，提示去装插件 |

**理由**：这三件事走**不同分支**。全塞进 503 的后果是调用方只能去重试一个
永远不会成功的请求，且响应里**没有任何东西**说明「重试没有意义」。

## 3. D3：不信任能力声明，首次 `Unimplemented` 时回落

**决定**：注册期**不**做「声明的能力 ↔ 真的 serve 了」全量校验
（`docs/plugin-abi.md:76-77` 记着这是已知缺口）；改为**首次调用**收到
`Unimplemented` 时回落成 D2 那个 `unsupported` 码并 `warn`（每个
`(provider_key, capability)` 只警告一次）。

**理由**：全量校验要在加载期对每个可选能力发探测请求（或解析服务列表），启动
成本与复杂度都不小；而「声明错了」的真实后果就是**某一次调用失败** —— 在那一刻
判定既准确又免费。这也与上游靠鸭子类型探测的精神一致。

## 4. 落地顺序

| 序 | 一刀 | 收掉 |
|---|---|---|
| 1 | 播放计划（`PlaybackGateway`）| `media_playback.rs` 3 + `videos.rs` 3 |
| 2 | 下载器 | `download_tasks.rs` 2 + `transfers/download_*` 7 |
| 3 | 转存源/目标 | `media_transfer.rs` 2 + `media_transfer_task` 4 |
| 4 | 浏览 / 导入暂存 | `media_import.rs` 2 + `provider_browse`/`import_task` 3 |
| 5 | 探测（duration / resolution / video_info / hash / space）| `playback` 3 个 backfill |

选 1 先做的理由：proto 已设计完、不碰 DB、能一次收掉路由里最大的一片，且它是
验证 D1/D2 形状的**最小成本实验**。

## 5. 反选与代价

- **为什么不把 `StorageGateway` 做成 fat trait？** 见 D1：会把「不支持」变成
  `unimplemented!()`，且与上游的 `supports_*` 语义对不上。
- **为什么不让 `sm-service` 直接依赖 `sm-plugins`？** `2026-10-06` ADR §9 已答：
  会把业务逻辑与进程/gRPC 传输绑死，任何 provider 调用都要拉起整个插件栈才能单测。
- **为什么不把能力缺失也报 503？** 见 D2。

## 6. 尚未核实的（别当已定）

- `sm-plugins/src/provider_calls.rs` 的现有调用面到底包了 12 个 storage 方法里的
  哪些 —— 这决定「只差接线」的真实工作量。
- `PlaybackContext` 是否还残留跨进程的宿主回调（`storage.proto:12-24` 的注释说它
  刻意避开了返回 `Response`，但没读到全文）。
- `sm-server/src/plugin_host.rs` 的 36 rpc 缺口表（目前 3 个已接）。
