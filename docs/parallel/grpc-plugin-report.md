# gRPC 参考插件报告（并行线 C）

**日期**：2026-10-04　**分支**：`feat/grpc-ref-plugin`　**crate**：`crates/plugin-ref-local`
**对应决策**：ADR `docs/adr/2026-10-04-tech-selection.md` · 决策 A「插件 ABI」
**结论先行**：**按当前 proto 做一个真实插件是可行的，但 proto 需要在合 SMA（主骨架）
之前补 4 处结构性缺口**（见 §4 的 P1 清单）。gRPC 作为**控制面**的开销量级可接受；
瓶颈不在传输，而在「proto 把返回值与失败语义丢掉了」这件事本身。

---

## 1. 做了什么

把「一个本地目录」包装成 `StorageProvider` gRPC 服务，然后用**真实 server
（127.0.0.1 + 内核分配的随机端口）+ 真实 tonic client** 跑往返。

| rpc | 形态 | 实现要点 |
|---|---|---|
| `Browse` | 一元 | 目录分页；`parent_ref` 是不透明 Struct；路径穿透要自己拦 |
| `PlanPlayback` | 一元 | 存在则回 `RedirectPlan`（`file://`），不存在回 `unavailable=true` |
| `GenerateThumbnails` | **server streaming** | 按 4 帧/流发 `ProgressEvent`，产物落进宿主给的 workspace |
| `ScanImportSource` | **server streaming** | 递归遍历，边走边发 `ImportFileEntry`（8 槽缓冲，背压真实发生） |

其余 28 个 rpc 返回 `Status::unimplemented` —— 不是偷懒，是 tonic 0.14 不给默认实现
（§4.1），这部分本身就是本线最重要的发现之一。

复现：

```bash
cd /workspace/.worktrees/plugin
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="$(pwd)/target"
cargo test -p plugin-ref-local                                  # 12 个功能测试
cargo test -p plugin-ref-local --test latency -- --nocapture     # 延迟数据（§3）
cargo clippy -p plugin-ref-local --all-targets -- -D warnings
```

---

## 2. 验收结果

| 命令 | 结果 |
|---|---|
| `cargo test -p plugin-ref-local` | **13 passed / 0 failed**（含 2 个 server-streaming 往返） |
| `cargo clippy -p plugin-ref-local --all-targets -- -D warnings` | 通过 |
| `cargo fmt -p plugin-ref-local -- --check` | 通过 |

流式测试**不靠超时判定结束**：用 `while let Some(item) = stream.next().await`
收完再断言，`None` 即正常 EOS（OK 状态），非正常断开会以 `Status` 出队。

| 测试 | 断言的事 |
|---|---|
| `generate_thumbnails_streams_progress_in_order` | 10 帧顺序严格为 current=1..10，且流正常结束 |
| `scan_import_source_streams_files_in_order` | 5 个文件的相对路径顺序、大小、is_video 全部逐项对齐，且流正常结束 |
| `cancelling_a_stream_does_not_break_the_server` | 客户端中途 `drop(stream)` 后，同一连接上的 Browse 照常可用 |
| `plan_playback_refuses_proxy_delivery` | 本地 provider 撑不起 proxy 投放时是明确报错，不是静默降级 |
| `browse_rejects_path_escape_and_missing_library` | `../../etc` 被拒；缺 library / 目录不存在分别给出不同 code |

---

## 3. 代价：往返延迟（本机回环，只求量级）

单次代表性运行的原始输出：

| 项目 | 样本 | min(μs) | p50(μs) | p95(μs) | max(μs) |
|---|---:|---:|---:|---:|---:|
| 一元 RPC 纯传输（服务端零工作） | 1000 | 280 | 437 | 684 | 13180 |
| 一元 RPC + 一次目录列举（Browse） | 1000 | 534 | 844 | 1809 | 7244 |
| 流式首帧（GenerateThumbnails，含写文件） | 200 | 512 | 1167 | 41608 | 42884 |
| 流式首帧（ScanImportSource，只读目录） | 200 | 487 | 1111 | 41780 | 45669 |

四次复跑的分散度（debug 构建，同一容器）：

| 项目 | p50 区间 |
|---|---|
| 纯传输 | 360 – 575 μs |
| Browse（含 1 次 read_dir + 逐项 metadata） | 560 – 845 μs |
| 流式首帧 | 0.65 – 1.9 ms |

读法：

- **纯传输 ~0.4ms 是「把插件拆进程」要付的税**，也是唯一的税。这里面包含
  HTTP/2 帧编解码 + prost 序列化 + tokio 任务切换。
- **Browse 比纯传输贵的那 ~0.3ms 全是文件系统 IO**（每个条目一次 `metadata`），
  跟 gRPC 无关。也就是说：**provider 里多做的活远比多一层进程贵**，
  优化顺序应当是「先省 syscall，再省进程」。
- 一次 browse / stage_import_file / plan_playback 的调用密度下（每页 100 条、
  每文件一次 unary），这里的量级结论是：**每次 gRPC 调用 ≈ 0.4ms，
  每秒几千次调用，够用**。
- 需要搬运**字节**的地方不要走这里：`StageImportFile`、`ReadTransferSource`
  这类应当走 proto 已经预留的 `data_plane_endpoint`（HTTP），
  否则一次 4MB 的默认消息体上限会成为隐形天花板。

### 3.1 已知噪声：约每 3 条新建流出现一次 ~41ms 卡顿

必须写清楚，否则这张表会被信任过度：

- 现象：流式测量的样本里，**大约每 3 次新建流**出现一次 ~40–43ms 的首帧延迟，
  规律性很强（`idx=24,27,30,33,36,39,42…`）。
- 排除过的解释：
  - 不是文件写入造成的 —— 只读的 `ScanImportSource` 同样有；
  - 不是 tokio 运行时 flavor 造成的 —— 单线程与 `worker_threads=4` 都复现；
  - 不是其它并行测试抢 CPU —— `--test-threads=1` 单独跑也复现；
  - 一元 RPC 的 p95 只有 ~1.8ms，**只有「新建流」会触发**。
- 因此它大概率是本机/容器回环 TCP + HTTP/2 建流路径上的环境问题
  （延迟 ACK 一类），**在正式下定论前需要在目标 NAS 硬件上复测**。
- 处理方式：**不要拿 p95/max 当结论**。本报告的全部判断都基于 p50 区间。

---

## 4. proto 缺口清单

按我判断的处理顺序排列。**P1 = 建议主线在合 SMA 之前谈定**，因为它们要么
改变 service 签名（`StorageProvider`），要么要求重生成 `sm-plugin-api`
——后补的代价远高于现在改。我没有改任何 proto，全部落实为报告。

### P1-1 · `GenerateThumbnails` 的返回值丢了（定义被冷落的 `GenerateThumbnailsResponse`）

- 位置：`proto/storage.proto:369`，对照 `159-176`
- 现状：`rpc GenerateThumbnails(...) returns (stream ProgressEvent)`，
  而 `GenerateThumbnailsResponse { ThumbnailGeneration generation = 1; }`
  **定义了却没有任何 rpc 用它**。
- 后果：进度能流式回来，**产物回不来**。`ThumbnailGeneration.expected_count`
  与实际生成的 `ThumbnailArtifact` 列表（offset_seconds + relative_path）
  没有通道 —— 宿主不知道生成了几张、叫什么名字、该写到哪一格 time line。
  我在测试里只能断言「workspace 里确实多了 10 个文件」，而这 10 个文件名
  客户端是拿不到的。
- 建议：改成 `returns (stream GenerateThumbnailsResponse)` 并在其中加
  `oneof { ProgressEvent progress = 1; ThumbnailGeneration done = 2; }`，
  或者给 `ProgressEvent` 加终态变量。**这是唯一一处「返回值被吞掉」的设计**，
  其余 streaming rpc 都还能用 error-only 收场。

### P1-2 · `PlaybackPlan` 缺一种 delivery：本地路径

- 位置：`proto/storage.proto:26-52`
- 现状：`delivery` 只有 `RedirectPlan`（302 外链 URL）与 `ProxyPlan`
  （插件自己的 HTTP endpoint）。
- 后果：本地 / NFS / SMB 挂载类型的 provider —— 也就是最基础的一种 ——
  只能把自己最擅长的答案「给你一个路径，你自己读」退化成 `file://` 伪直链，
  逼宿主为 `file://` 单开一个分支。我在 `plugin-ref-local` 里就是这么做的
  （`plan_playback`），宿主随后还得关心 URL 转义（空格、`#`、`%`）。
- 不一致的证据：`OpenCoverSourceResponse`（`storage.proto:229-237`）
  **恰有** `oneof { local_path, url }`。同一个 proto 里两套口径。
- 建议：给 `PlaybackPlan.delivery` 加 `LocalPathPlan { string path = 3; }`
  （走 `OpenCoverSourceResponse` 同一套语义）。

### P1-3 · 失败只有一个布尔位，`ProviderError` 无处安放

- 位置：`proto/common.proto:311-329`（定义了 `ProviderError` + `ProviderErrorCode` + `retryable`），
  对照 `PlaybackPlan.unavailable`（`storage.proto:50-51`）
- 现状：`unavailable: bool` 是这 37 个 rpc 里**唯一**表达「我给不了」的手段。
  精心定义的 `ProviderError`（含 code / safe_message / retryable）
  没有任何 rpc 把它当返回值，proto 也没定义它在 gRPC status details 里怎么编码
  （未 import `google/protobuf/any.proto`，也没有约定 status metadata 键名）。
- 后果：「文件不存在」「没权限」「已被黑名单」「临时不可用，请重试」
  在宿主眼里完全一样。宿主无法决定是回退还是重试。
- 建议：二选一并写进 ABI 文档 —— (a) 统一用 gRPC status + `ProviderError`
  作为 `google.rpc.Status` 的 detail；(b) 在响应里加 `optional ProviderError error`。
  无论哪种，请先写清楚映射规则，否则 30 个 provider 会有 30 种做法。

### P1-4 · tonic 0.14 生成的 trait 没有默认实现，32 个方法必须全写

- 位置：`StorageProvider` service（`storage.proto:347-403`）
- 现状：生成的 `trait StorageProvider` 里 32 个方法**全部没有方法体**
  （`grep -c unimplemented` 结果 0）。这不是 proto 的问题，是 tonic-prost-build
  0.14 的行为，但它直接决定了「写一个插件要动多少行」。
- 后果：只想实现 4 个方法的参考插件，也要手写 28 个 `Status::unimplemented`
  签名（见 `provider.rs` 尾部）。量级感可以从这一句得到：
  `provider.rs` 共 811 行，**其中约 170 行只是为了把 trait 填满**。
  而且这是**编译期强制**的：proto 每加一个 rpc，全部插件都编译不过。
- 建议：proto 侧接纳这个现实（不必改 proto），但**在 `sm-plugin-api` 里
  加一层适配** —— 例如一个 `DefaultStorageProvider` 适配 trait，
  让插件只需 impl 自己那几个方法。**这属于 `sm-plugin-api` 的活，
  我按边界没有动它**，但它应该排进 SMA 的任务列表。
  另一个方向：把 32 个 rpc 拆成几个更小的 service（必需 / 可选 / 转存 / 合并播放），
  让插件按需实现 —— 代价是宿主要管理多条 service，值得单独开一次讨论。

### P2-5 · 不透明 `Struct` 的 schema 无处宣告

- 位置：`LibraryHandle.provider_config`、`MediaHandle.storage_ref`、
  `BrowseRequest.parent_ref`、`ScanImportSourceRequest.source_ref`、
  以及所有 `receipt`
- 现状：`Struct` 换来了「宿主无需理解 provider 方言」，代价是
  **宿主无法校验、无法渲染表单、无法在错误时指出缺哪个字段**，
  而且每个插件都要自己写一遍「取字段 + 缺字段报错」（我写了两份：
  `opaque.rs` 与 provider 内的 `confined_path`）。
- 建议：至少给一个登记 DB（文档 + `RegisterResponse` 可选字段），
  说清楚每个 provider_key 用的键名与类型。不改 proto 也能做。

### P2-6 · `Browse` 分页/排序语义未定义

- 位置：`BrowseRequest`（`storage.proto:86-91`）、`BrowsePage`（`common.proto:118-121`）
- 缺口：`limit` 是 `int32`（可为负）、没有默认值与上限；没说按什么排序；
  游标是不透明字符串但没有失效语义；`BrowsePage` 没有总数，宿主渲染不出「共 N 项」；
  `EntryType` 只有 FILE/DIRECTORY，**符号链接无处安放**。
- 我在 `plugin-ref-local` 的自定方案（可当作候选约定）：`limit <= 0` → 100，
  上限 1000；同层按文件名字典序；游标 = 上一页最后一个条目名；目录不给 size。

### P2-7 · `ScanImportSource` 没有进度、没有汇总、错误会以流中断收场

- 位置：`storage.proto:351` + `ImportFileEntry`（`98-100`）
- 现状：流里只有 `ImportFileEntry`，既没有 `ProgressEvent` 那样的进度位，
  也没有终态汇总（对比 P1-1 的 `ThumbnailGeneration`）。
- 后果：宿主做不出「已发现 1234 个文件」的进度条；扫描到一半遇到坏目录时，
  只能以 `Status` 中断整条流，**已经扫到的部分也随之作废**；
  `ImportFile` 没有「来源是否已去重」「建议如何 stagger」之类的提示，
  宿主只能全量接收再自己过一遍。
- 建议：改成 `returns (stream ScanImportSourceResponse)`，
  `oneof { ImportFileEntry entry = 1; ScanSummary summary = 2; }`。

### P2-8 · 其它零碎（不改也能活，但值得记一笔）

| 位置 | 现象 |
|---|---|
| `MediaHandle` | 只有 `file_name` 与 `storage_ref`，**没有 relative_path**；storage_ref 缺 path 时插件只能猜 |
| `BrowseEntry.modified_at` | 是 `string`（RFC 3339），不是 `Timestamp`，宿主侧解析成本转嫁给了每个插件 |
| `BrowseEntry.size_bytes` | 用 `int64` 承载 u64 语义，>i64::MAX 的文件只能被截断 —— 我在 `clamp_i64` 里写死 `i64::MAX`，这在语义上是**静默不正确** |
| `CreateClipRequest` | `start/end_offset_seconds` 是 `int32`，长视频会触顶 |
| `ProgressEvent` | 只有 `text/current/total`，没有 phase / level / 结构化 payload；一个失败/警告装不进去 |
| `storage.proto` 全部 | 没有 `deadline` / `timeout` 约定，也没有 request id 用于跨进程日志关联 |
| 所有 rpc | 没有 `fields_mask` / 稀疏返回；`probe_*` 类可选能力每次都要一整次往返 |

---

## 5. 结论：工作量与风险是否可接受

**可接受，但需要配套动作。**

1. **server streaming 跑得通，且语义可靠。** 顺序严格、EOS 正常、背压真实
   （8 槽缓冲验证过）、客户端断开不影响服务端**（设计成立，
   `GenerateThumbnails` / `ScanImportSource` 这两个最难 RPC 化的能力是被覆盖了。
2. **性能不是阻力。** 控制面 0.4ms/次、几千次/秒的量级，对比「每个 provider
   多上一次 FFmpeg 或一次 fs metadata 就是 0.3ms 起」来说并没有涨 —— 真正要
   绕开的是**字节搬运**，proto 已经有 `data_plane_endpoint` 这个出口，
   应当在 ABI 文档里把它**写成硬性要求而不是可选优化**。
3. **阻力在 proto 的表达力，不在 gRPC 本身。** P1-1（返回值被吞）、
   P1-2（本地路径没 delivery）、P1-3（失败只有一个布尔位）三条，
   **每一条都会让宿主在狂欢期不得不开外挂分支**。它们改起来都不大，
   但会重生成 `sm-plugin-api`，越晚改越贵。
4. **真正的工作量在 P1-4。**「32 个方法必须全写」是本线摸出来的最大隐性成本：
   每写一个插件先缴 170 行签名的税。建议在 `sm-plugin-api` 里加一层默认实现
   （或拆分 service），否则 plugin hub 上每多一个插件，这笔税就重缴一遍。

给主线的下一步建议（按优先级）：

- [ ] 就 P1-1 / P1-2 / P1-3 三个缺口做一次决策（改 proto or 写文档约定），
      结论出来后 `sm-plugin-api` 重生成
- [ ] 在 `sm-plugin-api` 里引入默认实现层，消灭 P1-4 的税
- [ ] 把 `data_plane_endpoint` 写成字节搬运的强制路径
- [ ] 目标 NAS 硬件上复跑 §3 的测量，验证 §3.1 的 ~41ms 是否为环境噪声

---

## 6. 本线没做的（边界）

- 不实现完整 37 个 rpc，不做插件生命周期 / 安装 / 依赖管理（`sm-plugins` 的活）
- 不改 `proto/*.proto`、不碰 `sm-api` / `sm-service` / `sm-db` / `sm-core`
- `Cargo.toml` 只加 `members` 一行 + `[workspace.dependencies]` 一行；
  `[profile.release]`、`argon2` 0.6 保持一致，未引入新的第三方 crate
  （`Cargo.lock` 的改动只有新增 `plugin-ref-local` 一个 package 块，14 行，纯增量）
- 缩略图不真正解码视频（不引 `image`，那属于 `svc-image` 的活）—— 产物是占位文件
