# sakuramediabe-rs

SakuraMedia 后端的 Rust 实现。

> **这是 [`sakuramediabe`](https://github.com/tinypinglite/sakuramediabe)
> （Python/FastAPI，作者 tinypinglite）的衍生作品**，与其同以
> **GPL-3.0** 授权。取自上游的代码范围见 [NOTICE.md](NOTICE.md)。

高性能、内存安全的媒体库管理后端，提供完整的 REST API、插件系统与定时任务调度。

## 特性

- **完整的 REST API**：126 个端点，覆盖影片、演员、合集、播放列表、下载、系统管理等
- **插件系统**：gRPC 插件架构，支持存储、元数据、字幕、翻译等扩展
- **定时任务**：内置 cron 调度器，支持幂等任务执行
- **向量搜索**：集成 Qdrant，支持以图搜图与语义搜索
- **高性能**：Rust 原生性能，无 GIL 限制，支持多核并发

## 架构

| crate | 作用 |
|---|---|
| `sm-server` | HTTP 服务入口（配置/连接池/日志/调度器/优雅关闭） |
| `sm-api` | REST 端点 / 鉴权 / 错误信封 / CORS / multipart / SSE |
| `sm-service` | 业务逻辑层（7 个业务域） |
| `sm-db` | 数据访问层（40 张表 / 仓储模式 / sqlx） |
| `sm-core` | 基础原语（JWT / Argon2 / 签名 URL / 分页） |
| `sm-plugins` | 插件宿主（加载 / 注册 / 生命周期管理） |
| `sm-plugin-api` | 插件 gRPC 契约定义 |
| `sm-scheduler` | cron 调度器与任务队列 |
| `hashing` | SHA-1 / SHA-256 / Base32（零依赖） |
| `media-file-hash` | 媒体文件指纹（`media-file-hash-v1`） |
| `svc-hash` | BT info hash 解析 |
| `svc-image` | 封面处理（Sobel 分割 / WebP / EXIF 转正） |

## 快速开始

### 依赖

| 依赖 | 版本 | 说明 |
|---|---|---|
| Rust | ≥ 1.85 | 需 `clippy` 与 `rustfmt` 组件 |
| PostgreSQL | 16+ | 时区必须为 UTC |
| Qdrant | 1.x | 向量搜索引擎（可选，用于以图搜图） |

### 构建

```bash
cargo build --release
```

### 配置

```bash
export SAKURAMEDIA_DATABASE_URL="postgres://user:pass@localhost:5432/sakuramedia"
export SAKURAMEDIA_JWT_SECRET="your-secret-key"
# 可选
export SAKURAMEDIA_QDRANT_URL="http://localhost:6333"
```

### 运行

```bash
./target/release/sm-server
```

服务默认监听 `0.0.0.0:8000`。

### 鉴权

```bash
# 获取 token
curl -X POST http://localhost:8000/auth/tokens \
  -H "Content-Type: application/json" \
  -d '{"username": "admin", "password": "password"}'

# 使用 token
curl http://localhost:8000/movies \
  -H "Authorization: Bearer <access_token>"

# API Key（sk- 前缀）
curl http://localhost:8000/movies \
  -H "Authorization: Bearer sk-your-api-key"
```

## 插件

插件是独立的 gRPC 进程，通过 `sm-plugins` 宿主管理。

已发布的官方插件：

| 插件 | 功能 |
|---|---|
| 115-provider | 115 网盘存储（浏览/导入/离线下载/直链播放） |
| plugin-ref-local | 本地目录存储（参考实现） |
| javbus-metadata | JavBus 元数据刮削 |
| javdb-ranking | JavDB 排行榜 |
| actor-metadata | 演员元数据 |
| judge-collection | 合集评分 |
| scrape-translate | 刮削翻译 |
| subtitlecat | 字幕下载 |
| more-movies | 更多影片源 |

安装插件：

```bash
curl -X POST http://localhost:8000/plugins \
  -H "Authorization: Bearer <token>" \
  -F "file=@plugin.zip"
```

## 开发

```bash
# 运行测试
cargo test

# 代码检查
cargo fmt --check
cargo clippy -- -D warnings

# 生成文档
cargo doc --no-deps
```

## 许可证

GPL-3.0-or-later
