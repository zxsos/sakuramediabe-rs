# sakuramedia-javbus-metadata

JavBus 元数据插件，作为 `catalog.metadata_source` 扩展点给 SakuraMedia 后端
按番号补充影片元数据。是上游 Python 插件
[`sakuramedia_javbus_metadata`](https://github.com/tinypinglite/sakuramedia_javbus_metadata)
的 Rust 移植。

归属仓库：[`sakuramediabe-rs`](https://github.com/zxsos/sakuramediabe-rs)
（宿主实现与任务书 `docs/tasks/javbus-metadata.md` 都在那里）。

## 宿主怎么用它

宿主按生命周期协议拉起本仓库产出的**可执行文件**，注入：

| 环境变量 | 含义 |
|---|---|
| `SAKURAMEDIA_PLUGIN_GRPC_ADDR` | 宿主 bind 后分配给插件的控制面地址 |
| `SAKURAMEDIA_PLUGIN_ID` | 插件 id，`register` 要回显它 |
| `SAKURAMEDIA_PLUGIN_DATA_DIR` | 数据目录，宿主保证可读写且重装时保留 |
| `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` | 宿主每次拉起重写的 `settings.json`，本插件只读 |

可执行文件的位置约定是 `<root_dir>/<plugin_id>/<plugin_id>`，所以二进制名必须是
`sakuramedia_javbus_metadata`（见 `Cargo.toml` 的 `[[bin]]`）。

## 交付约定（与上游不同的三处）

1. **图片必须落在 `FetchMovieRequest.delivery_dir` 内**，且再深一层：
   `<delivery_dir>/<uuid>/<file>`。躺在 `delivery_dir` 根下会被宿主判
   `movie_delivery_missing_request_dir`，写到外面是 `movie_delivery_path_escape`。
   上游写的是 `<插件 data_dir>/metadata-tmp/<uuid>/`，不适用。
2. **「没收录」用 `found = false` 的正常响应表达**，不是 `Err`。`Err` 只表示调用
   失败 —— 混起来会让宿主的兜底链路在第一个来源就停下。
3. **配置从环境给的文件读**，不是侧向宿主请求。

## 配置项

| 项 | 范围 | 默认 |
|---|---|---|
| `timeout_seconds` | 1..120 | 20 |
| `base_url` | 任意 | 上游站点 |

`base_url` 是对上游的偏离，为的是让测试完全离线（也是镜像站的入口）；声明写在
`RegisterResponse.settings_schema` 里，由宿主渲染表单。

## 构建与测试

```bash
cargo build --release   # 产物 target/release/sakuramedia_javbus_metadata
cargo test              # 解析、番号压平、配置的内联单测
```

**测试不许联网**：HTTP 交互一律走 `wiremock` 本地假服务。

宿主侧的生命周期集成测试（用 `sm_plugins::supervisor` 真拉起本二进制并走完
`Register` → `FetchMovie` → 交付校验）**不在本仓库** —— 它依赖宿主实现，按
「插件只依赖契约」的拆分原则留在后端仓库，日后由后端以 `[dev-dependencies]`
按 tag 引入本仓库来跑。

## 许可证

GPL-3.0-or-later（见 `LICENSE`）。
