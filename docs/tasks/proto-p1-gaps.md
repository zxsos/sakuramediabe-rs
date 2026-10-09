# 任务：proto P1 缺口处置（P0 契约分叉 + P1-2 决策 + 收尾）

**背景**：`docs/parallel/grpc-plugin-report.md` §4 列了 P1-1 ~ P1-4 四个缺口，建议
「在合 SMA 之前谈定」。本文件把**当前实际状态**核对清楚，并给出剩余动作。

**触发它的那轮结论要修正一处**：上轮 `handoff.md` §八 写的是「proto 三个缺口决策」，
核对后发现 **P1-1 / P1-3 / P1-4 都已经在 `sakuramediabe-rs` 里落地了** ——
真正待做的不是「决策三个」，而是「**修一张已经分叉的契约**」+「决策 P1-2」。

---

## 零、执行状态（2026-10-07）

| 项 | 状态 |
|---|---|
| P0 契约仓同步（`proto/*.proto` + `src/*.rs` 共 9 个文件） | ✅ 已提交并**推送**（`cargo check --all-targets` 绿） |
| `ABI_MAJOR` 1 → 2（两仓同时） | ✅ 已做（`sm-plugins` / `sm-plugin-api` 测试 76 项全绿） |
| 契约仓 tag `v0.2.0` | ✅ **已推送**（GitHub，指向 `1b1edbf`） |
| 防漂移门禁 `parity/check_contract_sync.py`（接进 `verify.ps1`） | ✅ 已做（人为制造两种漂移验证过会红） |
| 插件改 `tag = "v0.2.0"` + `plugin-ref-local` 补 `done` 帧 | ✅ **已做**（两插件在 `v0.2.0` 下 `cargo test` 绿） |
| P1-2 决策 | ⬜ 待拍板（§三 已给精确 diff） |

> **推送已解决（2026-10-08）**：契约仓改放到 **GitHub** ——
> [`zxsos/sakuramedia-plugin-api`](https://github.com/zxsos/sakuramedia-plugin-api)
> （public；`main` + `v0.1.0` + `v0.2.0` 已推）。本机原来那个 `cnb.cool` 远端
> **已从仓配置里删除**（对它提交也没有可用凭据：
> `fatal: could not read Username for 'https://cnb.cool'`）；
> cnb 的**服务端那份仍在**、且匿名可读（`git ls-remote` 不需要凭据），
> 需要时 `git remote add cnb <url>` 即可加回来 —— **发布源已改为 GitHub**。
>
> 所以 §2.4 的第 4、5 步已做完，不再是「等推送」。

---

## 一、现状核对（逐条带证据）

| 缺口 | 状态 | 证据 |
|---|---|---|
| **P1-1** `GenerateThumbnails` 产物清单没通道 | ✅ **本仓已落地**（提交 `99671f1`） | `proto/storage.proto`：rpc 改为 `returns (stream GenerateThumbnailsResponse)`，消息含 `oneof payload { progress = 1; done = 2; }`；`crates/sm-plugin-api/src/provider.rs:154-165` 默认体返回 `GenerateThumbnailsResponse` 流；`crates/sm-plugins/src/provider_calls.rs:551-618` 宿主侧消费并**把「收不到 `done`」判为 provider 违约** |
| **P1-2** `PlaybackPlan` 缺本地路径 delivery | ❌ **未做** | `proto/storage.proto:41-52` 的 `oneof delivery` 仍只有 `redirect = 1` / `proxy = 2`（**编号 3 空闲**）；对照同文件 `OpenCoverSourceResponse` 却有 `oneof { local_path, url }` |
| **P1-3** 失败只有一个布尔位 | ✅ **本仓已落地**（方案 A） | `crates/sm-plugin-api/src/error.rs`：`to_status` / `from_status` 把 `ProviderError` 编进 `Status::details`（含往返无损单测）；`crates/sm-plugins/src/provider_calls.rs:14-44` 模块文档标题即「✅ 已闭合的 ABI 缺口」；`classify_status` 先解结构、解不出按 gRPC 码猜 |
| **P1-4** tonic 不生成默认方法体 | ✅ **已落地** | `StorageProviderExt` / `DownloadProviderExt`（`sm-plugin-api/src/provider.rs`），`plugin-ref-local` 已迁移（少约 170 行 stub） |

> 所以报告 §5「给主线的下一步建议」里的四条，实际只剩第 1 条的一半（P1-2）
> 与第 4 条（`data_plane_endpoint` 写成硬性要求）没做。

---

## 二、P0：两张契约已经分叉（**不依赖任何决策，先修**）

### 2.1 分叉事实

| | 宿主侧 | 插件侧 |
|---|---|---|
| 来源 | 本仓 `crates/sm-plugin-api` + `proto/` | 契约仓 `sakuramedia-plugin-api` **tag `v0.1.0`**（分叉时的值） |
| `proto/` | `storage.proto` **已含 P1-1**（15888 B） | `storage.proto` 旧版（14963 B） |
| `src/` | **5 个模块**：`lib` / `provider` / `error` / `json_struct` / `movie_delivery` | **2 个**：`lib` / `provider` |
| `ABI_MAJOR` | `1` | `1` |

`proto/` 逐文件比对：`common` / `plugin` / `host` **一致**，**只有 `storage.proto` 不同**
（`git diff --no-index` 结果：20 insertions / 3 deletions，正是 P1-1 的改动）。

两个插件都按 git tag 依赖契约仓：

```toml
# sakuramedia-plugin-ref-local/Cargo.toml / sakuramedia-javbus-metadata/Cargo.toml
sm-plugin-api = { git = "https://cnb.cool/zxsos1/sakuramedia-plugin-api.git", tag = "v0.1.0" }
```

> 上面是**分叉当时**的地址与 tag。发布源现已改为 GitHub，插件已改指
> `tag = "v0.2.0"`（见 §零）。

### 2.2 后果：**一个指错方向的解码错误**

`GenerateThumbnails` 在两侧的**方法名与路径相同**（`/sakuramedia.v1.StorageProvider/GenerateThumbnails`），
但消息类型不同：

```text
插件按旧 proto 发：  ProgressEvent { text=1(string,LEN), current=2(int32,VARINT), total=3(int32,VARINT) }
宿主按新 proto 解：  GenerateThumbnailsResponse { oneof payload { progress=1(LEN), done=2(LEN) } }
```

逐字段看：`field 1` 两侧都是 `LEN`，但旧版是 **string**、新版是**嵌套 message** ——
宿主会把那句进度文本当作一个 `ProgressEvent` 去解；`field 2` 旧版是 `VARINT`、
新版要 `LEN` —— **wire type 不匹配，直接解码失败**。

于是表现是：**宿主收到一个解码错误**，经 `classify_status` 归成
`ProviderErrorCode::Unspecified`，对外文案是「媒体提供方操作失败」。真实原因
（两仓 proto 不同步）在日志里**一个字都看不到**。

> 严格说它不会「静默写错数据」（wire type 那一步会拦住），但归因成本极高 ——
> 这正是 `grpc-plugin-report.md` §5.3 说的那类问题：「宿主在狂欢期不得不开外挂分支」。

### 2.3 为什么现在**没有**任何东西会拦住它

1. **`ABI_MAJOR` 两边都是 1** → `validate_registration` 的「必须完全相等」检查通过
   （`crates/sm-plugins/src/registration.rs:127-133`）；
2. **插件自测是两端同版本**（自己起 server、自己当 client，都用 v0.1.0）→
   `plugin-ref-local/tests/lifecycle.rs` 全绿；
3. **宿主侧没有跨仓集成测试** —— 两个插件的 README 都写着「宿主侧的生命周期集成
   测试**不在本仓库**，日后由后端以 `[dev-dependencies]` 按 tag 引入本仓库来跑」。

第 3 条是根因：**唯一能发现分叉的测试还没写**。

### 2.4 修复步骤与**执行状态**

| # | 动作 | 状态 | 判据 |
|---|---|---|---|
| 1 | 把本仓 `proto/*.proto` 与 `crates/sm-plugin-api/src/*.rs` 同步到契约仓 | ✅ **已做并推送**（契约仓提交 `1b1edbf`） | `parity/check_contract_sync.py` 报 9 个受管文件一致 |
| 2 | **`ABI_MAJOR` 1 → 2**（两仓同时） | ✅ **已做** | 两边 `lib.rs` 都是 `pub const ABI_MAJOR: i32 = 2;` |
| 3 | 契约仓打 tag **`v0.2.0`** | ✅ **已推送**（GitHub） | 远端有 `v0.2.0` |
| 4 | 两个插件的 `Cargo.toml` 改 `tag = "v0.2.0"` | ✅ **已做** | 两插件 `cargo test` 绿（`ref-local` 13 项 / `javbus` 33 项） |
| 5 | `plugin-ref-local` 的 `generate_thumbnails` 改成发 `done` 帧 | ✅ **已做**（宿主内置副本早已实现，按它镜像过去；`GAP:` 注释已删） | 流末帧是 `done`；`tests/roundtrip.rs` 断言 `payload` 必须在 |
| 6 | ~~跨仓集成测试~~ → **改为 `parity/check_contract_sync.py`** | ✅ **已做**（并接进 `verify.ps1`） | 见下 |

**第 3~5 步原卡在「契约仓推不出去」，现已随改放 GitHub 解开**（见 §零）。两个插件
按 GitHub 上的 `v0.2.0` 重新编译、自带测试全绿 —— 这同时是「两仓契约确实一致」的
一次端到端检验：`check_contract_sync.py` 只比字节，它多证明了一件事 ——
「那个 tag 拉得下来，而且按它编得过」。

> ✅ **第 2 步可以现在就生效的原因**：宿主与契约仓**同时**改成了 2。
> 唯一还在用旧契约的是「按 `v0.1.0` 编译出来的插件二进制」，而
> `ABI_MAJOR` 递增正是要拒掉它们 —— 拒掉比「能注册但调用时报解码错」好。
> 现在没有生产链路依赖这两个插件（`javbus` 的入库路径还没写，见 §3 之外的
> `tasks/javbus-metadata.md` §二），所以代价接近零。

**为什么第 2 步要 bump**（而不是「两边同时升就行」）：

- 这次改动**定义上就是不兼容变更** —— rpc 签名变了。proto 注释与
  `registration.rs:18` 都写着「不兼容变更时递增，宿主据此拒绝加载旧插件」。
- 边际成本≈0：**插件本来就必须重新编译**（否则拿不到新 proto），改 tag 与 bump 是同一件事的两面。
- 收益是唯一的那种：下次**有人只升一边**时，能被拦住而不是排查一个解码错误。

### 2.5 关于第 6 步：为什么用**静态检查**替代（而不是取消）

原计划是「宿主以 `[dev-dependencies]` 按 tag 引入插件、真拉起、跑一次
`GenerateThumbnails`」—— 那是「防止再发生」的手段。它有现实阻力：
需要网络拉 git tag、需要凭据，而且会让每次 `verify` 慢一个数量级。

改成静态检查是**有取舍的**，两者抓的东西不同：

| | `check_contract_sync.py`（已做） | 跨仓集成测试（未做） |
|---|---|---|
| 抓什么 | **源码不同步** —— 本次事故的**根因** | 协议**实现**不自洽（例如插件发出空帧、`done` 缺失） |
| 代价 | 毫秒、离线、无凭据 | 秒级起、需网络与 git 凭据 |
| 何时红 | 有人只改了一边的 proto/src | 行为不兼容 |

**这次事故的根因是前者**，所以先补前者。后者仍值得做（它同时能替代
「插件自测两端同版本」那个盲区），但**不是这一批的前置**，登记为待办。

### 2.6 已知债务：proto 有**两份**，靠手工同步

本仓 `crates/sm-plugin-api/build.rs` 从**本仓根 `proto/`** 编译
（`CARGO_MANIFEST_DIR.ancestors().nth(2)`），而插件从契约仓编译。也就是说
**同一份 proto 被维护了两遍**。

可选方案：契约仓作为唯一源，本仓用 git submodule / subtree 或构建期拉取。
**本轮不建议做** —— 它会与「离线 NAS 构建零网络」（`README.md` 的「零外部依赖」节）
和 `nth(2)` 的路径约定冲突，值得单独评估。但**必须登记为债务**：
现在是「一个改动要落两个仓，且没有机器检查」。

---

## 三、P1-2 决策：`PlaybackPlan` 加本地路径 delivery

### 3.1 问题

`proto/storage.proto:41-52`：

```proto
message PlaybackPlan {
  oneof delivery {
    RedirectPlan redirect = 1;
    ProxyPlan proxy = 2;
    // ← 3 空闲
  }
  optional int64 size_bytes = 4;
  optional string content_type = 5;
  bool unavailable = 6;
}
```

本地 / NFS / SMB 挂载类 provider —— 也就是**最基础的一种** —— 只能把
「给你一个路径，你自己读」退化成 `file://` 伪直链，逼宿主为 `file://` 单开一个
分支，还要自己关心 URL 转义（空格 / `#` / `%`）。

**同一份 proto 里有反证**：`OpenCoverSourceResponse`（`storage.proto:229-237`）
**恰有** `oneof { local_path, url }`。同一个仓库里两套口径。

### 3.2 三个方案

| 方案 | 内容 | 代价 |
|---|---|---|
| **A（推荐）** | 加 `LocalPathPlan { string path = 3; }`，走 `OpenCoverSourceResponse` 同一套语义 | 宿主 `plan_playback` 消费点加一支；`plugin-ref-local` 去掉 `file://` 拼接 |
| B | 约定 `RedirectPlan` 里用 `file://` 前缀表示本地 | 不推荐：要做 URL 转义、宿主必须按前缀特判、语义靠约定而非类型 |
| C | 不改 proto，宿主自己识别 `file://` | 等于把 B 的代价留给宿主，且 30 个 provider 会有 30 种做法 |

### 3.3 方案 A 的改动（精确）

```proto
// storage.proto，PlaybackPlan 内
  oneof delivery {
    RedirectPlan redirect = 1;
    ProxyPlan proxy = 2;
    // 本地 / 挂载类 provider 的原生答案：给一个路径，宿主自己读。
    //
    // 与 `OpenCoverSourceResponse` 的 `local_path` 是同一套语义 ——
    // 同一个 proto 里不该有两种表达「本地文件在哪」的方式。
    LocalPathPlan local_path = 3;
  }
```

配套消息：

```proto
message LocalPathPlan {
  // 宿主机上可读的绝对路径。**不做 URL 转义** —— 这里是路径不是 URL，
  // provider 不要编码，宿主也不要解码。
  string path = 1;
}
```

**编号 3 是安全的**：它当前没被 `PlaybackPlan` 的任何字段占用
（`1`/`2` 在 `oneof` 内，`4`/`5`/`6` 在外，`3` 空着）。

### 3.4 影响面

| 位置 | 改什么 |
|---|---|
| 契约（proto + `sm-plugin-api`） | 新消息 + `oneof` 加一支；重生成 |
| `crates/sm-plugins/src/provider_calls.rs` | `plan_playback` 的返回处理加一支（**注意**：`PlaybackPlan` 还有 `unavailable: bool`，两支失败语义要理清 —— 这也是 P1-3 落地的那个通道该用起来的地方） |
| `plugin-ref-local/src/provider.rs` | 不再拼 `file://`；它的 `plan_playback_refuses_proxy_delivery` 测试正好覆盖「不支持 proxy 时明确报错」这条，可保留 |
| 未来的 `local_provider`（`deployment.md` §六 阶段 D） | 直接返回路径，不必再想 URL 编码 |

> **P1-2 与阶段 D 有先后关系**：`local_provider` 是第一个真正会用到它的插件。
> 建议在开始阶段 D 之前定下来 —— 否则那个插件要先按 `file://` 写一遍再改。

---

## 四、执行顺序与判据

```text
① P0 契约分叉修复（§二）        ← 本仓侧已做完，卡在「推送契约仓」
② P1-2 决策（§三）              ← 在开始 local_provider 之前
③ data_plane_endpoint 写成硬性要求 ← 与 ② 同批写进 docs/plugin-abi.md
```

### 判据

- **①** 完成（本仓 / 契约仓两侧都算）：
  - [x] `parity/check_contract_sync.py` 报 9 个受管文件一致，且**人为制造漂移时会红**（已验）
  - [x] 两仓 `ABI_MAJOR` 都是 2，`cargo test` 绿
  - [ ] **推送契约仓 + tag `v0.2.0`**（需要凭据）
  - [ ] 两个插件改 `tag = "v0.2.0"` 且 `cargo test` 绿
  - [ ] `plugin-ref-local` 的 `generate_thumbnails` 以 `done` 收尾（`GAP:` 注释删掉）
- **②** 完成：`PlaybackPlan` 有 `local_path`；`docs/plugin-abi.md` 写明三种 delivery
  各自的适用场景；`sm-plugin-api` 重生成后两个插件都能编译。
- **③** 完成：`plugin-abi.md` 明确写「**字节搬运必须走 `data_plane_endpoint`**」，
  并给出反面例子（`StageImportFile` / `ReadTransferSource` 走控制面会撞 4MB 上限）。

---

## 五、本提案没做的事

- **没改任何 proto / 代码**（除本文件的 §三 是 diff 提案，未落盘）—— 上表 ① ② 都待拍板。
- 没评估 `sm-plugin-api` 是否该拆成「必需 / 可选」几个 service（P1-4 的另一个方向，
  报告 §4 P1-4 已注明「值得单独开一次讨论」）。
- 没碰 `host.proto` / `plugin.proto` 的缺口 —— 它们两侧一致，不在分叉范围。
- 没解决 proto 双份维护（§2.5），只登记为债务。
