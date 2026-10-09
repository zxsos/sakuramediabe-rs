# 重构进度

| 指标 | 现在 |
|---|---|
| 未实现的方法体（`todo!()`） | **136** 处（`sm-service` 74 + 路由 62）|
| 端点（方法级） | 175 / 177 已注册，**其中 62 条仍是 `todo!()`** |
| 端点（路径级） | 136 / 136（未注册的方法级端点 2 条）|
| 完成的域 | `collections`、`videos`（2/7 个域） |
| 待办最多的域 | `transfers` 27 · `catalog` 24 · `playback` 16 |
| worker handler | 5 / 21 |
| 基线提交 | `22ea79e`（生成时的 HEAD）|

> 数字由 `pwsh -File scripts/progress.ps1 -Write` 生成（**不要手数**：手数三次错过
> 分母，126 应为 177）。改完代码就跑 `-Write` 并提交本文件 —— 门禁里有 `-Diff`，
> 漂移即失败。下面各节是明细。

## 待实现的方法体（`todo!()`）

口径：`todo!(` / `unimplemented!(` 出现次数（剥掉注释）。与「注册了多少」是两件事。

- 全仓合计：**136** 处
- 其中 `crates/sm-api/src/routes/*.rs`：**62** 处（= 已注册但**未实现**的端点 / 辅助函数）

| crate | `todo!()` |
|---|---|
| `sm-service` | 74 |
| `sm-api` | 62 |

### 按模块目录

| 位置 | `todo!()` |
|---|---|
| `sm-api/routes` | 62 |
| `sm-service/transfers` | 27 |
| `sm-service/catalog` | 24 |
| `sm-service/playback` | 16 |
| `sm-service/system` | 4 |
| `sm-service/discovery` | 3 |

### `sm-service` 按文件（降序）

| 文件 | `todo!()` |
|---|---|
| `catalog\catalog_import.rs` | 7 |
| `playback\media_library.rs` | 6 |
| `transfers\download_client.rs` | 6 |
| `catalog\metadata_source.rs` | 5 |
| `catalog\movie_image.rs` | 5 |
| `playback\media.rs` | 5 |
| `catalog\movie_metadata_search.rs` | 4 |
| `transfers\download_sync.rs` | 4 |
| `transfers\media_transfer_task.rs` | 4 |
| `catalog\movie_metadata_refresh.rs` | 3 |
| `transfers\import_service.rs` | 3 |
| `transfers\import_task.rs` | 3 |
| `discovery\moment_recommendation.rs` | 2 |
| `system\plugin_removal.rs` | 2 |
| `system\telemetry.rs` | 2 |
| `transfers\download_common.rs` | 2 |
| `discovery\image_search_space.rs` | 1 |
| `playback\media_file_hash_backfill.rs` | 1 |
| `playback\media_metadata_probe.rs` | 1 |
| `playback\media_thumbnail_pack_backfill.rs` | 1 |
| `playback\media_validity_scan.rs` | 1 |
| `playback\media_video_info_backfill.rs` | 1 |
| `transfers\auto_download.rs` | 1 |
| `transfers\download_request.rs` | 1 |
| `transfers\download_resource_hash.rs` | 1 |
| `transfers\download_task.rs` | 1 |
| `transfers\provider_browse.rs` | 1 |

## 端点

| 口径 | 上游 | Rust | 完成 |
|---|---|---|---|
| 方法级（path+method 组合） | 177 | 175 | 99% |
| 唯一路径级（参数归一后） | 136 | 136 | 100% |

未实现路径：**0** 条


未**注册**的方法级端点（`路径|方法`）：**2** 条

> 只报「未实现路径」会漏掉「路径在、少一个方法」这一档。

- `/actors/{}/profile-image|PUT`
- `/media/{}/clips|POST`

⚠️ 注册 ≠ 能用：其中 **62** 条的 handler 还是 `todo!()`。

### 已注册端点（按文件）

第三列 = 这个文件里还是 `todo!()` 的 handler 数；相减才是能用的端点数。

| 文件 | 端点（已注册） | 其中仍是 `todo!()` |
|---|---|---|
| `account.rs` | 3 | 0 |
| `activity.rs` | 6 | 0 |
| `actors.rs` | 12 | 1 |
| `auth.rs` | 3 | 1 |
| `clip_collections.rs` | 9 | 0 |
| `config.rs` | 2 | 0 |
| `download_clients.rs` | 5 | 3 |
| `download_tasks.rs` | 4 | 2 |
| `downloads.rs` | 1 | 0 |
| `files.rs` | 2 | 2 |
| `image_search.rs` | 7 | 7 |
| `indexer_settings.rs` | 3 | 0 |
| `jobs.rs` | 2 | 0 |
| `media_clips.rs` | 7 | 0 |
| `media_import.rs` | 5 | 3 |
| `media_libraries.rs` | 5 | 5 |
| `media_playback.rs` | 3 | 3 |
| `media_points.rs` | 3 | 3 |
| `media_transfer.rs` | 2 | 2 |
| `media.rs` | 11 | 5 |
| `moment_collections.rs` | 9 | 0 |
| `movie_subscriptions.rs` | 3 | 0 |
| `movies.rs` | 22 | 9 |
| `playlists.rs` | 9 | 0 |
| `plugins.rs` | 8 | 8 |
| `ranking_sources.rs` | 3 | 0 |
| `recommendations.rs` | 3 | 3 |
| `status.rs` | 6 | 2 |
| `tags.rs` | 3 | 0 |
| `video_collections.rs` | 9 | 0 |
| `videos.rs` | 5 | 3 |

### 上游端点（按子目录）

| 子目录 | 端点 |
|---|---|
| catalog | 41 |
| collections | 27 |
| discovery | 12 |
| files | 2 |
| playback | 30 |
| system | 34 |
| transfers | 17 |
| videos | 14 |

## service 层

| 域 | Rust 文件 | Rust 行 | 上游文件 | 上游行 |
|---|---|---|---|---|
| catalog | 26 | 10192 | 27 | 7556 |
| collections | 4 | 2024 | 5 | 1292 |
| discovery | 17 | 7113 | 16 | 4485 |
| playback | 20 | 6223 | 19 | 3741 |
| system | 19 | 5313 | 19 | 2935 |
| transfers | 17 | 6905 | 23 | 4248 |
| videos | 3 | 1735 | 4 | 927 |
| **合计** | **106** | **39505** | **113** | **25184** |

> ⚠️ **行数比不是完成度**（本仓注释占大头）；看上面的 `todo!()`。

## 调度

- 上游内建任务：21
- worker handler 已落地：**5**
  - `activity_record_cleanup`
  - `image_search_index`
  - `movie_similarity_recompute`
  - `movie_asset_pack_backfill`
  - `movie_heat_update`

> handler 少 = 任务能被 cron 入队、但没人执行。
