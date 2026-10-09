# sakuramedia-subtitlecat

SubtitleCat 中文字幕插件，按番号为 SakuraMedia 后端抓取 `zh-CN` 字幕。
是上游 Python 插件
[`sakuramedia_subtitlecat`](https://github.com/tinypinglite/sakuramedia_subtitlecat)
的 Rust 移植。

## 宿主怎么用它

宿主按生命周期协议拉起本仓库产出的**可执行文件**，注入：

| 环境变量 | 含义 |
|---|---|
| `SAKURAMEDIA_PLUGIN_GRPC_ADDR` | 宿主 bind 后分配给插件的控制面地址 |
| `SAKURAMEDIA_PLUGIN_ID` | 插件 id，`register` 要回显它 |
| `SAKURAMEDIA_PLUGIN_DATA_DIR` | 数据目录（本插件暂未使用） |
| `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` | 宿主每次拉起重写的 `settings.json`，本插件只读 |

可执行文件的位置约定是 `<root_dir>/<plugin_id>/<plugin_id>`，所以二进制名必须是
`sakuramedia_subtitlecat`（见 `Cargo.toml` 的 `[[bin]]`）。

## 两个任务

| task_key | 说明 |
|---|---|
| `sakuramedia_subtitlecat_fetch` | 手动抓取单部影片（参数 `movie_number`） |
| `sakuramedia_subtitlecat_fetch_subscribed` | 定时抓取已订阅影片（需宿主提供影片列表，暂未实现） |

`RunJob` 返回事件流：进度事件 + 终态摘要（含 base64 编码的字幕列表，由宿主侧导入）。

## 与上游不同的地方

1. **`state.py` 未移植**：上游用 SQLite 记「已抓取」避免重复抓；Rust 侧暂未实现，
   每次调用都实时抓取。
2. **配置从环境给的文件读**，不是 `context.settings`。
3. **用 `reqwest`（async）替代 `httpx`**。

## 配置项

| 项 | 范围 | 默认 |
|---|---|---|
| `request_timeout_seconds` | >0..120 | 20 |
| `request_retries` | 0..3 | 2 |
| `release_age_months` | 1..120 | 3 |
| `base_url` | 任意 | `https://subtitlecat.com/` |

`base_url` 是对上游的偏离，为的是让测试完全离线。

## 构建与测试

```bash
cargo build --release   # 产物 target/release/sakuramedia_subtitlecat
cargo test              # 解析、番号规范化、配置、base64 的单测
```

**测试不许联网**：HTTP 交互一律走 `wiremock` 本地假服务。

## 许可证

GPL-3.0-or-later（见 `LICENSE`）。
