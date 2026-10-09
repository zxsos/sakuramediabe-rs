# sakuramedia-judge-collection

合集判定插件：按影片时长 / 番号特征 / 标签自动标记合集影片，不覆盖 App 内的手动判定。
是上游 Python 插件
[`sakuramedia_judge_collecttion_movie`](https://github.com/tinypinglite/sakuramedia_judge_collecttion_movie)
的 Rust 移植。

归属仓库：[`sakuramediabe-rs`](https://github.com/zxsos/sakuramediabe-rs)（宿主实现）。

## 判定规则（与上游一致）

对每部影片，按顺序：

1. 时长 `<` 阈值（默认 300 分钟）**且**番号不命中特征 **且** 标签不命中 → 不动；
2. 已是合集（`is_collection` 为真）→ 不动；
3. `is_collection` 的 owner 存在且不是本插件（`plugin:<plugin_id>`）→ 跳过，不覆盖手动判定；
4. 否则 `patch(movie_id, {"is_collection": true}, expected_revision)`。

番号归一化：去空格、大写、`_` → `-`、去掉 `PPV-` 前缀；纯 `数字-数字` 形状原样返回。
番号特征命中：归一化番号以前缀集合任一项开头，或以后缀集合任一项结尾（默认前缀 `OFJE,CJOB,DVAJ,REBD`）。

## 宿主怎么用它

宿主按生命周期协议拉起本仓库产出的**可执行文件**，注入：

| 环境变量 | 含义 |
|---|---|
| `SAKURAMEDIA_PLUGIN_GRPC_ADDR` | 宿主 bind 后分配给插件的控制面地址 |
| `SAKURAMEDIA_PLUGIN_ID` | 插件 id，`register` 要回显它 |
| `SAKURAMEDIA_PLUGIN_DATA_DIR` | 数据目录（本插件未使用） |
| `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` | 宿主每次拉起重写的 `settings.json`，本插件只读 |
| `SAKURAMEDIA_PLUGIN_HOST_ADDR` | 宿主 `PluginHost` 回调用地址；**宿主目前不注入**，缺省时 `run_job` 回 `unimplemented` |

可执行文件的位置约定是 `<root_dir>/<plugin_id>/<plugin_id>`，所以二进制名必须是
`sakuramedia_judge_collecttion_movie`（拼写沿用上游，见 `Cargo.toml` 的 `[[bin]]`）。

`register` 声明一个 `JobDefinition`：`task_key = sakuramedia_judge_collecttion_movie`，
`default_cron = 0 4 * * *`（每天 04:00），与上游一致。

## 契约

`zxsos/sakuramedia-plugin-api` 的 `v0.2.0` tag（与宿主 `ABI_MAJOR = 2` 配套），按 tag 依赖。

## 本地验证

```sh
cargo build
cargo test   # 16 项：判定分支、归一化、配置、扫描主循环（含分页/owner/失败计数）
```
