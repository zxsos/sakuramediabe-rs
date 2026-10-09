# sakuramedia-115-provider

115 网盘 StorageProvider gRPC 插件（Rust）。

上游 Python 插件 [`sakuramedia_115_provider`](https://github.com/tinypinglite/sakuramedia_115_provider)
的 Rust 重写，走同一套插件 ABI（`sm-plugin-api` v0.2.0）。

## 功能

| rpc | 说明 |
|---|------|
| `Browse` | 按 115 目录 id（cid）分页浏览 |
| `ScanImportSource` | 递归枚举文件（server streaming） |
| `PlanPlayback` | 取 115 直链，`redirect` 投放 |
| `GenerateThumbnails` | 暂不支持（`unimplemented`） |
| `GetSpaceUsage` | 115 空间用量 |
| `PrepareLibrary` | 校验 Cookie 有效性，解析媒体/下载目录 |

## 配置

115 认证走 Cookie（与 Python 版一致），**不硬编码**：

| 来源 | 优先级 |
|---|---|
| `LibraryHandle.provider_config` | 1 |
| 环境变量 | 2 |
| `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` | 3 |

环境变量：

- `PLUGIN_115_WEB_COOKIE`：网页端 Cookie
- `PLUGIN_115_DEVICE_COOKIE`：小程序设备 Cookie（优先）
- `PLUGIN_115_MEDIA_ROOT`：媒体根目录（如 `/媒体/电影`）
- `PLUGIN_115_DOWNLOADS_ROOT`：离线下载目录
- `PLUGIN_115_PROVIDER_KEY`：provider key（默认 `115`）

## 构建

```sh
cargo build
cargo test
```

## 许可证

GPL-3.0-or-later
