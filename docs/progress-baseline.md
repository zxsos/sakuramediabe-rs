# 重构进度基线

生成方式：`powershell -File scripts/progress.ps1 -Write`。**不要手数** —— 手数口径每次不同，
历史上分母错过三次（126 应为 177）。改动后跑 `-Diff` 确认基线是否需要更新。

- 提交：`5d9e397`

## 端点

| 口径 | 上游 | Rust | 完成 |
|---|---|---|---|
| 方法级（path+method 组合） | 177 | 76 | 43% |
| 唯一路径级（参数归一后） | 136 | 59 | 43% |

未实现路径：**77** 条

- `/actors/search/javdb/stream`
- `/auth/docs-token`
- `/daily-recommendations`
- `/download-clients`
- `/download-clients/{}`
- `/download-clients/test`
- `/download-requests`
- `/download-tasks`
- `/download-tasks/{}`
- `/download-tasks/{}/import`
- `/files/images/{}`
- `/files/subtitles/{}`
- `/hot-actress-releases`
- `/image-search/plot-sessions`
- `/image-search/plot-sessions/{}/results`
- `/image-search/plot-text-sessions`
- `/image-search/reset`
- `/image-search/sessions`
- `/image-search/sessions/{}/results`
- `/image-search/text-sessions`
- `/imports`
- `/imports/{}/failed-items`
- `/imports/{}/failed-items/{}/retry`
- `/imports/{}/failed-items/{}/search`
- `/import-sources/browse`
- `/media`
- `/media/{}`
- `/media/{}/play/{}`
- `/media/{}/points`
- `/media/{}/points/{}`
- `/media/{}/progress`
- `/media/{}/thumbnails`
- `/media/duplicates`
- `/media/invalid`
- `/media/merged-play/{}`
- `/media/multi-version-movies`
- `/media/playback-attempts/{}`
- `/media/thumbnail-generation/reset`
- `/media-libraries`
- `/media-libraries/{}`
- `/media-libraries/providers`
- `/media-points`
- `/media-points/{}`
- `/media-points/{}/collections`
- `/media-transfers`
- `/media-transfers/candidates`
- `/moment-collections`
- `/moment-collections/{}`
- `/moment-collections/{}/points`
- `/moment-collections/{}/points/{}`
- `/moment-recommendations`
- `/movies/{}`
- `/movies/{}/heat-recompute`
- `/movies/{}/merged-playback`
- `/movies/{}/metadata-refresh`
- `/movies/{}/reviews`
- `/movies/{}/similar`
- `/movies/{}/subscription`
- `/movies/{}/subtitles`
- `/movies/search/javdb/stream`
- `/movies/series/{}/javdb/import/stream`
- `/ranking-sources`
- `/ranking-sources/{}/boards`
- `/ranking-sources/{}/boards/{}/items`
- `/status/image-search`
- `/status/metadata-providers/{}/test`
- `/system/plugins`
- `/system/plugins/{}`
- `/system/plugins/{}/settings`
- `/system/plugins/{}/upgrade`
- `/video-collections`
- `/video-collections/{}`
- `/video-collections/{}/items`
- `/video-collections/{}/items/{}`
- `/video-collections/{}/items/reorder`
- `/videos`
- `/videos/{}`

### 已完成端点（按文件）

| 文件 | 端点 |
|---|---|
| `account.rs` | 3 |
| `activity.rs` | 6 |
| `actors.rs` | 11 |
| `auth.rs` | 2 |
| `clip_collections.rs` | 9 |
| `config.rs` | 2 |
| `downloads.rs` | 1 |
| `indexer_settings.rs` | 3 |
| `jobs.rs` | 2 |
| `media_clips.rs` | 7 |
| `movie_subscriptions.rs` | 3 |
| `movies.rs` | 11 |
| `playlists.rs` | 9 |
| `status.rs` | 4 |
| `tags.rs` | 3 |

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

| 域 | Rust 文件 | Rust 行 | 上游文件 |
|---|---|---|---|
| catalog | 7 | 3171 | 27 |
| collections | 4 | 1783 | 5 |
| discovery | 6 | 1661 | 16 |
| playback | 6 | 2292 | 14 |
| system | 17 | 4875 | 11 |
| transfers | 3 | 990 | 1 |
| videos | 3 | 1249 | 4 |

## 调度

- 上游内建任务：21
- worker handler 已落地：**1**
  - `activity_record_cleanup`

> handler 数远少于任务数是当前最大的空白：任务能被 cron 触发入队，
> 但没有 handler 去执行。
