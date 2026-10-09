# 部署形态与瘦身路线

**为什么有这份文档**：本仓的重构动机不是「换个语言」，而是**上游那个 4GB 的
Python 应用镜像**。目标形态是「保证 Flutter 客户端无感」的前提下，用**一个静态
二进制**替掉它。这份文档把这件事算清楚：钱花在哪、哪些能省、**什么时候能删掉
Python**，以及替换时怎么不出事。

> 数字口径：体积是量级估算（不同平台/构建差别不小），但**「谁在用」是逐条 grep
> 出来的，带行号**。前者用于判断优先级，后者用于判断可行性 —— 后者才是硬证据。

---

## 一、目标与约束

| 项 | 内容 |
|---|---|
| **目标** | 用 Rust 二进制替换上游 Python 后端；产物体积与运行时内存大幅下降 |
| **硬约束** | **Flutter 客户端契约冻结**。前端是已发布的多平台客户端（`sakuramedia/` 下 `android` / `ios` / `macos` / `windows` 俱全），改契约 = 要求用户升级 App |
| **软约束** | 后端地址/端口/路径不变（客户端可能硬编码）；错误信封、字段名、分页形状逐字一致 |
| **非目标** | ~~「零容器、单文件」~~ —— PostgreSQL / Qdrant / 推理服务仍应是独立容器，详见 §四 |

契约冻结这件事本仓已经在用**对拍**兜住（`compare.py` 44/44、`compare_core.py`
64/64、`compare_schema.py` 40/40）。**继续守着它，这是唯一能保证「前端无感」的东西。**

---

## 二、现在能跑到什么程度

`sm-server` 是组合根（单进程同时承载 API 与调度器，替代上游 supervisor 管的
`uvicorn` + `aps` 两个进程）。本机实测（2026-10-07）：

```powershell
pwsh -File scripts/dev-services.ps1 up                 # PostgreSQL 5433 + Qdrant 6334
podman cp docker/schema.sql sakuramedia-rs-pg:/tmp/schema.sql
podman exec sakuramedia-rs-pg psql -h 127.0.0.1 -p 5433 -U sakuramedia `
  -d sakuramedia_test -v ON_ERROR_STOP=1 -f /tmp/schema.sql     # 建 40 张表

cargo build -p sm-server
$env:SAKURAMEDIA_DATABASE_URL = 'postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
$env:SAKURAMEDIA_JWT_SECRET   = 'dev-secret'      # 空密钥拒绝启动（退出码 2）
$env:SAKURAMEDIA_SCHEDULER_ENABLED = '0'
.\target\debug\sm-server.exe
```

实测输出：

```text
INFO sm_server:  sakuramediabe-rs 启动中 version="0.1.0"
INFO sm_server: 数据库连接池就绪 max_connections=20
INFO sm_server: 调度器已被配置关闭
INFO sm_server: HTTP 服务已监听 address=127.0.0.1:8000

GET /definitely-not-a-route → 404 {"error":{"code":"http_error","message":"Not Found"}}
GET /playlists              → 401 {"error":{"code":"unauthorized","message":"Authentication required"}}
```

**能跑 ≠ 能替代**：端点方法级 175/177 已注册，但其中 **75 条 handler 仍是
`todo!()`**（口径见 `progress-baseline.md`）。所以现在它是「一个装好路由表、
大部分业务还没实现的真后端」。

---

## 三、那 4GB 的账：逐条对应到 Rust 侧

上游后端镜像 = `python:3.10-slim-bookworm` + apt（`supervisor`/`ffmpeg`）+
77 个 Python 依赖。按**「谁在用」**逐个查（grep 结果带行号）：

| 上游装的 | 量级 | 上游的**全部**用处 | Rust 侧对应物 | 现状 |
|---|---|---|---|---|
| `opencv-python-headless` | 300–500MB | `movie_image_service.py:101,151`（封面书脊分割） | `svc-image` 自写 Sobel | ✅ 已落地（12 测试） |
| `numpy` | ~50MB | 同上，**唯一用处** | 不需要 | ✅ |
| `libtorrent` | 100–150MB | `resource_hash.py:34`、`local_provider/qbittorrent.py:799`、`115_provider/offline.py:101` | `svc-hash`（`canonical_btih`） | ✅ 主后端侧已落（32 测试） |
| `av`（PyAV） | 100–200MB | `video_cover_service.py:15`、`media_metadata_probe_service.py:12` | `MediaProbe`（ffprobe / symphonia） | ⚠️ 未落地 |
| `python:3.10-slim` + 其余 70 包 | ~200MB | — | 静态二进制 | ✅ |
| apt `supervisor` + uvicorn + aps | — | `docker/backend/supervisord.conf` 的两个 program | `sm-server` 单进程 | ✅ |
| apt `ffmpeg` | 300–500MB | 视频探测 / 抽帧 | `ffprobe` CLI（同层级） | ⚠️ 按需 |

### 3.1 `libtorrent` 是个**硬结论**，不是猜测

三处用法的**代码形状完全一样** —— 都是「拿种子字节换 info hash」：

```python
# local_provider/qbittorrent.py:797-803
def _parse_torrent_hash(payload: bytes) -> str:
    import libtorrent as lt
    return canonical_btih(str(lt.torrent_info(payload).info_hash()))

# 115_provider/offline.py:99-105 —— 同样的三行
```

而 `canonical_btih` 是**上游自己的函数**，libtorrent 只承担「解析」这一步。
本仓 `svc-hash` 已实现完整的 seed 解析（v1/v2、base32、强制 v1 校验，32 测试）。

> **所以 libtorrent 可以整条链上消失** —— 前提是那两个 provider 插件完成 Rust 化
> （见 §五）。主后端那一处早已不需要它。

### 3.2 `PyAV` 是**可降级**的（降低迁移门槛）

上游对它缺失的态度写得很清楚（`video_cover_service.py:4`）：

> 封面为增益项：PyAV 缺失或解码失败时只记日志、返回 None，绝不阻断导入主流程。

`media_metadata_probe_service.py:293` 同样是 "skipped because pyav is unavailable"。

> 也就是说：**可以做「先能用、后完整」**。媒体探测/视频封面不阻塞「前端可用」。

### 3.3 一个容易忽略的好消息

上游 Dockerfile 第 48 行的注释：

> 主服务只保留自身运行所需的数据目录，**嵌入模型由独立服务管理**。

即**推理服务（SigLIP2）本来就不在这个镜像里** —— 它不是你那 4GB 的来源，
也不是 Rust 化的对手。图搜作为**可选组件**存在，不做也不影响主流程。

### 3.4 Qdrant 容量：**实测**数字（2026-10-08）

> 下面每个数字都是**实测**（本机压测 10 万向量），不是估算。
> 复现方式见本节末尾。

**空载基线**：内存 **51–70 MB**、磁盘 **24 KB – 1 MB**、版本 1.19.1。
端口 **6333 = HTTP/REST**、**6334 = gRPC**（门禁的 `SMVEC_TEST_QDRANT_URL`
指的是 **gRPC**，用 HTTP 客户端打 6334 会得到 `HTTP/0.9` 错误）。

**10 万向量 @1152 维**（SigLIP2 so400m 的常见输出）+ 真实形态 payload
（`media_id` / `offset` / `origin` 路径，约 60 B JSON）：

| 配置 | 内存（稳态） | 磁盘（稳态） | 写入速度 |
|---|---|---|---|
| 默认（HNSW 索引全内存） | 533.8 MB（净增 **480**） | 497 MB | 513 点/秒 |
| **`on_disk: true`** | **71.6 MB**（净增 **17.6**） | 497 MB | 280 点/秒 |

**单位成本（稳态）**：磁盘 **4.97 KB/点**（几乎就是向量本身：1152 × 4 B =
4.5 KB，索引不落盘）；内存默认 **4.8 KB/点**、`on_disk` **0.18 KB/点**（降 96%）。

**按向量数换算**：

| 向量数 | 磁盘 | 内存（默认） | 内存（`on_disk`） |
|---|---|---|---|
| 10 万 | 497 MB | 480 MB | **18 MB** |
| 50 万 | 2.5 GB | 2.4 GB | **90 MB** |
| 100 万 | 5.0 GB | 4.8 GB | **180 MB** |
| 500 万 | 25 GB | 24 GB | **900 MB** |

**建议用 `on_disk: true`**：100 万向量时默认配置吃掉 4.8 GB 内存，而
`on_disk` 只要 180 MB；代价是建索引慢 45%，那是**一次性**成本，对查询延迟
无影响（磁盘 mmap + page cache）。

#### ★ 三个「只有实测才发现」的坑

1. **峰值是稳态的 2 倍且会自己回落。** 写入中磁盘到过 985 MB、内存 942 MB；
   collection 转 `green` 后回落到 497 MB / 534 MB。**拿瞬时值做容量规划会翻倍。**
2. **`yellow` → `green` 之前不要采数。** segment 合并完成前的 `du` 读数虚高
   （同一份 1 万点数据，中途读到 984 MB，稳态是 157 MB）。
3. **磁盘读数会因合并而回落，内存读数会因 GC 抖动。** 采样要挑
   `status == "green"` 的时刻。

#### ⚠️ 本次实测的偏差（**待实测校正**）

- **压测没有建 payload 索引**，而本仓 `dense.rs` 声明了
  `THUMBNAIL_PAYLOAD_INDEX = &["movie_id", "media_id"]`。Qdrant 的 payload 索引走
  mmap，真实部署时内存会**高于**上表（磁盘也略高）。
- **每部媒体有多少张缩略图尚未实测**。当前口头结论是「一部只有几张」，
  据此推算的向量数会**远低于**上表 —— 但**没有实测数据**，所以上表刻意按
  「向量数」给，不按「媒体数 × 每部张数」给。

> **动手前先补这两项实测**，否则会照着一份偏乐观的容量规划上线。
> 复现脚本的形态：随机向量 + `PUT /collections/{name}` 建库（**不是** POST）
> + `PUT /collections/{name}/points?wait=true` 写点（**也不是** POST），
> 采样 `podman stats` 与 `podman exec … du -sm /qdrant/storage`。

---

## 四、三类东西：会消失 / 不会消失

| 类别 | 内容 | 说明 |
|---|---|---|
| **已经消失** | OpenCV、numpy、uvicorn、supervisor、aps | 已被 `svc-image` / `sm-server` 替代 |
| **会消失**（有前置条件） | Python 运行时、libtorrent、PyAV、Pillow | 前置条件 = §五 的判据清单 |
| **不该消失** | PostgreSQL、Qdrant、推理服务、`ffprobe`/`cwebp` CLI | 见下 |

**「不该消失」的三条理由**（别被「零依赖」带偏）：

1. **PG / Qdrant 是数据容器**，不是应用镜像。它们的运维成熟、体积固定，且与
   你的 4GB 无关。把它们塞进应用进程是倒退。
2. **`ffprobe` / `cwebp` 是 ADR 的显式决策**（`2026-10-04-tech-selection.md` §3.3/§3.4）：
   Rust 生态缺有损 WebP 编码器，`image-webp` 0.2.4 只有 VP8L 无损。走 CLI
   是「构建零链接」的代价交换，各约 1MB–50MB。
3. **推理服务是模型容器**。它本来独立，做图搜才需要。

> **终局不是「一个二进制」，而是**：一个 ~30MB 的静态二进制 + 几个小 CLI +
> 原来那几个数据容器。**被干掉的是那个 4GB 的应用镜像。**

---

## 五、砍掉 Python 的判据清单

**逐条可核对**。全部勾上 = 可以把 Python 镜像从部署里删掉。

### 5.1 后端侧

- [x] `sm-server` 单进程替代 uvicorn + aps（含配置、日志、优雅关闭）
- [x] OpenCV / numpy 的用处被 `svc-image` 替代
- [x] `libtorrent` 主后端那处被 `svc-hash` 替代
- [ ] **`svc-probe`（ffprobe）落地** —— 解锁 `media_metadata_probe_service`(338)、
      `media_video_info_backfill_service`(232)、`video_cover_service`
- [ ] **zip 实现** —— 解锁 `media_thumbnail_pack_backfill_service`(216)（仓库当前无 zip crate）
- [ ] `catalog` 入库路径（插件元数据 → 库表），见 §6 第 3 条
- [ ] 剩余域推进到「客户端用到的路径都有实现」（`playback` 16 / `catalog` 9 / `transfers` 10 个文件）

### 5.2 插件侧 —— **这才是胜负手**

宿主按约定 `<root_dir>/<plugin_id>/<plugin_id>` 拉起**可执行文件**
（`sm-server/src/plugins.rs` 的模块文档），**Python 插件拉不起来**。
所以只要还有一个 Python 插件在跑，Python 运行时（+ 它声明的库）就一个都省不掉。

| 插件 | 体量（py，不含 tests） | 状态 | 难度 |
|---|---|---|---|
| `sakuramedia_javbus_metadata` | 12 KB | ✅ **已完成 Rust 移植** | — |
| `sakuramedia_judge_collecttion_movie` | 5.7 KB | ✅ **已完成 Rust 移植**（独立仓 `sakuramedia-judge-collecttion-movie`）| — |
| `sakuramedia_javdb_ranking` | 10 KB | ⬜ | 小 |
| `sakuramedia_subtitlecat` | 23 KB | ✅ **已完成 Rust 移植**（独立仓 `sakuramedia-subtitlecat`）| — |
| `sakuramedia-actor-metadata` | 30 KB | ✅ **已完成 Rust 移植**（独立仓 `sakuramedia-actor-metadata`）| — |
| `sakuramedia_local_provider` | **172 KB** | ⬜ | **难 —— 分水岭** |
| `sakuramedia_115_provider` | **251 KB** | ⬜ | **最难 —— 最后一关** |

> `manifest.json` 的 `dependencies` 是**插件自己的依赖声明**（例如
> `local_provider` 声明了 `libtorrent` / `qbittorrent-api`）。上游插件
> `pyproject.toml` 里没有 `dependencies` —— 依赖由宿主环境统一装。这解释了
> 为什么**最后一个 Python 插件决定整个运行时能不能删**。

---

## 六、瘦身路线图（按依赖与性价比排序）

### 阶段 A · 小插件扫尾（最小成本，验证路径）

`judge_collecttion_movie`(5.7KB) → `javdb_ranking`(10KB) → `subtitlecat`(23KB)
→ `actor-metadata`(30KB)。四个合计不到 `local_provider` 的一半。

**判据**：每个插件都有 `<plugin_id>` 二进制、宿主能拉起、扩展点声明被
`collect_extensions` 收下。已有样板：`plugin-ref-local` 与 `javbus-metadata`。

**进度（2026-10-08）**：两个已移植，各在新仓里自带 46 / 49 项测试（都**真的拉起
本仓二进制**跑完整流程）：

- `judge_collecttion_movie` → `sakuramedia-judge-collecttion-movie`，它要的宿主能力
  **也已接线**（`ListMovies` / `PatchMovie`，身份走「每个插件一个能力出口端点」，
  见 [`plugin-abi.md`](plugin-abi.md) 的已知缺口；快照补齐 6 个可写字段）。
- `subtitlecat` → `sakuramedia-subtitlecat`，抓取/解析自足，**宿主侧的
  `ImportSubtitle` 也已接线**（转发 `sm-service` 的 `SubtitleAssetService`：查影片 →
  扩展名白名单 → 内容 sha256 去重 → 落盘 → 登记）。跨仓冒烟里它跑完了整条链路：
  宿主拉起二进制 → 插件去假站点抓 → 回调 `ImportSubtitle` → 库里多一行字幕、文件落在
  图片根下面。

**只剩一个**：`javdb_ranking` 要宿主提供的 JavDB 客户端（或把抓取整段搬进插件）——
`actor-metadata` 那组演员 rpc（`ListActors` / `PatchActor`）与影片快照的 `actors` 都已
接线。逐条清单见
[`handoff.md`](handoff.md) §8.2 表后的块。

### 阶段 B · **入库路径**（让插件产出真的有用）

`docs/tasks/javbus-metadata.md` §二 明确写着：

> **入库路径没有**：catalog 域缺「插件元数据 → 入库」的服务，所以拿到校验过的
> 结果也没处写。

**不做这一步，阶段 A 的插件全是空转**（数据校验完就丢了）。这是当前**最被
低估的缺口**。

### 阶段 C · `svc-probe`（ffprobe）

解锁 4 个文件（§5.1）。注意上游对 PyAV 缺失是**降级**，所以它不阻塞「能用」，
但阻塞「完整」。

### 阶段 D · **`local_provider` 的 Rust 版（分水岭）**

> **前置**：P1-2（`PlaybackPlan` 加 `local_path`）必须先定 —— 它是第一个真会用到
> 那个 delivery 的插件。见 [`tasks/proto-p1-gaps.md`](tasks/proto-p1-gaps.md) §三。

做完它同时发生三件事：

1. **本地媒体库 + 播放闭环** —— 不依赖任何网盘；
2. **libtorrent 可以整条扔掉**（§3.1 的结论）；
3. 宿主第一次承载一个「真的在干活的 provider」。

它的逻辑其实直白：文件系统 + qBittorrent 的 HTTP API（`qbittorrent-api`
走 HTTP，不需要 libtorrent）。体积大是因为功能多（`storage.py` 75KB +
`qbittorrent.py` 44KB + `merged_mp4.py` 32KB）。

### 阶段 E · `115_provider`（最后一关）

251KB，含 HLS 读取、range reader、加密、离线下载。**只有它完成，Python 运行时
才能真正删掉**。只影响网盘用户，所以放最后。

### 阶段 F · 可选：图搜

推理服务 + Qdrant 独立容器。不做也不影响主流程。

---

## 七、替换时的部署形态：三层 + 一条退路

| 层次 | 形态 | 现在能不能做 |
|---|---|---|
| **L1 源码跑** | `cargo build -p sm-server` + 手建表（§二） | ✅ **已验证** |
| **L2 单容器** | 静态二进制塞进一个极小镜像，替掉 4GB 那个 | ❌ 缺 Dockerfile；且需 §五 清单基本勾完 |
| **L3 灰度双跑** | 反向代理按路径分流，两套后端共用一个 PG 库 | ⚠️ 技术前提已满足（见下），但无人做过 |

### L3 的关键前提**已经满足**

`compare_schema.py` 是 **40/40** —— Rust 侧模型与上游 Peewee 逐列（名/类型/可空性）
一致。也就是说：

```text
Flutter 客户端 ──► 反向代理 ──┬── 已完成的域（/playlists、/daily-recommendations…）→ sm-server
                              └── 其余路径 ──────────────────────────────► 原 Python 后端
```

**同一个 PostgreSQL 库可以被两边读写**，出问题一条路由切回去，**前端完全无感**。

> 这是整个重构最强的安全网，也是「渐进式」三个字真正的兑现方式。
> 它需要一个反向代理配置（nginx/Caddy）与一次真机演练 —— 目前都没有。

### L2 之前**不要**动的两件事

1. **端口与路径**：`sm-server` 默认 `0.0.0.0:8000`（`config.rs` 的 `ListenConfig`），
   与上游 uvicorn 一致。**保持它** —— Flutter 客户端可能硬编码。
2. **`SAKURAMEDIA_*` 环境变量名**：`config.rs` 模块文档写明「它**已经是部署契约**
   （`docker-compose.yml` 在用），不为了更标准而改」。

---

## 八、待拍板项（别自己决定）

| # | 事项 | 影响 |
|---|---|---|
| 1 | **契约分叉 + P1-2**：本仓 `proto/storage.proto` 已含 P1-1 修订，而契约仓 tag `v0.1.0` 仍停在旧版（`ABI_MAJOR` 两边同为 1，**检查不到**）；P1-2（`PlaybackPlan` 缺本地路径 delivery）仍未做 | 分叉现状会让 `GenerateThumbnails` 两侧消息类型不一致 —— 线上表现为一个**指错方向**的解码错误。`P1-1` / `P1-3` / `P1-4` 其实都已落地，真正待办的是「修分叉 + 决策 P1-2」，详见 [`tasks/proto-p1-gaps.md`](tasks/proto-p1-gaps.md) |
| 2 | **`data_plane_endpoint` 是否写成字节搬运的硬性要求** | 否则 4MB 默认消息体会成为隐形天花板（同报告 §5.2） |
| 3 | **L3 灰度是否要做** | 决定要不要写反向代理配置与演练预案 |
| 4 | `system/telemetry.rs` 去留 | 既有待拍板项，见 `handoff.md` §7.5 |

---

## 九、怎么验证这份文档没写错

- **数字**：`pwsh -File scripts/progress.ps1 -Diff`（应回 `OK`）—— 端点/`todo!()` 口径
- **体积归属**：本文每个「谁在用」都给了文件:行号，可逐条 grep 复核
- **能跑**：§二 的四步照做一遍（20 分钟内）
- **契约**：`cargo test --workspace`（含真库集成）+ `python parity/compare_schema.py`
