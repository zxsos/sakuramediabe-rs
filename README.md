# sakuramedia-plugin-ref-local

SakuraMedia 的 **gRPC 参考插件**：把一个本地目录包装成 `StorageProvider`，用来
打穿插件 ABI 的数据面、并量化「插件拆进程」的开销。

归属仓库：[`sakuramediabe-rs`](https://cnb.cool/zxsos1/sakuramediabe-rs)。

## 它回答的三个问题

1. server streaming（`GenerateThumbnails` / `ScanImportSource`）能否跑通；
2. 同机回环的一元 RPC 往返开销量级，够不够支撑插件拆进程；
3. 现有 proto 够不够做一个真实插件（缺口清单见后端仓库
   `docs/parallel/grpc-plugin-report.md`）。

只实现最小集：

| rpc | 形态 |
|---|---|
| `Browse` | 一元 |
| `PlanPlayback` | 一元 |
| `GenerateThumbnails` | server streaming（`ProgressEvent`） |
| `ScanImportSource` | server streaming（`ImportFileEntry`） |

其余方法一律返回 `Status::unimplemented` —— tonic 生成的 trait **没有默认方法体**，
「只想实现 4 个方法」也必须写满全部。这也是契约层提供 `StorageProviderExt`
默认实现层的原因。

## 构建与测试

```bash
cargo build --release            # 产物供宿主按生命周期协议拉起
cargo test                       # roundtrip（RPC 往返）+ latency（开销统计）
```

宿主侧的生命周期集成测试（用 `sm_plugins::supervisor` 真拉起本二进制并走完
`Register`）**不在本仓库** —— 它依赖宿主实现，按「插件只依赖契约」的拆分原则留在
后端仓库，日后由后端以 `[dev-dependencies]` 按 tag 引入本仓库来跑。

## 许可证

GPL-3.0-or-later（见 `LICENSE`）。
