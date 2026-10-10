# 决策：宿主 → 插件的调用面（provider seam）怎么接

- 日期：2026-10-07
- 状态：**已决定，待实施**
- 关联：`2026-10-05-plugin-lifecycle.md`、`2026-10-06-plugin-architecture.md`

## 一、结论

允许 **`sm-service → sm-plugins`**，但**只用于 provider 调用层**
（`sm_plugins::provider_calls`），不碰 loader / supervisor / registry。

## 二、依据：那条环已经被拆掉了

上一版 ADR 写「sm-service 只依赖契约、不依赖实现」，理由是依赖环：

```
sm-plugins → sm-scheduler → sm-service → sm-plugins   （旧）
```

P0（`22ea79e` 起）把 `sm-plugins → sm-scheduler` 拆掉之后，`sm-plugins` 的
依赖面是：

```
sm-core / sm-plugin-api / sm-db / tonic / tokio / tracing
```

**已核实**：全 crate 无 `sm_service` 引用（`crates/sm-plugins/**/*.rs` 搜
`sm_service` = 0 命中）。所以 `sm-service → sm-plugins` **不成环**：

```
sm-server ──▶ sm-service ──▶ sm-plugins ──▶ sm-plugin-api（叶子）
    └────────────────────────────────────────┘
```

不拆环就得靠「组合根注入一批窄 trait」绕开，代价是：
`ProviderOperationError`（含 7 个码与 `retryable`）要在 `sm-service` 里**再
定义一份**，而调用方分支（`source_not_found` → 继续、`unavailable` → 重试）
恰恰依赖它 —— 第二份定义必然漂移。

## 三、约束（写下来防止以后加回去）

1. `sm-plugins` **不得**依赖 `sm-service` / `sm-scheduler` / `sm-api`。
   一旦加了，本决策失效，必须回到注入方案。
2. `sm-service` 引用 `sm-plugins` **只用** `provider_calls` 与其中的错误
   类型；不用进程管理（`loader` / `supervisor`）与注册表（`registry` /
   `jobs`）—— 那些属于组合根。
3. 新增 provider 能力时，先加 `provider_calls` 的 wrapper（一处），再让
   service 调它；不在 service 里直接持有 tonic client。

## 三·补：已落地（导入组 + 指纹）

`sm_plugins::provider_calls` 新增：`scan_import_source_all`（server stream
收干成 `Vec`）、`stage_import_file_call`、`finalize_import_call`、
`abort_import_call`、`delete_import_file_call`、`compute_file_hash_call`，
以及两个宿主侧结构体 `ImportFileEntry` / `StagedImport`。

配套：`sm_plugin_api::json_struct` —— 宿主是 `serde_json::Value`、proto 是
`google.protobuf.Struct`，互转规则只有一份（放契约仓，插件作者可直接用）。

### ★ 顺带核出的**契约缺口**：`in_place` 传不过去

`SourceDisposition` 只有 `KEEP` / `DELETE_AFTER_COMMIT`（`proto/common.proto`
那个 enum 就两个值），而上游三方（`import_service.py:188` /
`schema/transfers/media_import.py:39` / `:629`）都认 `in_place`。
**这个 ABI 现在表达不了「原地导入」**，宿主必须按「不支持」处理
（`in_place_import_unsupported` 那条 422），不能塞 `UNSPECIFIED` 蒙混 ——
它在 provider 侧的语义未定义。要支持得先给 proto 加枚举值。

## 四、缺口清册：还差哪些 wrapper

契约消息已在 `sm-plugin-api::v1`（`proto/storage.proto`），缺的是把 tonic
调用包成 `ProviderOperationError` 的那层。按解锁顺序：

| 组 | rpc（proto 行号） | 解锁 |
|---|---|---|
| 导入 | `ScanImportSource`(368, **server stream**) / `StageImportFile`(374) / `FinalizeImport`(376) / `AbortImport`(378) / `GetImportSourceIdentity`(396) / `DeleteImportFile`(372) | `import_service` 3 处、`import_task` 部分 |
| 指纹 | `ComputeFileHash`(382) | `import_service::_create_media` 的 `file_hash` |
| 转存 | `OpenTransferSource`(407) / `ReadTransferSource`(408, 双向流) / `AssertTransferSourceUnchanged`(409) / `CloseTransferSource`(410) / `CleanupTransferSource`(411) / `StageTransfer`(414) / `FinalizeTransfer`(415) / `AbortTransfer`(416) | `media_transfer_task` 4 处 |
| 下载 | `Submit`(425) / `ListTasks`(427) / `DeleteTask`(429) / `PrepareClient`(431) / `TestClient`(432) | `download_client` 5 处、`download_sync` 4 处、`download_common` 2 处、`auto_download` / `download_request` / `download_task` 各 1 |
| 浏览 | `Browse`(366) | `provider_browse` 1 处 |

已核对的关键消息（`proto/storage.proto`）：

```proto
message StageImportFileRequest {
  LibraryHandle library = 1;  ImportFile source = 2;
  ImportPlacement placement = 3;
  SourceDisposition source_disposition = 4;
  string operation_key = 5;   // 幂等键：同一 key 重复调用必须返回同一结果
}
message FinalizeImportRequest { LibraryHandle library = 1; google.protobuf.Struct receipt = 2; }
message ComputeFileHashResponse { string file_hash = 1; }  // "media-file-hash-v1:<40 hex>"
```

⚠️ `ImportFile` / `ImportPlacement` / `StagedMedia` / `MediaHandle` /
`LibraryHandle` / `SourceDisposition` **定义在 `common.proto`**
（`storage.proto` 里只有引用）—— 实施时先读那边，不要照名字猜字段。

## 五、实施顺序

1. 先补齐「导入」那一组（4 个 + `ComputeFileHash`）：它解锁 `import_service`
   3 处，而那三处的上游语义**已经核清**（见 `docs/handoff.md`），可以一次写完。
2. 再「下载」组（5 个）：解锁 12 处，是 transfers 最大的一块。
3. 「转存」组放最后：四个流式 rpc 的生命周期（open → read → assert →
   close/cleanup）要单独设计，贸然写会与「校验后切换」的语义错位。

## 六、不做什么

- **不**为了「看起来完整」新增 proto 里没有的能力；
- **不**把 `ProviderOperationError` 复制到 `sm-service`；
- **不**在 service 里直接持有 `StorageProviderClient`（端点解析与连接归
  `provider_calls`）。
