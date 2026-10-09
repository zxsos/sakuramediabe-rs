# sakuramediabe-rs

SakuraMedia 后端的 Rust 重写实现。

> **这是 [`sakuramediabe`](https://github.com/tinypinglite/sakuramediabe)
> （Python/FastAPI，作者 tinypinglite）的衍生作品**，与其同以
> **GPL-3.0** 授权。取自上游的代码范围见 [NOTICE.md](NOTICE.md)。
> GPL-3.0 的 copyleft 条款决定了本项目**无法**以更宽松的许可证分发。

采用**渐进式**策略：按模块逐个替换，客户端与 API 契约保持不变。

## 为什么渐进式而非全量重写

后端 38,802 行 + 19,802 行测试、126 个 API 端点。真正的约束不是性能，而是：

- `uvicorn --workers 1`（`docker/backend/supervisord.conf:16`）—— 插件是进程内
  Python 包，多 worker 会各自加载一份副本，状态与文件锁全乱。后端因此**无法
  水平扩展**，吞吐上限锁死在单进程 GIL。
- 主后端只有 4 个文件真正需要重量级媒体库，且全部落在同一批模块里。

## 现状

| crate | 作用 | 替代掉 | 测试 |
|---|---|---|---|
| `hashing` | SHA-1 / SHA-256 / Base32 | — | 13 |
| `media-file-hash` | `media-file-hash-v1` 采样指纹 | 两个 provider 里的重复实现 | 9 |
| `svc-hash` | BT info hash 解析 | **`libtorrent`** | 32 |
| `sm-core` | JWT / Argon2 / 统一错误信封 | — | 72 |
| `sm-db` | **40 张表的模型映射（40/40 ✅）** | `model/` + 部分 Peewee → sqlx | 65 |
| `parity-cli` | 对拍入口（开发工具） | — | — |

- `cargo test`：**192 passed / 0 failed，零编译警告**
- `python parity/compare.py`：**44/44** Rust 与 Python 逐条一致
- `python parity/compare_core.py`：**64/64** 核心原语逐条一致

### 重构进度

| 层 | 后端规模 | 已完成 | 进度 |
|---|---|---|---|
| 模型 `model/` | 40 表 | 40 表 | **100% ✅** |
| 服务 `service/` | 114 文件 | — | 0% |
| Schema `schema/` | 44 文件 | — | 0% |
| API `api/` | 126 端点 | — | 0% |
| 插件 ABI `provider_protocol.py` | 543 行 / 30 方法 | — | 0% |

**全部 8 个域完成**：`catalog`(9) / `playback`(6) / `collections`(6) / `videos`(3) / `transfers`(6) / `system`(5) / `discovery`(5)，共 40 表。
详见 [docs/schema-mapping.md](docs/schema-mapping.md)。

## 零外部依赖

`hashing` / `media-file-hash` / `svc-hash` 三个库不引入任何第三方 crate。
SHA-1 / SHA-256 / Base32 / bencode 全部自实现：

1. 产物是塞进 `sakuramedia` 镜像的静态二进制，依赖越少交叉编译与审计面越小。
2. 四个算法都是固定标准实现，总计约 500 行，每处都有公开测试向量兜底。
3. 能在无外网环境完成 `cargo build`（CI / 离线 NAS 构建）。

`bencode` 只做单遍扫描 + 深度上限（32），不做完整反序列化 —— 需求仅仅是
定位顶层 `info` 字典并取出它的**原始字节区间**（v1 hash 是对原始编码字节做
SHA-1，重新编码会得到不同哈希）。

## 关键语义：不可改动的部分

替换 Python 实现时，下列行为必须逐字节对齐，否则已入库的 `file_hash`
会全部失效、重复文件识别会出现假阳性/假阴性。

### `media-file-hash-v1`

```text
size < 8 MiB  ->  payload = "media-file-hash-v1" + b"\x00full\x00" + size_be64 + sha1(全文)
size >= 8 MiB ->  采样：头 3 MiB + 尾 3 MiB + 两段中间采样各 1 MiB（共 8 MiB，恒定）
                   槽位由头/尾摘要前 8 字节决定 => 同一文件在任何机器上命中同一组槽位
                   payload = "media-file-hash-v1" + b"\x00sampled\x00" + size_be64 + 四段 sha1
结果 = "media-file-hash-v1:" + sha1(payload).hexdigest()
```

三个协议向量（两个插件测试里共享同一组）：

| 输入 | 期望 |
|---|---|
| `hash_fixture(8 MiB)` | `media-file-hash-v1:52385d3512a8a9ff8b6e6c5aa315e46633b28d9a` |
| 空文件 | `media-file-hash-v1:524935ebf533f3b952f2397f80691a87a7b289c7` |
| `b"abc"` | `media-file-hash-v1:da6ba51927337cc1035be69e84f851f48dbe7d71` |

### BT info hash

- `canonical_info_hash`：40 位 hex（v1）优先判定，其次 32 位 base32（v2）。
  Python 侧是 `b32decode(value.upper())`，因此**小写 base32 也必须接受**。
- `_magnet_hash`：先 `unquote` 再不区分大小写搜 `urn:btih:`。
- `_torrent_hash`：**强制要求 v1**。纯 v2 种子（`info` 无 `pieces`）必须被判为
  `invalid_download_torrent`，对应 libtorrent 的 `info_hashes().has_v1() == false`。

### 错误码契约

| Rust | HTTP | code |
|---|---|---|
| `InvalidResourceHash` | 422 | `invalid_download_resource_hash` |
| `InvalidTorrent` | 422 | `invalid_download_torrent` |
| `InvalidSource` | 422 | `invalid_download_source` |
| `SourceNotFound` | 404 | `download_source_not_found` |
| `SourceUnavailable` | 503 | `download_source_unavailable` |
| `TorrentTooLarge` | 422 | `download_torrent_too_large` |

这张表同时被 `svc-hash/src/lib.rs` 的 `status_and_code()` 与对拍脚本断言，
改动任一侧都必须同步 `src/service/transfers/downloads/resource_hash.py`。

## 分工：HTTP 抓取仍在 Python

原实现里紧邻哈希解析的还有 HTTP 重定向链追踪（≤5 跳、10 MiB 上限），
**故意不复刻** —— `httpx` 已经在后端跑得好好的，为它引入 `reqwest` + `tokio`
会让这些 crate 从「零依赖纯逻辑」变成重量级网络服务。Python 侧继续负责抓取，
只把最终字节交给 Rust。

## 构建与验证

```bash
cargo test                                        # 54 项单元测试
cargo build --release -p parity-cli              # 对拍二进制
python parity/compare.py                         # 全量对拍（44 项）
python parity/compare.py --quick --debug         # 跳过 8 MiB 级用例
```

`parity/` 下另有两个辅助脚本：

- `gen_bencode_fixtures.py` —— 用标准编码器生成 bencode 测试数据并自校验。
  bencode 的长度前缀极易写错（`13:meta version` 的 13 是**字符数**，
  而 `meta version` 13 个字符、`file tree` 只有 10 个），手写几乎必错。
- `trace_bencode.py` —— 逐行模拟 `parse_torrent`，用于定位解析失败点。
  写 Rust 测试时先在这里验证，能省掉大量「为什么 unwrap 挂了」的往返。

## 对拍框架

`parity/compare.py` 的两条原则：

1. Python 侧**不复用**后端源码，而是照着插件/后端语义独立重写一遍。
   两份独立实现一致，才说明 Rust 侧正确。
2. 同时比对**成功路径与失败路径**。错误码必须逐条对齐 —— 迁移最容易漏的
   恰恰是「本该报错却成功了」这种反向失败。

它已经抓出过两类真实问题：bencode 少写一个闭合符导致 `unwrap` 失败，
以及参照实现错把「整个种子」而非「info 字典」做哈希。

> 大块数据必须走 `fingerprint <path>`：Windows 命令行有 32 KiB 上限，
> 8 MiB 的十六进制传不进去。

## 路线图

| 阶段 | 内容 | 改动面 | 状态 |
|---|---|---|---|
| **1** | `svc-hash` 接入 → 移除 `libtorrent` | 主后端 1 文件 | **代码就绪** |
| 1b | `media-file-hash` 接入 → 两插件去重 | 2 个插件 | **代码就绪** |
| 2 | `svc-probe` / `svc-thumb` / `svc-image`（ffprobe + 图像算法） | 主后端 3 文件 | 待做 |
| 3 | 内置 Rust 本地 Provider（Range/206、缩略图、切片） | 新增 Provider | 待评估 |

阶段 1 的 Python 侧接入方式：把 `resource_hash.py` 的 `_torrent_hash` /
`canonical_info_hash` 改为调用 Rust，保留 `resolve_resource_hash` 的 httpx
抓取逻辑不动。`svc-hash` 暂时以独立进程 + Unix socket 或直接 FFI 接入，
取决于阶段 0.5 的结论。


