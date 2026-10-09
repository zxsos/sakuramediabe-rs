# sakuramedia-javdb-ranking

JavDB 排行榜插件的 Rust 实现：`discovery.ranking_source` 扩展点。

上游：`tinypinglite/sakuramedia_javdb_ranking`（Python）。

## 榜单

| board_key | 显示名 | 周期 |
|---|---|---|
| `hot` | 热播 | 无 |
| `top_rated` | 高评分 | daily/weekly/monthly |
| `censored` | 有码 | daily/weekly/monthly |
| `uncensored` | 无码 | daily/weekly/monthly |
| `fc2` | FC2 | daily/weekly/monthly |
| `top250` | TOP250 | 动态（年份） |

## 定时任务

`sync_rankings`：每天 03:00 全量同步（cron `0 3 * * *`），由宿主调度。

## 构建

```bash
cargo build --release
```

二进制名必须为 `sakuramedia_javdb_ranking`（宿主按
`<root_dir>/<plugin_id>/<plugin_id>` 查找）。

## 测试

```bash
cargo test
```

测试不联网：榜单页用 wiremock 本地模拟。
