# 逐域架构盘点

盘点时间：2026-10-05，提交 `5d9e397`。

数字全部来自 `scripts/progress.ps1`（见 `docs/progress-baseline.md`），本文只做**判定**。
判定分三类：

- **A 纯接线** —— 所需 service 已在 Rust 里，只差路由与 DTO
- **B 缺 service** —— 要新写领域逻辑
- **C 卡外部依赖** —— 缺的不是代码

判定的依据是「上游 router 的 `from src.service... import` 提取出的 `*Service`，
逐个在 Rust 的 `pub struct` 里找」。**不按文件体量排序** —— 体量排序曾经把
`sm-plugins` 误判成「1 行空壳」（实际 12 文件 / 127KB），方向完全错了。

## 一、端点缺口 77 条的分类

### A 类：service 已存在，只差路由（7 条）

| 路径 | service |
|---|---|
| `/videos`、`/videos/{}` | `VideoItemService` ✅ |
| `/video-collections` | `VideoCollectionService` ✅ |
| `/video-collections/{}` | `VideoCollectionService` ✅ |
| `/video-collections/{}/items` | `VideoCollectionService` ✅ |
| `/video-collections/{}/items/{}` | `VideoCollectionService` ✅ |
| `/video-collections/{}/items/reorder` | `VideoCollectionService` ✅ |

**这 7 条是零 service 成本** —— `sm-service/src/videos/` 的 `collection.rs` 与
`item.rs` 已经写好（`VideoCollectionService` / `VideoItemService`），缺的只是
`sm-api/src/routes/videos.rs` 与注册。这是当前性价比最高的一块。

另有 1 条虽然 service 存在，但**已判定不做**：

| 路径 | 不做的理由 |
|---|---|
| `/auth/docs-token` | 唯一消费方是 Swagger UI 的 OAuth2 表单，而本仓库无 `/docs` 路由、0 处 `utoipa`。实现它要加 axum 的 `form` feature + 写一个新提取器，换一个没人调用的 handler。见 `routes/auth.rs` 的模块文档。 |

★ `/actors/search/javdb/stream` **曾在此表**（当时的理由：SSE 需要真实 provider
插件）。2026-10-09 已落地 —— 演员搜索走的是 **JavDB**（`build_javdb_provider()
.search_actors`，不需要插件），缺的是它自己那一段抓取与流式编排
（`ActorJavdbStreamService`），不是插件侧。

### B 类：缺 service（66 条）

| 所需 service | 路径数 | 备注 |
|---|---|---|
| `MediaService` + `MediaThumbnailService` | 13 | `/media/*`，playback 域最大缺口 |
| `MovieMetadataRefreshService` / `MovieRecommendationService` / `MovieSubtitleService` / `MovieTaskService` | 10 | `/movies/{id}/*`；`MovieService` 已有，其余 4 个缺 |
| `DownloadClientService` / `DownloadRequestService` / `DownloadTaskService` | 7 | `DownloadSearchService` 已有 |
| `ImageSearchIndexService` / `get_movie_plot_image_search_service` | 6 | `/image-search/*`；推理 client 与 Qdrant 存储层已就绪 |
| `ImportTaskService` + `ProviderBrowseService` | 5 | `/imports/*`、`/import-sources/*` |
| `MomentCollectionService` | 7 | `/moment-collections/*` 与 `/media-points/*` 合并计 |
| `ImageSearchResetService` | 3 | 只有 980 行，**不卡 Qdrant** |
| `MediaLibraryService` | 3 | `/media-libraries/*` |
| `RankingCatalogService` | 3 | `/ranking-sources/*` |
| `MediaTransferTaskService` | 2 | `/media-transfers/*` |
| `DailyRecommendationService` / `HotActressReleaseService` / `MomentRecommendationService` | 3 | 三个推荐端点，各 1 条 |

### C 类：router 无 `*Service` import（2 条）

`/files/images/{}`、`/files/subtitles/{}` —— 逻辑在 router 内联或走文件系统，
不依赖 service 层。这两条**不受上面任何阻塞影响**。
## 二、catalog 域：27 个上游文件里做了 6 个

Rust 侧 7 个文件（含 `mod.rs`），覆盖上游 6 个 service，且**行数普遍超过上游** ——
说明拆得更细，不是没做。

| 上游文件 | 上游行 | Rust 对应 | Rust 行 | 判定 |
|---|---|---|---|---|
| `movie_service.py` | 1177 | `movie.rs` | 837 | 部分（`MovieService` 在，4 个子 service 缺） |
| `actor_service.py` | 827 | `actor.rs` | 1228 | 已超 |
| `actor_merge_service.py` | 207 | `actor_merge.rs` | 379 | 已超 |
| `movie_subscription_service.py` | 318 | `movie_subscription.rs` | 254 | 部分 |
| `movie_resolution_service.py` | 76 | `resolution.rs` | 266 | 已超 |
| `tag_service.py` | 104 | `tag.rs` | 178 | 已超 |
| `__init__.py` | 30 | `mod.rs` | 29 | 对应 |

未做的 20 个（约 4,900 行），按「是否解锁端点」重新排序：

| 上游文件 | 行 | 解锁的端点 | 判定 |
|---|---|---|---|
| `movie_metadata_refresh_service.py` | 535 | `/movies/{id}/metadata-refresh` | B |
| `movie_subtitle_service.py` | 183 | `/movies/{id}/subtitles` | B |
| `movie_task_service.py` | 48 | `/movies/{id}/subscription` | B |
| `movie_ownership_gateway.py` | 240 | 无直接端点 | B（catalog 内部依赖） |
| `actor_ownership_gateway.py` | 141 | 无直接端点 | B（catalog 内部依赖） |
| `metadata_source_service.py` | 242 | `/status/metadata-providers/{}/test` | **C 卡 metadata source** |
| `movie_javdb_backfill_service.py` | 109 | 无直接端点（喂 `movie_javdb_backfill` 任务） | **C 卡插件 ABI** |
| `movie_image_service.py` | 794 | `/media/{}/thumbnails` | **C 卡 zip（`movie_asset_pack_backfill`）** |
| `movie_asset_pack_service.py` | 169 | 同上 | C |
| `movie_asset_pack_backfill_service.py` | 181 | 同上 | C |
| `catalog_import_service.py` | 965 | `/imports/*` | B |
| `movie_service.py` 的其余部分 | — | `/movies/{}/heat-recompute` 等 | B |
| 其余 9 个（search/heat/interaction/cleanup/subscription_search_state/thin_cover/list_media 等） | ~1,100 | 零散 | B |

**结论**：catalog 域的 20 个文件里，**只有 3 个卡在外部依赖**（metadata source、
插件 ABI、zip），其余 17 个都是纯工作量。卡住的是少数派。

## 三、system 域：Rust 17 文件 vs 上游 11 —— 是拆分，不是职责漂移

上游 11 个文件是「一个 service 一个文件」；Rust 侧 17 个是因为把**同一职责的
常量与纯函数**单独成文件：

| Rust 文件 | 上游对应 | 性质 |
|---|---|---|
| `account.rs` / `activity_cleanup.rs` / `auth.rs` / `config.rs` / `indexer_settings.rs` / `jobs.rs` / `status.rs` / `task_queue.rs` | 同名 service | 一一对应 |
| `activity/`（7 文件） | `activity_service.py` | **一个 service 拆成子模块** |
| `optional_services.rs` | 无 | 能力开关与「为什么功能不可用」 |
| `mod.rs` | — | 模块声明 |

**判定：合理。** `activity` 上游是 1 个 15KB 文件，Rust 拆成 7 个是因为它承担了
通知、任务台账、任务执行三块职责。`optional_services.rs` 没有上游对应物，但它是
`image_search_requires_qdrant` 那类跨节校验的落点，**没有它功能开关会散落在各处**。

这一域**没有架构问题**，之前我担心的「职责漂移」不成立。
## 四、21 个内建任务：1 个有 handler，20 个缺

**关键结论：20 个缺的 handler 里，19 个的 service 在 Rust 里不存在。**
所以「补 handler」这档工作量基本等于「写 19 个 service」—— 按任务数排优先级
会误导，那其实是按 service 数排。

| 任务 | 缺什么 | 判定 |
|---|---|---|
| `activity_record_cleanup` | — | ✅ 已落地 |
| `movie_javdb_backfill` | `MovieJavdbBackfillService` | **C 卡插件 ABI** |
| `media_thumbnail_generation` | `MediaThumbnailService` | **C 卡 `media_thumbnail_pack_backfill`（zip）** |
| `MediaThumbnailPackBackfillService::TASK_KEY` | 同上 | C |
| `MovieAssetPackBackfillService::TASK_KEY` | `MovieAssetPackBackfillService` | C |
| `MediaFileHashBackfillService::TASK_KEY` | `MediaFileHashBackfillService` | B |
| `MediaVideoInfoBackfillService::TASK_KEY` | `MediaVideoInfoBackfillService` | B |
| `MediaValidityScanService::TASK_KEY` | `MediaValidityScanService` | B |
| `image_search_index` | `ImageSearchIndexService` | B（推理 client 与 Qdrant 已就绪） |
| `movie_similarity_recompute` | `MovieRecommendationService` | B |
| `moment_recommendation_generate` | `MomentRecommendationService` | B |
| `daily_recommendation_generate` | `DailyRecommendationService` | B |
| `movie_heat_update` | 模块级 `_run_movie_heat` | B |
| `gfriends_filetree_refresh` | `refresh_gfriends_filetree` | B |
| `library_import` | `ProviderBrowseService` / `ImportTaskService` | B |
| `media_storage_transfer` | `MediaTransferTaskService` | B |
| `actor_subscription_sync` | `SubscribedActorMovieSyncService` | B |
| `subscribed_movie_auto_download` | `SubscribedMovieAutoDownloadService` | B |
| `movie_interaction_sync` | `MovieInteractionSyncService` | B |
| `download_task_sync` | `DownloadSyncService` | B |
| `download_task_auto_import` | `DownloadSyncService` | B（与上一条共用） |

去重后 **19 个 service 缺失**，其中 3 个卡外部依赖。

## 五、建议的推进顺序

按「解锁端点数 / 需要新写的 service 数」排，不按文件体量：

| 优先级 | 做什么 | 解锁 | 新写 service |
|---|---|---|---|
| 1 | `/videos/*` + `/video-collections/*` 接线 | **7 路径 / 14 端点** | **0** |
| 2 | `/files/images/{id}`、`/files/subtitles/{id}` | 2 路径 | 0（router 内联） |
| 3 | `ImageSearchResetService`（980 行）+ `/status/image-search` | 3 路径 | 1（很小） |
| 4 | `/image-search/*` 全套 | 6 路径 | 2（推理与 Qdrant 已就绪） |
| 5 | `ranking_service` + `hot_actress_releases` | 4 路径 | 2（纯 PostgreSQL） |

**第 1 项应该先做** —— 零 service 成本、解锁 14 个端点，是全表里唯一的
「纯接线且收益最大」项。它也顺带验证 `videos` 域的 service 是否真的够用
（`VideoCollectionService` / `VideoItemService` 写完后从未被路由调用过，
**没有测试证明它们能对外服务**）。

## 六、盘点过程中发现的方法论问题

写这份盘点时我三次提取出错，全部同一类：**正则/切分没对着真实结构写**。

1. 上游 import 是 `from x import (\n A,\n B,\n)` 多行括号形式，正则贪婪匹配跨过
   换行，把下一条 import 也吞进来 —— 导致 `VideoCollectionService` 明明存在却
   被判为「缺」。
2. 按 900 字符窗口截取 JobDefinition 块，窗口串到下一个条目 ——
   `movie_javdb_backfill` 显示出了属于 `actor_subscription_sync` 的 service。
3. 数上游任务时只匹配 `task_key="字面量"`，漏掉 5 个
   `task_key=SomeService.TASK_KEY` 形式。

**这与端点分母错三次（126 vs 177）是同一个错误模式。** 已在
`scripts/progress.ps1` 的注释里逐条记录。

## 七、一个仍然没有答案的问题

`sm-api` 有 2 处绕过 `sm-service` 直连 `sm-db` 仓储：

- `routes/auth.rs:26` 用 `UserRepository`（token 校验要读用户表，
  **service 层没有对应方法** —— 这是真实的分层缺口）
- `routes/jobs.rs:237` 用 `BackgroundTaskRunRepository`，而**同一文件 `:85` 的注释
  写着「走服务层而不是直接调仓储」**

第 2 处该改（加一个 service 方法）。第 1 处要先决定「用户查询」归 `sm-service::auth`
还是独立成 `sm-service::system::users` —— 这是个归属判断，需要拍板。