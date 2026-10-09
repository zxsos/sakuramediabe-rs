# sakuramedia-more-movies

更多影片 / 更多影片榜单插件的 Rust 移植，一个仓库两个插件：

| 二进制 / `plugin_id` | 上游 Python 插件 | 功能 |
|---|---|---|
| `sakuramedia_more_movies` | [`sakuramedia_more_movies`](https://github.com/tinypinglite/sakuramedia_more_movies) | 定时抓取 JavDB 最新影片（有码/无码/FC2），热度达标且主库没有的自动入库 |
| `sakuramedia_more_rank_movies` | [`sakuramedia_more_rank_movies`](https://github.com/tinypinglite/sakuramedia_more_rank_movies) | Minnano AV 日榜/周榜/月榜 + JavLibrary 高评价/最想要榜单，`discovery.ranking_source` 扩展点 |

归属：[`zxsos/sakuramediabe-rs`](https://github.com/zxsos/sakuramediabe-rs)（宿主）。

## 任务与扩展点

- `sakuramedia_more_movies_sync`（cron `0 6 * * *`）：JavDB 最新列表 → 查重
  （`PluginHost::FindMoviesByNumbers`）→ 详情热度门槛 → 入库
  （`PluginHost::ImportMovieByNumber`）。
- `sakuramedia_more_rank_movies_sync`（cron `45 1 * * *`）：调宿主
  `PluginHost::SyncRankingSources` 做全量同步。
- `discovery.ranking_source`：Minnano AV（`minnano_av`，daily/weekly/monthly）
  与 JavLibrary（`javlibrary_bestrated`、`javlibrary_mostwanted`，
  monthly/all）的榜单声明；取数走 `RankingSourceExtensionService::FetchRanking`。

> 注意：上游的「手动单榜同步」任务（`sakuramedia_more_rank_movies_sync_board`）
> 在 v0.2.0 契约里没有对应的宿主 RPC（`SyncRankingBoard` 是后加的），
> 所以本移植不声明它；契约升级后再补。

## 与上游不同的地方

1. **JavDB 列表走插件自己的 HTTP**，不是宿主的 `JavdbProvider`。Rust 契约
   （v0.2.0）没有「任意 JavDB 请求」的宿主 RPC，只有榜单查询；签名算法
   （`jdsignature`）是公开的（上游 `javdb.py:_get_sign`），插件自己实现。
2. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**（宿主写文件 + 环境变量指路），
   不是进程内插件的 `context.settings`。
3. **回调宿主走 `SAKURAMEDIA_HOST_GRPC_ADDR`**（宿主 `PluginHost` 服务地址）。

## 构建与测试

```bash
cargo build
cargo test
```

测试不联网：HTTP 用 wiremock，HTML 解析用内联 fixture。
