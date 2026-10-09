# ADR · 补齐依赖选型（Rust 生态优先 / 最小占用）

**日期**：2026-10-04
**状态**：已接受（部分存在未闭环项，见文末）
**决策者**：项目作者 + AI 协作

---

## 1. 背景与约束

`sakuramediabe-rs` 是上游 Python/FastAPI 后端的 Rust 重写。补齐依赖时需同时满足：

| 约束 | 来源 |
|---|---|
| API 契约逐字节不变（客户端是 Flutter） | `Cargo.toml:39-40` 注释 |
| 40 张表 schema 冻结，不引迁移框架 | `crates/sm-db/src/lib.rs:5-7` |
| `hashing` / `media-file-hash` / `svc-hash` 保持零第三方依赖 | `README.md:50-61` |
| `unsafe_code = forbid` | `Cargo.toml:24-25` |
| NAS / 离线环境可构建 | `README.md:57` |
| **纯 Rust 优先，其次性能强、占用小** | 本轮新增 |

迁移前唯一依赖数基线：**251**（`cargo tree --workspace -e normal` 去重实算）。

---

## 2. 决策

| 缺口 | 决策 | 纯 Rust | 直接依赖数 |
|---|---|:---:|:---:|
| 调度 | `cron` 0.17 + 自研 tick（**替换** `tokio-cron-scheduler`） | ✅ | 5 |
| 图像解码/缩放 | `image` 0.25.10，`default-features = false` + `["jpeg","png","webp"]` | ✅ | 19（裁剪后 ~6） |
| WebP **无损** | `image/webp` → `image-webp` 0.2.4 | ✅ | — |
| WebP **有损** | **无纯 Rust 实现** → 进程外 `cwebp` CLI | ❌ | 0 |
| 封面书脊 Sobel | **自写**（`crates/svc-image/src/cover_split.rs`），不引 `imageproc` | ✅ | 0 |
| 媒体探测 | `trait MediaProbe`，默认 `ffprobe` CLI；可替换 `symphonia`（纯 Rust） | ⚠️ | 0 |
| XML（Torznab） | `quick-xml` 0.42 | ✅ | 6 |
| HTML（JavDB） | `scraper` 0.27 | ✅ | 9 |
| 上传 | axum `multipart` feature（内部即 multer） | ✅ | 0 |
| 流式响应 | `tokio-util` + `Body::from_stream` | ✅ | ~0 |
| SSE | axum 自带 `response::sse` | ✅ | 0 |
| 签名 URL | 复用 `sm-core::hashing_support` 的自实现 HMAC-SHA256 | ✅ | 0 |
| 日志落盘 | `tracing-appender` 0.2.5 | ✅ | 小 |
| 系统遥测 | `sysinfo` 0.39.6（替代 psutil） | ✅ | 小 |
| 向量检索 | 保持 Qdrant，经 `reqwest` 访问 | — | 0 |
| 配置 | 维持 `config` 0.15（`default-features=false, features=["toml"]`） | ✅ | 0 |
| HTTP 测试替身 | `wiremock` 0.6.5（dev-only） | ✅ | dev |

---

## 3. 反选论证

### 3.1 调度器：为什么不用 `tokio-cron-scheduler`

上游 APScheduler **只负责按 cron 入队**，真正的队列是自建设 `BackgroundTaskRun`
+ `FOR UPDATE SKIP LOCKED` 领取 + 租约续期；而 `sm-db` 已有
`BackgroundTaskRunRepository` / `ClaimedTask` / `TaskOutcome`，队列语义已搬完。
调度器只剩「解析 cron + 到点幂等入队」两件事。

- **`tokio-cron-scheduler` 0.15.1**：自带 job store 与执行器，是完整内存调度器，
  与 DB 持久队列形成**两套状态源**；且最后更新 2025-10-28，维护已停滞。
- **`apalis` 0.7.4**：自带 storage backend，会引入**第二个队列**；且仍在 pre-1.0。

`cron` 0.17 只做解析（依赖仅 chrono/once_cell/phf/serde/winnow），配一个 tick
循环约 150 行。代价：需自己处理 DST/时区边界 —— 上游 16 个 cron 全是 UTC
固定时刻，风险可控。

### 3.2 图像：为什么不引 `imageproc`

`imageproc` 0.27 直接依赖 **13 个** crate，含 `nalgebra`（线性代数）、`rayon`、
`rustdct`、`ab_glyph`。本模块需要的全部算子是「3×3 Sobel + 逐列求和 + argmax」，
自写约 60 行。为一 60 行的卷积拖进整套线性代数栈不划算。

`image` **必须 `default-features = false`**：默认特性 = `rayon + default-formats`，
而 `default-formats` 含 `avif` → `ravif`/`rgb` → **`dav1d`（AV1 解码的 C 绑定）**，
直接违背「纯 Rust + 离线可编」。

### 3.3 视频探测：为什么默认 `ffprobe` 而不是 `ffmpeg-next`

`ffmpeg-next` 9.0 是 unsafe FFI + 需要系统 ffmpeg dev 库，与 `unsafe_code = forbid`
的项目气质和交叉编译目标冲突。纯 Rust 的 `symphonia`（format-isomp4 / format-mkv，
各 4 个纯 Rust 依赖）+ `h264-reader`（SPS → 分辨率）技术上可行，但要自研约 300 行
且只能覆盖 MP4/MKV + H.264。

结论：**面向 trait 编程，默认走 `ffprobe` CLI（构建零链接，镜像本来就有二进制），
纯 Rust 实现作为可替换项**。契约是一个 `trait MediaProbe`，换实现不动调用方。

### 3.4 有损 WebP：Rust 生态的缺口（本轮最重要的发现）

`image-webp` 0.2.4 只实现 VP8L 无损编码。证据两条：

1. 其 `WebPEncoder::new` 文档原文："Only supports \"VP8L\" lossless encoding."
2. `image` 0.25.10 的 `WebPEncoder` **只有 `new_lossless` 一个构造函数** ——
   编译期即可确认（`svc-image` 首次编译就因此报 `E0599`）。

上游有两处**有损** WebP：`actor_service.py:701`（头像 `quality=90, method=6`）与
`video_cover_service.py:66`（封面 `quality=80`）。候选与代价：

| 方案 | 纯 Rust | 代价 |
|---|:---:|---|
| `webp` 0.3.1 | ❌ | libwebp 的 C 绑定，破坏纯 Rust 与离线构建 |
| `cwebp` CLI | ❌ | 构建零链接，运行时依赖二进制（与 `ffprobe` 同层级） |
| 降级为无损 | ✅ | 头像/封面体积涨数倍，不可接受 |

**决策：有损走 `cwebp` CLI**。`svc-image` **不提供** `encode_lossy` 占位函数 ——
与其放一个会静默降级的入口，不如让调用点在编译期就看见这个缺口。

### 3.5 签名 URL：为什么不引 `hmac` crate

`sm-core/src/hashing_support.rs` 已用 inner/outer Sha256 自实现 HMAC，JWT 在其之上
跑通 72 个测试。再引 `hmac` 0.13 + `sha2` 0.11 会形成**同一项目两套 HMAC 实现**，
也破坏 `hashing` crate 的零依赖叙事。代价：需补 RFC 4231 测试向量兜底。

### 3.6 向量：为什么保留 Qdrant

`qdrant-client` 1.19.0 自身依赖 tonic 且版本节奏独立，与 `sm-plugin-api` 的
tonic 0.14 可能不同步 → 同 workspace 两个 tonic 版本。而上游只用 collection 管理 /
upsert / search / alias 切换，接口面小且稳定，用 `reqwest` 足够。

`instant-distance`（纯 Rust HNSW）虽占用极小，但换用它要**重建全部向量索引**且
需自写 payload filter，收益不匹配代价；`arroy` 0.8 经 `heed` 链接 LMDB（C），
非纯 Rust。

---

## 4. 本轮已落地的改动

| 改动 | 位置 |
|---|---|
| 新建 `svc-image` crate：Sobel 封面书脊检测 + WebP 无损编解码 | `crates/svc-image/` |
| `argon2` 0.5 → **0.6**，`password-hash` 0.5 → **0.6**（消除双声明） | `Cargo.toml`、`crates/sm-core/Cargo.toml`、`crates/sm-core/src/password.rs` |
| 删除 `jsonwebtoken = "11"` 死声明（JWT 是自实现 HS256） | `Cargo.toml:75-79` |
| `[profile.release] panic = "abort"` | `Cargo.toml` |
| **`Cargo.lock` 纳入版本控制**（原被 `.gitignore` 忽略） | `.gitignore` |
| `parity/compare_schema.py` 的 `RUST_ROOT` 硬编码 Windows 路径 → 工作区相对路径回退 | `parity/compare_schema.py:43-57` |
| `tokio-cron-scheduler` → **`cron` + 自研 tick**（ADR §2/§3.1 的决策此前只写在文档里） | `Cargo.toml`、`crates/sm-scheduler/` |
| axum 开 `multipart` feature；新增 `extract::Multipart`（强制 8 MiB 上限） | `crates/sm-api/src/extract.rs` |
| SSE 传输骨架 + **13 个**事件名常量（2026-10-09 更正：此前写「10 个」，那是演员流未移植时的漏数） | `crates/sm-api/src/sse.rs` |
| 慢请求日志中间件（`SAKURAMEDIA_SLOW_LOG` 白名单 + `SAKURAMEDIA_SLOW_REQUEST_MS`） | `crates/sm-api/src/middleware/slow_log.rs` |
| 405 走错误信封 + 逐路由回归测试（此前每条 `MethodRouter` 已挂 fallback，缺的是测试） | `crates/sm-api/src/routes.rs`、`tests/method_not_allowed_http.rs` |
| 组合根 `sm-server`：配置 / 池 / 日志 / 路由 / 调度器 / SIGTERM 优雅关闭 | `crates/sm-server/` |
| `DbError::ConstraintViolation` 带上 **SQLSTATE**，新增 `is_unique_violation()` | `crates/sm-db/src/error.rs` |
| Linux 门禁脚本（本仓库原先只有 PowerShell 版） | `scripts/verify.sh` |

### `argon2` 升级的 API 变化（值得记一笔）

password-hash 0.6 里：

- `SaltString`、`Ident` 移入 `phc` 子模块，`rand_core` 指向 **rand_core 0.10**
  （该版本不再提供 `OsRng`）；
- `PasswordHasher::hash_password` **不再接收盐**，改为开 `getrandom` 特性后由 trait
  自动生成；显式控盐另有 `hash_password_with_salt(password, salt: &[u8])`；
- `Ident::new_unwrap` 不再可用 → 改用 `parsed.algorithm.as_str()` 比较。

验证：`cargo test -p sm-core` 的 9 个 password 测试全部通过，PHC 前缀
`$argon2id$` 与「每次哈希用新盐」两条断言保持成立。

---

## 5. 未闭环项（必须先出证据）

| 项 | 阻塞原因 | 验证方式 |
|---|---|---|
| 灰度系数与 OpenCV 对齐 | 当前环境**无 pip**，装不了 `opencv-python` | 真实封面逐像素对拍；`to_gray` 用的是文档浮点公式，OpenCV 内部是 8 位定点，个别像素可能差 1 |
| 自写 Sobel 与 OpenCV 分割点一致 | 同上 | 用上游真实封面比对 `_detect_split_points` 输出 |
| WebP 无损与 Pillow 产物可互解 | 同上（无 Pillow） | Rust 编码 → Pillow 解码双向 |
| `image-webp` 无损编码质量 | 未实测 | 已自证往返逐像素一致（`lossless_round_trip_preserves_every_pixel`） |
| gRPC 插件 ABI 往返开销 | 未做参考插件 | 先做 1 个 local 存储插件打穿流式 RPC |
| ~~`cron` 的 DST/时区边界~~ | **已闭环，但结论与原假设不同** | 见下 |
| 慢 SQL 归因（`db_ms` / `db_queries`） | sqlx 无 peewee 式全局查询钩子 | `sm-db` 发 `tracing` span 后由中间件汇总 |
| 存量 bcrypt 密码哈希 | Rust 侧无 bcrypt 实现 | 引入 bcrypt 校验器（**需拍板新增依赖**） |
| 启动引导任务（`trigger_type = "startup"`） | 就绪判定依赖 `catalog` 域状态 | 该域落地后补 |

### `cron` 的时区：原假设错了，已按上游改正

ADR §3.1 原写「全 UTC 解析 + 单测」。读上游后发现
`src/start/aps.py:361` 是 `CronTrigger.from_crontab(expr, timezone=get_runtime_timezone())`，
而 `get_runtime_timezone()` 取 `TZ` 环境变量 → 系统时区 → 兜底
`Asia/Shanghai`（`src/common/runtime_time.py:16-44`）。所以「每天凌晨 2 点」
是**本地** 2 点，不是 UTC。

`sm_scheduler::RuntimeTimezone` 因此按上游顺序解析，且**不引 `chrono-tz`**：
IANA 时区名交给 `chrono::Local`（Linux 上系统时区就是它），`UTC` 与
`±HH:MM` 单独处理。DST 由系统时区承担；`FixedUtcOffset` **不**跟随 DST
切换，文档里写明了。

同时暴露两处会**静默**出错的方言差异（`crates/sm-scheduler/src/cron_spec.rs`
有逐条测试）：

| 项 | 上游 `from_crontab` | `cron` crate | 不转的后果 |
|---|---|---|---|
| 字段顺序 | 分 时 日 月 周（5 段） | **秒** 分 时 日 月 周（6/7 段） | 16 个任务**全部**编译失败 |
| 星期编号 | 0/7=周日，1=周一 | 1=周日，2=周一（Quartz） | `0 4 * * 1` 从「周一」变「**周日**」——不报错，只错一天 |

> 注：列梯度的最后一步是**除以最大值归一化**，因此 Sobel 核的整体缩放（OpenCV
> 是否除以 8）不影响结果，只有核内权重比例 1:2:1 与灰度系数会影响。

---

## 6. 待引入依赖（按阶段解锁，复制即用）

当前**未写入** `Cargo.toml` —— 不使用的依赖声明是幽灵配置，等对应模块开工时再加：

```toml
[workspace.dependencies]
# 阶段 1（HTTP 骨架 + 调度）
cron = "0.17"
tokio-util = "0.7"
tracing-appender = "0.2"

# 阶段 4/5（catalog 抓取 + transfers）
quick-xml = "0.42"
scraper = "0.27"
sysinfo = "0.39"

[dev-dependencies]
wiremock = "0.6"

# sm-api 需要开 axum 的 multipart（上传插件 zip / 图片）
# axum = { version = "0.8", features = ["multipart"] }
```

---

## 7. 撤销条件

| 决策 | 撤销条件 | 回退目标 |
|---|---|---|
| 自写 Sobel | 与 OpenCV 分割点对拍不通过 | 引入 `imageproc`（接受 `nalgebra` 体积） |
| `ffprobe` CLI | 部署环境缺失导致故障 | 切 `symphonia` 纯 Rust 实现 |
| Qdrant + `reqwest` | 接口面超出 REST 封装承受范围 | 引 `qdrant-client` 并统一 tonic 版本 |
| `cron` + 自研 tick | DST/闰秒引发真实故障 | 换回成熟调度库 |
| 有损 WebP 走 `cwebp` | 镜像无 `cwebp` 且无法加装 | 引 `webp` 0.3.1（接受 libwebp C 绑定） |

---

## 8. 验证证据

| 项 | 命令 | 结果 |
|---|---|---|
| 全量单测 + 集成 | `cargo test --workspace`（PG 16 真实库） | 全绿，0 失败 |
| `svc-image` | `cargo test -p svc-image` | **12 passed / 0 failed** |
| `sm-core` 密码学 | `cargo test -p sm-core` | **72 passed**（含 argon2 0.6 迁移后 PHC 断言） |
| fmt / clippy | `cargo fmt --all` + `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 均通过 |
| schema 对拍 | `python parity/compare_schema.py` | **40/40 张表**一致 |
| 核心原语对拍 | `python parity/compare_core.py` | **64/64** |
| 哈希对拍 | `python parity/compare.py` | **44/44** |
| release 构建（`panic=abort`） | `cargo build --release -p parity-cli` | 成功 |
