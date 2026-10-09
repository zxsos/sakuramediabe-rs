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
| `media-file-hash` | `media-file-hash-v1` 采样指纹 | 两个 provider 里的重复实现 | 10 |
| `svc-hash` | BT info hash 解析 | **`libtorrent`** | 32 |
| `svc-image` | 封面分割（Sobel）+ 无损 WebP | Pillow / OpenCV 的相关部分 | 12 |
| `sm-core` | JWT / Argon2 / 签名 URL / 分页原语 / 统一错误信封 | — | 91 |
| `sm-db` | **模型映射 40/40 + 仓储层 + DDL 生成器** | `model/` + 部分 Peewee → sqlx | 358 |
| `sm-service` | 业务规则（**collections + videos + system(8) + playback(3) 四域部分**） | 114 个 service 文件中的 19 个 | 230 |
| `sm-api` | 错误信封 / 鉴权 / CORS / 端点 / multipart / **query 信封** / SSE 骨架 | FastAPI 路由层 | 80 |
| `sm-scheduler` | cron 解析 + 到点幂等入队 | APScheduler 的调度那一半 | 29 |
| `sm-server` | 组合根（配置/池/日志/HTTP/调度器/优雅关闭） | uvicorn + 独立 APS 进程 | 18 |
| `plugin-ref-local` | gRPC 参考插件（把本地目录包成 StorageProvider） | 插件 ABI 可行性验证 | 13 |
| `parity-cli` | 对拍入口（开发工具） | — | — |

- `cargo test`：**996 passed / 0 failed / 3 ignored**（含真实 PostgreSQL 集成测试）
- `python parity/compare.py`：**44/44** Rust 与 Python 逐条一致
- `python parity/compare_core.py`：**64/64** 核心原语逐条一致
- `python parity/compare_schema.py`：**40/40** 张表列名/类型/可空性一致

### 重构进度

| 层 | 规模 | 已完成 | 进度 |
|---|---|---|---|
| 模型 `model/` | 40 表 | 40 表 | **100% ✅** |
| 仓储层 | 40 张表的读写 | 40 张表 | **100% ✅** |
| 服务 `service/` | 114 文件 / 25,174 行 | `collections` + `videos` + `system`(8/11) + `playback`(3/19) | **~19%** |
| Schema `schema/` | 44 文件 | DTO 随端点落地（`sm-api::dto`） | 按需 |
| API `api/` | 126 端点 | 17 个（auth 2 + config 2 + playlists 7 + status 4 + indexer-settings 2） | **~13%** |
| 调度 `scheduler` | 19 个内建任务 | 16 个 cron 已注册（只入队） | **~84%** |
| 插件 ABI `provider_protocol.py` | 543 行 / 30 方法 | 参考插件（4/37 rpc）+ 可行性实测 | **~3%** |

> 逐域台账（已落规则 / 刻意不复刻 / 待核对项）见
> [docs/service-progress.md](docs/service-progress.md)。**为什么只有 17 个端点**
> 与「哪些端点在等哪个域」也记在那里 —— 百分比本身看不出这些。

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
bash scripts/verify.sh                 # 全部七道门禁（Linux/macOS）
bash scripts/verify.sh --skip-tests    # 跳过要连库的测试
powershell -File scripts/verify.ps1    # Windows 等价物
```

七道门禁：`cargo fmt --check` → `cargo doc -D warnings` →
`clippy -D warnings` → `cargo test` → schema 对拍 → 哈希对拍 →
核心原语对拍，外加一道「`paged_list!` 手写包装」静态检查。

**前置**（`scripts/verify.sh` 的 preflight 会逐项检查并给出可执行的修复提示）：

| 依赖 | 版本 | 说明 |
|---|---|---|
| Rust | ≥ 1.85（`rust-version`） | 另需 `clippy` 与 `rustfmt` 组件 |
| PostgreSQL | 16 | `docker compose up -d`（127.0.0.1:**5433**，timezone **UTC**） |
| C 工具链 | 任意 | `build-essential` —— 缺 `cc` 时链接失败 |
| 上游源码 | — | `git clone --depth 1 <repo> upstream/sakuramediabe`（`schema` 对拍要读它的 Peewee 模型） |

集成测试读 `SMDB_TEST_DATABASE_URL`（回退 `DATABASE_URL`）。
**没设时会 panic 而不是跳过** —— 此前「拿不到库就跳过」让 151 个集成测试
在无库环境下显示为「通过」，藏了至少六个真实缺陷；`sm_db::testing::db`
的文档记着那六个。想在无库环境跑单元测试用 `cargo test --lib`。

时间列是 `timestamp without time zone`，**时区必须为 UTC**：容器时区不是
UTC 时 PG 写入/读出的 naive datetime 会整体偏移，且这种偏移不报错，只让
时间字段静默错位。

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

`parity/` 下另有三个辅助脚本：

- `gen_bencode_fixtures.py` —— 用标准编码器生成 bencode 测试数据并自校验。
  bencode 的长度前缀极易写错（`13:meta version` 的 13 是**字符数**，
  而 `meta version` 13 个字符、`file tree` 只有 10 个），手写几乎必错。
- `trace_bencode.py` —— 逐行模拟 `parse_torrent`，用于定位解析失败点。
  写 Rust 测试时先在这里验证，能省掉大量「为什么 unwrap 挂了」的往返。
- `check_paged_wrappers.py` —— 第七道门禁。`paged_list!` 自己会生成整个方法，
  在外面再手写一层包装会让函数体返回 `()`：编译器会报，但指向宏展开处而
  不是真正的错误位置。这个错在重构里犯了五次，每次都要等编译失败才发现。

## 路线图

| 阶段 | 内容 | 状态 |
|---|---|---|
| **0** | 工具链 / PG 16 / 上游 clone / 七道门禁（`scripts/verify.sh`） | **完成** |
| **1** | `sm-core` 原语（JWT / Argon2 / 签名 URL / 分页） | **完成** |
| **2** | `sm-db` 模型 40/40 + 仓储层 + DDL 生成器 | **完成** |
| **3** | `sm-service` 最小域定型：`collections` → `videos` | **完成** |
| **4** | `sm-api` 骨架：错误信封 / 鉴权 / CORS / 405 / 慢日志 / multipart / SSE 骨架 | **完成** |
| **5** | `sm-scheduler`（cron + 自研 tick，只入队）+ `sm-server` 组合根 | **完成**（worker 未做） |
| **6** | `sm-service` 剩余域：`system`(15) → `playback` → `transfers` → `discovery` → `catalog` | 进行中（`system` 落了 4 个） |
| **7** | 后台任务 **worker**（队列侧已就位，只差 handler 分发） | 进行中 |
| **8** | 插件 ABI：**参考插件已落地**（`plugin-ref-local`，4/37 rpc + 开销实测 + 4 条 P1 缺口）；宿主侧 `sm-plugin-api` 重生成与 `sm-plugins` 未做 | 进行中 |
| **9** | `svc-probe`（ffprobe）与封面生成的有损 WebP 路径 | 待做 |

`playlists` 域已落 7/9 个端点。剩下的 `GET /playlists/{id}/movies` 卡在
**影片卡片聚合**（`with_movie_card_relations` / `attach_movie_list_media` /
`MovieListItemResource`）—— 它与 `catalog` 域的影片列表端点是同一套东西，
所以**刻意不单独做**，等 `catalog` 侧开工时一起落地。

原先记在这里的「`playback` 的 `movie_resolution_service` 被 `collections`
的 4 个列表端点引用，所以 `playback` 早于 `catalog`」这条约束**已解除**：
`resolution_interval` / `resolution_level_expression` / 档位分桶已随
`sm_service::catalog::resolution` 落地。剩下的 `movie_resolution_service`
职责只有影片卡片的封面聚合，那属于 `catalog` 域自身。

`svc-hash` / `media-file-hash` 的 Python 侧接入（改
`resource_hash.py` 调 Rust）仍按 ADR §6 的「独立进程 + Unix socket 或 FFI」
待定 —— 那是部署形态问题，与本仓库的代码进度无关。


