# service 层推进台账

上游 `src/service/` 共 **114 个文件 / 25,174 行**，分七个子域。这里逐域记录
**已落地的规则**、**刻意不复刻的部分**与**待核对项**，取代早先 README 里
那个看不出「哪个域卡在哪」的百分比。

测试口径：每个测试对应上游一条规则，**同时断言状态码与错误码**
（`crates/sm-service/tests/`，跑在真实 PostgreSQL 上）。

## 汇总

> 数字全部来自 [`progress-baseline.md`](progress-baseline.md)（脚本生成，
> **不要手数**）；下面的逐域叙述讲「为什么这么做」，数字可能滞后，**以基线为准**。

| 域 | 上游文件 | 上游行 | 本仓 `todo!()` | 状态 |
|---|---|---|---|---|
| `collections` | 5 | 1,292 | 0 | **完成** |
| `videos` | 4 | 927 | 0 | **完成** |
| `system` | 19 | 2,935 | 4 | 12 个 service 已落；剩 `telemetry`(2)、`plugin_removal`(2) |
| `playback` | 19 | 3,741 | 16 | **能做的已做完**，剩 5 个文件卡 provider / PyAV / zip |
| `catalog` | 27 | 7,556 | 33 | 演员子域 + 影片订阅 / 列表 / 黑名单 / 合集态 + 两个 ownership gateway（在 `sm-db`）已落；剩 9 个文件 |
| `transfers` | 23 | 4,248 | 27 | 契约层 + 台账 + 入队 + 失败项读取 + 导入提醒已落；剩 10 个文件**几乎全卡插件 ABI** |
| `discovery` | 16 | 4,485 | 3 | ⚠️ 下面「框架 12/16 铺完、方法体待实现」的说法**已过时** —— 后续多轮落了方法体，现在只剩 3 处 |
| **合计** | **113** | **25,184** | **83** | 另：`sm-api` 路由层 63 处 `todo!()` |

**端点：两个数一起报才不误导** —— 方法级 175/177 已**注册**（路径级 136/136），
但其中 **63 条的 handler 仍是 `todo!()`**。所以「99% 完成」是假的：
注册只说明路由表里有这一条。

行数口径与 `crates/sm-service/src/lib.rs` 的表格一致。推进次序沿用那里的
约定：**最小且完整的域先定型**，之后照此推进。

---

## 阻塞地图 —— 决定「什么现在能做」

外部依赖分三类。**避开这三类才能推进**，所以这张表比「按行数排序」更能
决定下一步做什么。

| 依赖 | 被阻塞 | 备注 |
|---|---|---|
| `provider_protocol` / `MEDIA_PROVIDER_REGISTRY` | **24 个文件 / 8,885 行** | ⚠️ **本行原写的理由已失效** —— 原文说「需先做 `sm-plugins` 宿主，而它目前是 1 行空壳 —— 这是最大的单点阻塞」。实测 `sm-plugins` 是 **12 个文件 / 127,655 字节**（`supervisor.rs` 16.5KB、`movie_delivery.rs` 18.4KB、`extensions.rs` 25KB、`jobs.rs` 13.4KB、`loader.rs`、`runner.rs`、`registration.rs`、`registry.rs`、`scheduling.rs`、`extension_calls.rs`、`error.rs`），组合根也已接上（`sm-server/src/plugins.rs`）。**真正剩下的阻塞是「还没有任何真实 provider 插件被移植过来」** —— 宿主有能力了，缺的是被宿主承载的东西。**7 个插件的体量、难度与移植次序见 [`deployment.md`](deployment.md) §5.2，执行顺序见 [`handoff.md`](handoff.md) §八。** |
| PyAV（`import av`） | 含 `playback/media_metadata_probe_service`(338) | 属 `svc-probe`（阶段 9） |
| zip 实现 | `playback/media_thumbnail_pack_backfill_service`(216) | 仓库无 zip crate；另缺 `write_pack` / `media_paths` 助手 |
| Qdrant | ~~`discovery/` 下 10 个文件~~ | ✅ **已不是阻塞**（2026-10-05）。`qdrant-client 1.19` 已接入（`sm-service/src/discovery/qdrant/`），服务端用 Podman 跑在 `127.0.0.1:6333/6334`。剩下的是 `movie_similarity` 那套**稀疏向量 + 别名切换**，结构与稠密那套不同，是独立的活 |

`playback` 域的 19 个文件里，**能做的都已做完**，剩下 4 类分别卡在上述三个
依赖上（`thumbnails/task_service` 508 行卡 provider、`thumbnails/artifacts` 204
行卡 provider 的一个类型、`media_metadata_probe` 338 行卡 PyAV、
`thumbnail_pack_backfill` 216 行卡 zip）。`thumbnails/progress`(56) 用
`threading.Thread` + 直连 DB，翻译成 tokio 是**设计变更**而非对译；
`thumbnails/contracts`(22) 是个异常类，要等 `task_service` 落地才有意义。

> ⚠️ **判断「某个上游文件是否被阻塞」必须读它的 import 段，不能用关键词 grep。**
> 我用 `grep MEDIA_PROVIDER_REGISTRY` 判定过一次，四个文件判错，其中两个是
> 700+ 行的大文件 —— 它们用的是多行
> `from src.plugins.provider_protocol import (...)`，grep 漏了；而
> `media_metadata_probe_service` 名字像纯逻辑，实际 `import av`。
> 错误的答案比没有答案更贵：它会把人送进编不过的工作里。

---

## `collections` —— 完成

| Rust 模块 | 上游文件 | 面向 |
|---|---|---|
| `collections::playlist` | `playlist_service.py` | 终端用户 |
| `collections::ordered` | `moment_collection_service.py` + `clip_collection_service.py` | 终端用户（宏生成两份） |
| `collections::plugin` | `plugin_collection_service.py` | 插件 facade |

**已落规则**：名称归一 / 唯一性 / 系统保留名 / 系统列表不可改 / 空更新 422 /
幂等加入 / 移除不重排位置 / 成员顺序语义。

**已落查询编排**（对应上游 `playlist_service.py` 的三个读方法）：

| 方法 | 上游 | 端点 |
|---|---|---|
| `list(include_system)` | `list_playlists` | `GET /playlists` |
| `member_count(id)` | `_playlist_counts` | 供 `GET /{id}` / `PATCH /{id}` 填 `movie_count` |
| `resolution_options(id)` | `list_playlist_resolutions` | `GET /{id}/resolutions` |
| —— | `list_playlist_movies` | **待做**，见下 |

三处容易改错的地方，各有集成测试钉住：

1. **系统列表固定排最前**（`CASE kind WHEN 'recently_played' THEN 0 ELSE 1 END`），
   且排序键是 `updated_at DESC, id DESC` 两级 —— `add_movie` 会连带 touch 父
   列表，同毫秒内并列时只按 `updated_at` 排会让列表页抖动。
2. **一部影片只计入它最高的那一档**。`MAX(level)` 之后按序号分桶，所以一部
   4K + 1080P 的影片算 1 部 4K，筛选项计数之和不会超过列表里的影片数。
3. **脏 `resolution` 值被排除而不是让查询报错**。`^\d+x\d+$` 之外的
   （`1920*1080` / `HD`）一律不算；没有这条正则，一个脏值就会让
   `split_part(...)::int` 抛 `invalid input syntax` 而让整个端点 500。

档位序号 → 标签的映射与半开区间在 `sm_service::catalog::resolution`，
对应上游 `catalog/movie_resolution_service.py`。上游把它放在 `catalog/` 下
而 `collections` 导入它，这里跟上游一致。

**刻意不复刻**：`ProgrammerError`（上游抛 `ValueError` 的两处）映射成
**500** 而不是 422 —— 那是「我们自己的代码传错参数」，不是用户输入无效。

## `videos` —— 完成

| Rust 模块 | 上游文件 | 行数 |
|---|---|---|
| `videos::item` | `video_item_service.py` | 436 |
| `videos::collection` | `video_collection_service.py` | 389 |
| （不落地） | `video_cover_service.py` | 95 |

**已落规则**（每条都有集成测试）：

| 规则 | 错误契约 |
|---|---|
| 条目 / 合集不存在 | 404 `video_item_not_found` / `video_collection_not_found`（details 键是 `collection_id`） |
| 名称重复 | 409 `video_collection_name_conflict` + `{"name": …}` |
| 空白标题 / 合集名 | 422 `validation_error` |
| 空更新 | 422 `validation_error` |
| 封面缩略图显式 null | 422 `video_cover_thumbnail_required` |
| 封面缩略图不存在**或**属于别的条目 | 404 `video_cover_thumbnail_not_found` |
| 重排未恰好覆盖全体成员 | 422 `invalid_collection_reorder` |
| 分页 / 搜索词 / 排序 | 422 `invalid_video_filter`（三类共用一个码，与上游一致） |

**三条容易改错的**（都有测试钉住）：

1. `add_item` 重复加入是**幂等成功**（409 是错的），且**不**把已存在的成员
   挪到末尾。
2. `remove_item` 收的是**关联行 id**，`remove_items_by_video_ids` 收的是
   **视频 id**。两个 id 空间相同，传错不报错、只删错行。
3. 更新时 `title` / `summary` 的显式 null 被**忽略**，`release_date` 的显式
   null 是**清空**，而 `{"title": null}` **不算空更新**（只推进 `updated_at`）。

**刻意不复刻**（三处，理由见 `sm_service::videos` 的模块文档）：

- 列表 / 详情 / 合集成员分页的查询编排（每条目 `MIN(Media.id)` 子查询 +
  三次 `LEFT JOIN` + `COALESCE`）→ 归仓储；
- 播放地址与 `can_play` → 需要插件 registry（gRPC 未落地）；
- 首帧封面生成（PyAV 解码 + 有损 WebP）→ 属 `svc-image` / `svc-probe`。

## `system` —— 进行中

| Rust 模块 | 上游文件 | 状态 |
|---|---|---|
| `system::auth` | `system/auth_service.py` | **完成** |
| `system::account` | `system/account_service.py` | **完成** |
| `system::activity_cleanup` | `system/activity_cleanup.py` | **完成** |
| `system::config` | `system/config_service.py` | **完成** |
| `system::task_queue` | `system/task_queue_service.py` | **完成**（`settle_bootstrap_blocker` 除外，见下） |
| `system::optional_services` | `system/optional_services.py` | **完成** |
| `system::status` | `system/status_service.py` | **完成**（3/5 个方法，见下） |
| `system::indexer_settings` | `system/indexer_settings_service.py` | **完成** |
| 其余 11 个文件 | `system/*.py` | 待做 |

### `optional_services` —— 完成

两个能力开关（相似影片 / 图搜）+ 两个被它们门控的任务。**13 个单元测试**，
不需要数据库 —— 它只读配置。

| 上游 | 本模块 |
|---|---|
| `movie_similarity_enabled()` | `movie_similarity_enabled(values)` |
| `image_search_enabled()` | `image_search_enabled(values)` |
| `capabilities()` | `capabilities(values)` / `capabilities_of(config)` |
| `require_image_search()` | `require_image_search(values)` |
| `job_disabled_reason(task_key)` | `job_disabled_reason(task_key, values)` |
| `require_job_enabled(task_key)` | `require_job_enabled(task_key, values)` |

**四个刻意的设计点**：

1. **只读配置，不做网络探测**（与上游一致）。即便 Qdrant 配好了但连不上，
   也回答「已启用」—— 否则同一个接口在网络抖动时给出不同答案，而客户端
   无从判断是配置变了还是服务挂了。
2. **函数收 `&Value` 而不是自己读盘。** 上游的 `settings` 是进程级单例，
   这里把它变成显式参数：三个能力判定共用**同一个快照**，而 `PATCH /config`
   可以在中间生效。逐个读会读到三个不同时刻的配置。
3. **缺节 / 类型不对一律读作「未启用」。** 让功能**意外启用**的后果远大于
   意外停用（会走到不存在的服务）。
4. **`FeatureDisabled` 单列，映射成 409。** 409 在这里的语义是「换个配置就能用」，
   与名称冲突（同样是 409）的 `code` 不同，客户端要能分辨。

**立刻解锁一个端点**：`GET /status/capabilities`。它是 status router 里唯一
**只读配置、不查库**的端点 —— 而能力开关正是客户端决定「要不要显示图搜入口」
的前提。等 `StatusService`（602 行）一起落的话，客户端会在图搜已配置好的部署上
看不到入口，而服务端其实是有能力的。

### `status` —— 完成 3/5 个方法

| 上游方法 | 端点 | 状态 |
|---|---|---|
| `get_status` | `GET /status` | **完成** |
| `get_insights` | `GET /status/insights` | **完成**（磁盘空间三列 `null`） |
| `get_watch_trend` | `GET /status/watch-trend` | **完成** |
| `get_image_search_status` | `GET /status/image-search` | 阻塞：`discovery` 域的 embedding / Qdrant 客户端 |
| `test_metadata_provider` | `POST /status/metadata-provider/test` | 阻塞：`metadata` 域的 JavDB provider |

被阻塞的两者主体都是**对外部服务发请求并解读响应**。写一个只会返回
`unhealthy` 的假实现比不写更糟 —— 客户端会把它当成「服务真的挂了」。

**一处刻意的 `null`**：`get_insights` 的磁盘空间三列返回 `null` 而非 `0`。
上游调 `MediaLibraryService.storage_space_usages()`（`playback` 域的真实磁盘
探测），那个 service 还没写。填 `0` 会被客户端渲染成「磁盘满了」，而事实是
「没探测」。字段必须存在（客户端读它），值待补。

**新增 `sm-db::repo::StatsRepository`**：状态页的跨表聚合。放在一起是因为
它们跨表，而 `repo/` 其它文件严格一文件一表。单表计数仍在各自仓储里。

**本轮修掉的两个真实缺陷**（都是「只有跑真库/真数据才暴露」）：

1. **`import_status::DONE` 的字面量是 `"done"`，而上游是 `"completed"`。**
   后果严重：上游 `StatusService._download_task_bucket` 按 `== "completed"`
   判 `imported` 桶，`"done"` 既不等于它、也不在 `UNFINISHED` 里，落到
   `else` 分支 —— **每一个导入成功的下载都会被 `/status/insights` 报成
   「导入失败」**。而且没有任何报错。
   同一处还**缺 `SKIPPED`**（上游五个字面量只有四个），导致「这一趟没有可导入
   的媒体文件」这个**正常结果**写不进去、只能记成 `failed`。
   DDL 里该列是**无 CHECK 约束**的 `varchar(32)`，所以数据库不拦，全靠代码。
   与 `task_state` 的 `succeeded` → `completed` 是同一类缺陷，同一仓库犯过两次
   —— 现在两个模块的字面量都逐条钉进了测试。
2. **`SELECT COUNT(DISTINCT movie)`** —— `media` 表的列是 `movie_number`。
   这是本仓库**第三次**照抄 Peewee 属性名而非 DDL 列名（前两次：
   `playlist_movie.movie` / `import_status` 的字面量）。三次都是
   `cargo check` + `clippy` + `cargo test --lib` 全绿、只在集成测试里报
   `column ... does not exist`。规则已写进 `stats.rs` 的模块文档。
3. **`SUM(bigint)` 在 PostgreSQL 返回 `NUMERIC`**，不是 `bigint`。不写
   `::bigint` 时 sqlx 解码报 500，而 **schema 对拍查不出来**（它只看表的列，
   不看聚合表达式的返回类型）。

### `task_queue` —— 完成

队列的四个原语全部落地，**阶段 7 的 worker 只差 handler 分发**：

| 方法 | 上游 |
|---|---|
| `mutex_key` | `build_mutex_key` |
| `enqueue(conflict)` | `enqueue(conflict="skip"/"raise")` |
| `claim_next(lease, lanes)` | `claim_next` |
| `renew_leases(ids, lease)` | `renew_leases` |
| `recover_expired_leases` | `recover_expired_leases` |
| `recover_interrupted_runs` | `recover_interrupted_runs` |
| —— | `settle_bootstrap_blocker` **未落**（见下） |

**三处与上游的刻意差异**：

1. **租约过期的默认处置保留两种**：本模块的 `recover_expired_leases` 判**失败**
   （对齐上游，写 `queue_lease_expired` 失败码）；仓储的 `reclaim_stale` 回
   **pending**。这是两种业务选择 —— 重入型任务该回 pending，不可重入型任务
   （上传、删除、扣配额）该判失败。合成一个会让其中一种永远错。
2. **冲突策略是枚举而非异常**。上游抛 `TaskQueueConflictError`，而那个异常
   在 API 层没有对应响应，只被 worker 内部调用。`EnqueueOutcome` 让「被挡住」
   成为一个正常返回的结果。`Skip` 与 `Raise` 的差别在**是否查阻塞方 id**。
3. **`settle_bootstrap_blocker` 未落**：它收敛的是两个内建引导任务的冲突行，
   而那两个任务的 service 还没写，硬写只会得到没有调用方的函数。

**两处修正了已有代码的错误**：

- **`sm_scheduler::tick` 与本模块的 `enqueue` 原来都写 `scheduled_at = NULL`**，
  而上游 `task_runs.py:161` 写的是 `scheduled_at=now()`。这不是装饰 ——
  `recover_interrupted_runs` 靠 `scheduled_at IS NOT NULL` 判定「上个进程遗留
  的任务」，写 NULL 会让所有队列行**永远不被中断回收**，崩溃后只能等租约到期
  （最多 300 秒）才动。
- **`sm-scheduler/tests/scheduler_tick.rs` 有一处断言与上游相反**：它声称
  「与上游逐字段一致」，实际断言 `scheduled_at IS NULL`。已按上游改正。

**互斥键前缀收成了唯一真相源**：`QUEUE_MUTEX_PREFIX` 现在住在
`sm_db::system::activity`（唯一索引的语义解释处），`sm-scheduler` 与
`sm-service` 都引用它。此前 `sm_scheduler::tick` 自己定义了一份，而新的
service 又要一份 —— 三处互不依赖的 crate 共用一个字符串常量，加前缀时
漏改一处就会让在跑的任务与新调度的任务**互相不认**，且不报错。

**已落规则**：登录 / 刷新 / 令牌轮换状态机 / 错误码（`invalid_credentials`、
`invalid_refresh_token`、`unauthorized`）。令牌生成与哈希在
`sm_core::refresh_token`，轮换在 `sm_db::repo::user::rotate`（三步同事务）。
密码算法是 Argon2id，**存量 bcrypt 哈希可校验并在登录时自动升级**（见下）。

**两处刻意差异**（`crates/sm-service/src/system/auth.rs` 有完整记录）：

- 密码算法是 **Argon2id**，上游是 bcrypt。**存量 bcrypt 哈希可校验**，
  登录成功后自动升级为 Argon2id（`sm_core::password` + `bcrypt` 校验器，
  纯 Rust 实现，不影响交叉编译）。bcrypt 只用于**验证**旧哈希，写侧永远是
  Argon2id。
- 刷新时取用户用 `find_primary`（照搬上游的 `User.select().order_by(id).first()`），
  因为 `user_refresh_tokens` **没有 `user_id` 列** —— 改不了的是 schema，不是这里。

## `plugin_removal` —— 被插件 ABI 阻塞

上游 95 行。数据库那一半（给定 `provider_keys` → 查 `media_library` /
`media` / `download_client` 的占用计数 → 抛带 details 的 `PluginInUseError`）
是可以实现的，且仓储层齐备。

但 `_provider_keys` 依赖 **Python 插件加载器**：`check_plugin_dir()` 加载插件
目录、读它的 extensions、过滤 `MEDIA_PROVIDER_EXTENSION_KEY`。Rust 侧
`sm-plugins` 仍是一行注释（`//! 插件宿主：进程管理、gRPC 客户端、能力注册表`），
provider registry 不存在。

只落数据库那一半会得到一个**没有调用方的**服务 —— `remove()` 的第一步就是
`_provider_keys`。所以整体记为阻塞，等阶段 8。

`PluginInUseError` 的 details 形状值得先记下来，将来照抄：
`{plugin_id, provider_keys, library_ids, media_count, download_client_count}`，
message 是「插件仍被 N 个媒体库引用（M 个媒体、K 个下载客户端），无法删除；
请先迁移或删除相关媒体库。」

### `indexer_settings` —— 完成

| 上游方法 | 端点 | 状态 |
|---|---|---|
| `get_settings` | `GET /indexer-settings` | **完成** |
| `update_settings` | `PATCH /indexer-settings` | **完成** |
| `test_connection` | `GET /indexer-settings/test` | **完成**（用 `transfers::torznab`） |

`test_connection` 用固定番号 `SSNI-888` 对每个 indexer 发一次**真实搜索**
（不是 ping），以此验证地址与 apikey 整体可用。它的解锁点是 `transfers` 域的
Torznab 客户端 —— 本批落了 `sm_service::transfers::torznab`（跨 indexer 搜一次
并数结果），完整候选映射随搜索端点再做。

**探测失败也是 200**：返回的是一份报告（`healthy` + `error.type`），不是请求
失败。三个分支：没配 indexer → `no_indexers_configured`；请求失败（HTTP 非
2xx / XML 被截断）→ `torznab_request_error`；成功 → `error` 为 `null`。

两处容易出错的：**`apikey` 只在索引器自己有 key 时才带**（空 key = 不带参数，
兼容免鉴权端点）；**失败消息里不能出现 URL 与凭据** —— 上游专门写了
`_describe_search_error` 剥掉 httpx 内嵌的完整 URL，本批照做，并有集成测试
钉住「HTTP 500」这种只有状态码的形态。

**一处刻意的加严**：`xmltodict` 对截断的 XML 会抛错，而 `quick-xml` 默认
**不校验**「EOF 时仍未闭合」。不补这道深度计数的话，一个被截断的响应会表现成
「健康的 0 条结果」—— 而 `test_connection` 正是靠 `result_count` 判断 indexer
是否可用。所以解析器自己数深度，未闭合即报错。

**整表替换而非增量 diff**：`indexers` 是完整列表，保存后库里就正好是这些。
全删全插**在同一个事务里**（`IndexerRepository::replace_all`）。不包事务的话
中途失败会留下空表或半张表，而用户只是点了一次保存。

**本轮抓到一个真缺陷（由集成测试发现）**：`api_key` 的**三态**在
`Option<Option<String>>` + 普通 serde 下**做不到**区分：

| 请求 | 需要 | 实际 |
|---|---|---|
| 不带 `api_key` 键 | 沿用旧值 | `None` ✅ |
| `"api_key": null` | **清空** | `None` ❌ 也变成了「沿用」 |
| `"api_key": "k"` | 设为 `k` | `Some(Some("k"))` ✅ |

serde 对「键不存在」用 `default` 给 `None`，对「键存在但为 null」也让外层
`Option` 解成 `None` —— 两者变成同一个值。后果是**「清空」变成了「保留」，
且没有任何报错**。修法是一个自定义反序列化器（`de_tri_state_string`），
判据是「反序列化器有没有被调用」：`#[serde(default)]` 生效时代它根本不
被调用（→ 键不存在），被调用且收到 `null` 才是显式清空。

**顺带修正了我自己的一个错误测试预期**：重复的 `download_client_ids` 用
不存在的 id 去测，实际拿到 404 —— 因为上游在**同一个循环**里既查重复又查
存在性，不存在的 id 先撞 404。要测「重复」必须用**真实存在**的 id。

## `telemetry` —— 待做，且有一个需要拍板的问题

| 项 | 状态 |
|---|---|
| ADR 第 43 行已批准遥测（`sysinfo` 0.39.6 替代 psutil） | ✅ 有据 |
| 上游触发点 | `aps.py:365`（定时任务，**不是** HTTP 端点） |
| 本批解锁的端点 | **0 个** |

**需要拍板的一点**：上游把心跳 POST 到一个**硬编码的第三方地址**
（`https://pswhnebzlzdcdljzvrqa.supabase.co/functions/v1/telemetry/v1/heartbeats`），
且是 **opt-out**（`SAKURAMEDIA_TELEMETRY_ENABLED != "false"` 即默认开启）。
ADR 批的是「用哪个库替代 psutil」，**没有**覆盖「是否继续往那个地址发」。

本批**没有**实现它，所以没有任何数据被发出。若要实现，可先落地全部**本地**
部分（`is_enabled` / `_build_payload` / CPU 型号 / 内存 / `instance_id`
持久化 —— 188 行里约 60% 是这些，都可离线测试），只把 `report()` 留作联网部分。
但 `plugins` 字段还依赖 `PluginManager().list_plugins()`，那属插件 ABI。

## `playback` —— 进行中（3/19）

### `media_summary` —— 完成

对应上游 `playback/media_summary_service.py`(40) + 它的调用方
`catalog/movie_list_media_service.py`(14)。**10 个集成测试**。

| 上游 | 本仓库 |
|---|---|
| `list_movie_media_summaries` | `playback::list_movie_media_summaries` |
| `attach_movie_list_media` | `playback::attach_movie_list_media` |

**它是 N+1 的解药**：一条带 `IN` 的查询（外加 `LEFT JOIN` 取库名），
再按番号在内存分组，所以「这部影片有几个媒体」「能不能播」**不会**变成
每部影片一次查询。

**`can_play` 是 any 而不是 all** —— 一条有效 + 五条判死的影片是**能播**的。
这条语义同时被 `list_playlist_movies` 与影片卡片端点依赖。

**`video_info` 刻意不解析**：它是 `JsonTextField`（TEXT 里的 JSON），
可能是脏文本。解析失败时上游会 500，而摘要的用途是渲染列表 ——
一个坏 `video_info` 不该让整个列表挂掉。调用方要读结构时自己 parse。

**一个被集成测试纠正的文档错误**（值得留着当记录）：我给 `LEFT JOIN` 写的
理由是「孤儿媒体（库被删了）历史上可能存在，用内连接会让它从摘要里消失」。
集成测试 `deleting_a_library_cascades_to_its_media` 证明这是**错的** ——
`media_library_id_fk` 是 `ON DELETE CASCADE`（`docker/schema.sql:512`），
删库会连带删掉媒体，**孤儿媒体在当前 DDL 下不可能存在**。

左连接仍然保留（跟着上游，且那三个 `library_*` 字段在 DTO 里声明为可空），
但理由已改成真话。教训是：**「为什么用 LEFT JOIN」这类推理必须能被一个
测试钉住**，否则它会变成看起来很合理的错误注释。

### `operation_locks` —— 完成

对应上游 `playback/operation_locks.py`（52 行）。**9 个集成测试** +
3 个单元测试，全部需要真实 PostgreSQL。

**机制在 `sm_db::common::advisory_lock`**（新模块），service 侧只做
409 映射与命名空间常量。

**核心是会话级 advisory lock + 连接池的组合**，它有一个真实的泄漏路径：

```text
1. 从池里借连接 C，在 C 上取锁
2. 忘记解锁（或解锁失败）
3. C 归还到池
4. 池把 C 发给另一个请求 —— 锁还在 C 的会话上
5. 那个请求对同一个资源 pg_try_advisory_lock **永远失败**
```

症状是「偶发 409，重启就好」，极难定位。机制层用两条规则消掉它：

1. **守卫持有那条连接** —— 解锁只可能在同一条连接上生效，而类型保证了
   「释放的连接」与「取锁的连接」是同一条。
2. **`Drop` 里 `detach()` 而不是归还** —— 正常路径 `release()` 显式解锁、
   连接干净归还；异常路径（`?` 提前返回 / panic）把连接**摘出池**，
   它随即被关闭，**PostgreSQL 因会话结束自动释放该会话上的全部锁**。
   代价是少一条可复用连接，但那**只发生在异常路径**，而「永久繁忙」
   严重得多。

`dropping_the_guard_does_not_leak_the_lock_into_the_pool` 与
`repeated_acquire_and_drop_never_accumulates_locks`（20 轮）钉住这条。

**两个命名空间**：`MEDIA = 17001` / `LIBRARY = 17002`，逐字沿用上游。
分开是必需的 —— 否则 `media.id == 1` 与 `media_library.id == 1` 会互相挡住。

**本轮修掉的两个真 bug**（都是测试发现的）：

1. **`2_i32.pow(31)` 溢出**。上游的校验是
   `if not 0 < resource_id < 2**31`，照抄成 Rust 就是
   `resource_id >= 2_i32.pow(31)` —— 而 `2^31 > i32::MAX`，**debug 构建下
   panic**，release 下回绕成 `i32::MIN` 让判断恒为假。一个恒假的检查比
   没有检查更糟，因为它看起来在校验。正确做法是**只判下界**：上界由
   `i32` 类型本身保证。
2. **测试夹具无法验跨会话语义**。`TestDb` 的池是 `max_connections(1)`
   （`search_path` 是会话级设置），用它验「第二个持有者被拒」会拿到
   **误导性的通过** —— 同一会话重复 `pg_try_advisory_lock` 总是成功。
   已给 `TestDb` 加 `pool_with_max_connections(n)`（`after_connect` 钩子
   给每条新连接设 `search_path`）。**同一个缺口还挡住 `FOR UPDATE SKIP
   LOCKED` 与会话级 GUC 的测试**，所以它是夹具层面的修，不是这一个模块的。

**一个更隐蔽的偶发失败：advisory lock 的命名空间是「整个数据库」**

`pg_locks` 里**没有 schema 概念** —— `pg_try_advisory_lock(17001, 1)` 与测试
schema 无关。所以两个**并发跑的测试二进制**（`cargo test --workspace` 会
并行跑多个 integration suite）只要都用 id = 1，就会互相挡住。

症状是**偶发**的 `.expect("lock")` 挂在「这个 id 明明只有我在用」的那一个
上，而且**重跑就绿** —— 正是最难定位的那类缺陷。它在门禁里表现为
`cargo test` 时而红时而绿。

修法是让 `resource_id` **跨测试二进制唯一**（20 位微秒计数 + 11 位进程内
序号 = 31 位，仍在 `0 < id < 2^31` 内）。改动很小，但**必须知道命名空间是
全局的**才会去找 —— 否则只会看到「重跑就好了」。

**一条运行约束**（写进模块文档）：守卫持有期间占着一条连接，所以

```text
需要连接数 = 同时持有的锁数 + 1
```

`max_connections = 1` 的部署会在第一次取锁后**整个池耗尽**，表现是所有
请求一起超时而非「媒体操作失败」。生产默认 20，余量充足。

### `search_filters` —— 完成

对应上游 `playback/search_filters.py`（61 行）。**14 个单元测试**，纯逻辑、
不需要数据库。

**它是番号归一化规则的唯一实现** —— 与影片搜索
`MovieService._number_search_target` 共用（上游 docstring 明说「容忍度一致」）。
所以同一个词在影片列表能搜到，在时刻点/片段列表也必须能搜到。

| 上游 | 本仓库 |
|---|---|
| `normalized_number_contains` | `number_condition` + `normalize_number_term` + `normalized_column_expr` |
| `keyword_conditions` | `TermCondition::from_parts` + `and_all` |

**归一化三步**（顺序不能换）：`UPPER` → `TRANSLATE(col, '-_', '')` 删分隔符
→ `REPLACE(..., 'FC2PPV', 'FC2')` 折前缀。

**一条快路径**：`^\d+[-_]\d+$`（如 `2023-001`）直接用**列原文** +
**未归一化的词**做 `LIKE`，跳过 `TRANSLATE`/`REPLACE` —— 那是全表扫描里
最贵的部分。

**两个容易写错的语义**：

1. **可搜索字符 = `is_ascii_alphanumeric()`**，不是 `is_alphanumeric()`。
   上游 `char.isascii() and char.isalnum()` 里那个 `isascii()` 不是冗余的
   —— Python 的 `isalnum()` 对 `文字` 返回 True，而 Rust 的
   `is_alphanumeric()` 对 `文` 也返回 True。只写后者的话「搜一个纯中文词」
   会落进番号条件，而不是被恒假掉。
2. **一个词匹配不到任何字段时必须恒假，不能忽略**。上游是
   `peewee.SQL("FALSE")`：静默丢弃会让用户看到「搜了个没用的词，列表没变」
   —— 而他真正想要的过滤**没发生**。搜索时刻点（无番号字段）时输入
   `abc-123` 就走这条：结果为空是**正确**的。

**词之间 AND、词内 OR** —— 上游不在 `keyword_conditions` 里加 `AND`（交给
Peewee 的 `where(*conditions)` 默认处理），Rust 侧用 `and_all` 显式化：
「词之间 AND」是这条规则里**最容易被漏掉的一半**，漏了它多词搜索会变成
「任一词命中」，结果集大得多且看不出原因。

### 剩下 16 个文件

| 文件 | 行数 | 备注 |
|---|---|---|
| `media_summary_service` | 40 | **完成** |
| `operation_locks` | 52 | **完成** |
| `thumbnails/progress` | 56 | 纯逻辑 |
| `search_filters` | 61 | **完成** |
| `media_file_hash_backfill_service` | 112 | `media-file-hash` 库已就位 |
| `media_validity_scan_service` | 188 | 判活扫描 |
| `thumbnails/artifacts` | 204 | 依赖 `svc-probe` |
| `media_thumbnail_pack_backfill_service` | 216 | |
| `media_video_info_backfill_service` | 232 | 依赖 `svc-probe` |
| `media_library_service` | 331 | 它的 `storage_space_usages` 是 `system::status` 磁盘三列的等待方 |
| `media_metadata_probe_service` | 338 | 依赖 `svc-probe` |
| `thumbnails/task_service` | 508 | 缩略图 worker；候选查询口径已被 `StatsRepository` 引用 |
| `media_clip_service` | 535 | |
| `media_service` | 781 | |
| `provider_helpers` / `thumbnails/contracts` | 24 / 22 | 阻塞：插件 ABI |

## `catalog` —— 进行中（演员子域 + 影片订阅流转）

上游 `src/service/catalog/` 27 文件 / 7,556 行。已落四块：

| Rust 模块 | 上游 | 说明 |
|---|---|---|
| `catalog::resolution` | `movie_resolution_service.py` 的档位部分 | 被 `collections` 的列表端点引用 |
| `catalog::actor` | `actor_service.py`（827 行） | 11 个端点里可做的 9 个方法 |
| `catalog::actor_merge` | `actor_merge_service.py`（208 行） | `POST /actors/{id}/merge` |
| `catalog::movie` | `movie_service.py`（1,177 行）的订阅流转 + 最新到货 + 合集标记 + 黑名单 + **影片列表** | 番号定位 + 批量订阅/退订 + `GET /movies`（15 个筛选位 + 相关度排序）+ `/latest` + `GET /{n}/collection-status` + `PATCH /collection-type` + `PUT`/`DELETE /blacklist`（8 条端点） |

### 影片订阅流转（本批落地）

`POST /movies/subscriptions` 与 `POST /movies/unsubscriptions`，外加
`MovieService::find_by_number` / `require_by_normalized_number`（后面 20 条
影片端点都要用的共用件）。

三条值得记住的规则：

1. **「番号不存在」不是 4xx 而是 `skipped`**。批量端点走部分成功：用户勾了
   20 部、其中 1 部已被删除时，他要的是另外 19 部订上。
2. **去重不互换分隔符**（`_` 与 `-` 不折叠）。两种分隔符的番号同时存在时
   （一本道 / 加勒比同日番号），折叠会让「订阅这部」扩散到**另一部影片**。
3. **`updated_count` 是上游的一处双重计数**：被拉黑而跳过的那几条仍被算进
   `updated_count`（同时也进 `skipped`）。**照抄** —— 客户端已按这个数字渲染，
   改掉就是静默的契约变更；`tests/movie_subscriptions_http.rs` 把当前形状钉住
   并说明了原因。

另外记一个踩过的坑：`UpdateSet` 的 `ValueInner::Null` 在绑定层是
`Option::<String>::None`，即一个 **text 类型的 NULL** —— 对 `timestamp` 列会报
`expression is of type text`。所以「清订阅时间 / 重置检索状态」走的是
`MovieRepository::mark_subscribed` / `clear_subscription` 里**字面量 `NULL`**
的窄更新（与 `ActorUpdate` 单独维护 `nulls` 列表同一个思路）。

### 演员子域的 HTTP 层

### 演员子域的 HTTP 层（本批落地）

`sm-service::catalog::actor` 的 9 个方法接进 `sm-api`，落了 **10 条路由**
（订阅的 `PUT`/`DELETE` 算两条）。DTO 逐字段对齐上游
`src/api/routers/catalog/actors.py` 与 `src/schema/catalog/actors.py`。

**两类上游发生在 service 之前、本层必须补的校验**：

1. `cups` 是逗号分隔字符串，`_parse_cups` 负责 trim / 大写 / 非 ASCII 字母即
   422 `invalid_actor_filter`（`details.cups` 回显原始输入）；
2. `age_min`/`age_max` 的 `ge=0`、`height_min`/`height_max` 的 `ge=1` 由
   pydantic 承担 —— 不复刻的话 `age_min=-5` 会被 `years_before(today, -5)`
   算成未来日期而**静默命中全部演员**。

**`PATCH` 收原始 JSON 对象**：上游靠 `exclude_unset` 区分「键不存在」与
「键存在但为 `null`」，serde 的 `Option<T>` 做不到，所以 handler 把 body 当
`serde_json::Map` 透传给 service。

**仍阻塞的两条**（逐条记在 `crates/sm-api/src/routes/actors.rs` 的模块文档）：

| 上游 | 阻塞原因 |
|---|---|
| `PUT /{id}/profile-image` | EXIF 旋转 + LANCZOS + **有损** WebP（阶段 9） |
| `POST /search/javdb/stream` | JavDB provider + `CatalogImportService`（插件 ABI） |

### 演员合并（`actor_merge_service`）

`POST /actors/{id}/merge` 已落，实现在 `sm_service::catalog::actor_merge`，
对齐上游 `actor_merge_service._apply_merge` 的**六步**：搬影片关联 / 合并别名 /
合并订阅（`subscribed_at` 取最早）/ 填空受保护字段（跳过 `host:manual` 的来源）/
搬头像 / 打墓碑并压平链。整体在一个显式事务里（`pool.begin()` + `Ctx::in_tx`）。

**这里抓到一个会挂死测试的真陷阱：不要依赖 `Transaction` 的 `Drop` 来回滚。**

`Drop` 只把 `ROLLBACK` 排队到「这条连接下次被签出」时才执行 —— 连接于是以
`idle in transaction` 留在池里，并继续持有 `FOR UPDATE` 的行锁。集成测试夹具
的池是 `max_connections(1)`，而它的收尾用**另一条**连接执行
`DROP SCHEMA ... CASCADE`，那条会一直等这把行锁：表现是**用例体跑完但进程
挂住不动**，PG 里能看到「一个 idle in transaction + 一个 DROP 在等 relation 锁」。

所以错误路径必须**显式**回滚。这条规则现在收敛成一个函数：
`sm_db::repo::commit_or_rollback`（`Ok` 提交、`Err` 显式回滚，事务体放
`async {}` 块里，里面的 `?` 不会把未结束的事务带出外层函数）。本批把
`sm-service` 里所有「事务体可能早退」的点位都改用了它：

| 位置 | 事务体里的失败点 |
|---|---|
| `videos::collection::add_item` | 唯一违例的幂等分支 |
| `videos::collection::remove_items_by_video_ids` | 两次仓储调用 |
| `videos::collection::reorder` | 成员行被并发删掉 |
| `system::activity_cleanup`（两个清理方法） | 三条仓储调用 |
| `collections::ordered::set_members` | 清空 + 逐条插入 |
| `catalog::actor_merge` | 加锁后的四类校验 |

`sm-db` 侧的 `user::rotate` 与 `common::page::in_snapshot_tx` 更早就已是显式
提交/回滚 —— 前者的文档里就写着这个坑，本批只是把它推广到了别处。

## `transfers` —— 进行中

上游 `src/service/transfers/` 23 文件 / 4,235 行。已落两块，都在**不依赖插件
ABI** 的那一侧 —— 这个域的大头全在 `sm-plugins` 宿主后面，见「阻塞地图」：

| Rust 模块 | 上游 | 说明 |
|---|---|---|
| `transfers::torznab` | `downloads/clients/torznab.py`（296 行） | 跨索引器搜索 + 候选构造 |
| `transfers::download_search` | `downloads/search_service.py`（98 行） | `GET /download-candidates` |

### 候选搜索：三条规则

1. **番号必须先大写**。比对的两边（请求里的番号、候选标题里解析出的番号）都过
   `movie_numbers`，而识别是大小写不敏感的 —— 不适时大写，`?movie_number=ssni-888`
   会把标题里的 `SSNI-888` 全部判成「错配」而剔除，表现为「搜得到却一条不返回」。
2. **标题过滤是启发式，不是内容闸门**：解析出**别的**番号的候选直接剔除，
   解析不出的保留，交给提交阶段的内容闸门做最终确认。比对**不折叠分隔符**
   （`123456-789` 与 `123456_789` 是两部不同的影片，见 `movie_numbers` 的模块文档）。
3. **「索引器全挂了」是 502 而不是 200 空列表**（`download_candidate_search_failed`）。
   前者客户端该重试，后者是「这个番号没有资源」—— 两者处置相反，合并会让
   「服务不可用」伪装成「没有资源」。

### 仍未开工

转存/下载编排（`import_service` / `task_service` /
`media_transfer_task_service`）、`downloads` 客户端注册与优先选择，以及
`GET/POST /download-clients`、`POST /download-requests`、`GET /download-tasks`
等 8 条端点。它们都要「下载器 provider」，即插件 ABI 宿主。

仓储层已就绪、可直接被这个域使用的部分：`download` / `transfer` /
`submission` / `task`。

## `discovery` —— 框架已铺，方法体待实现

**2026-10-05**：16 个上游文件里 **12 个的文件框架已铺**
（`sm-service/src/discovery/`，逐文件对照表见该目录 `mod.rs`）。

**本轮只铺框架** —— 签名、类型、错误语义、模块文档按上游定好，方法体是
`todo!()`，**且未做编译验证**（重构阶段）。所以「已铺」指类型与依赖经代码
走查对得上上游，**不是「验证过能跑」**。

**2026-10-07 更新**：`daily_recommendation_service.py`（18.7KB）
**读侧 + 生成侧都已落地并验证**：

- 读侧 `c801441`：`list_items` + `GET /daily-recommendations` + 完整影片卡片
  （`DailyRecommendationMovieResource`）；
- 生成侧 `2655f4d`：`generate_latest_snapshot` + 四个 IO 装载器 + Qdrant 相似度
  降级，并注册 `daily_recommendation_generate`（worker handler 5 → 6）；
- 测试：17 个纯函数单测 + 6 个真库集成测试
  （`crates/sm-service/tests/daily_recommendation_generate.rs`）。

未铺的一个是超大文件：`moment_recommendation_service.py`（24KB）—— 它消费
`recommendation_service` 的产出，单独一轮。

### 外部依赖：**已无阻塞**

| 依赖 | 状态 |
|---|---|
| Qdrant 客户端 | ✅ `qdrant-client 1.19` 已接入，`discovery/qdrant/` 下稠密 + 稀疏两套齐了 |
| Qdrant 服务端 | ✅ Podman 跑在 `127.0.0.1:6334` |
| 推理服务 | ✅ 客户端已按上游 HTTP 契约实现（`discovery/embedding.rs`）；**服务本身待定接什么** |
| provider 插件 | ❌ 唯一剩下的外部依赖 —— 只卡 `ranking` 的**写侧**（`RankingSyncService`）与同族的 daily/moment 推荐 |

### 四个文件**从一开始就不卡任何外部服务**

早期这节把整个 `discovery` 标成「卡 Qdrant」，那是关键词 grep 的结果。按
`handoff.md` 的 import 段纪律重新判定后，以下四个只 import `peewee` /
`src.model` / `PIL` / `optional_services` —— **纯 PostgreSQL**：

- `ranking.rs`（21.7KB，**全域最大的单文件**；`sm-db` 侧 13.4KB 早写好了）
- `hot_actress_release.rs`（9.3KB）
- `image_search_space.rs`（4.5KB + 848B）
- `image_search_reset.rs`（980B）

所以「`discovery` 卡 Qdrant」这个说法应当作废 —— 现在卡的是**方法体**，不是
外部依赖。

## 跨域次序：硬约束已解除，影片卡片已在 `collections` 侧定型

依赖次序上原有的硬约束**已解除**：`playback` 的 `movie_resolution_service`
被 `collections` 的列表端点引用，所以当初要求 `playback` 早于 `catalog`。
其中与分辨率档位有关的两块（`resolution_interval` / 档位分桶）已随
`sm_service::catalog::resolution` 落地，档位**筛选**的 `EXISTS` 子查询随
影片卡片一起进了 `sm_db::repo::collection`，`GET /playlists`、
`GET /playlists/{id}/resolutions` 与 `GET /playlists/{id}/movies` 因此全部接上。

`playlists` 域**9/9 端点全部落地**（最后一个是影片卡片）。它没有等 `catalog`
侧开工，理由恰恰相反：影片卡片是 `catalog` 的影片列表**同一套东西**，先在这里
把 `MovieListItemResource` 与聚合口径定下来，`catalog` 侧直接复用同一组 DTO
与仓储查询，避免两边各写一份聚合后开始漂移。

### 影片卡片的四条口径（`catalog` 侧复用时要一致）

1. **顺序只由分页查询决定**（`sm_db::repo::collection::list_movie_cards`）。
   后来补的影片本体 / 封面 / 系列 / 媒体都是按 id 批量补，**不得重排** ——
   在内存里重排会让 `added_at` / `bitrate` 两个相关子查询排序列直接失效。
2. **档位区间是半开的**：`4K → [6, 7)`。闭区间会让 8K 影片同时命中 4K。
3. **`can_play` 是 any 不是 all**：一条有效媒体就够。
4. **`video_info` 是对象**：库里 TEXT 存 JSON，DTO 输出解析后的对象；
   脏文本给 `null`（上游在那个情况下 500，而列表不该因为一条坏值整页拿不到）。

---

## 跨域待核对项

不是「忘了做」，而是需要上游代码或真实数据才能定的：

| 项 | 现状 | 闭合条件 |
|---|---|---|
| SSE 的三个端点 | 传输骨架已就位（`sm_api::sse`，10 个事件名 + 3 个路径已按上游核实） | `catalog` 域落地后注册路由 |
| `/files/*` 与 `/media/{id}/play/{path}` 的旁路签名路由 | `sm_core::signing` 与 403 三码已就位 | provider 资源就绪（插件 ABI） |
| multipart 上传路由 | 提取器已就位（`sm_api::extract::Multipart`，强制 8 MiB 上限） | 插件 zip / 图片上传落地 |
| `video_cover_service` 的首帧封面 | 不落地 | `svc-probe`（ffprobe）+ `svc-image` 的有损 WebP 路径 |
| 后台任务 **worker**（领取与执行） | **队列侧已就位**（`sm_service::system::task_queue`），只差 handler | 各域的 service 就位后写 handler |
| 启动引导任务（`trigger_type = "startup"`） | 常量已留，逻辑未落 | `gfriends_filetree_refresh` 与 `movie_similarity_recompute` 的就绪判定 |
| `settle_bootstrap_blocker` | 队列原语已就位，这个方法未落 | 同上 —— 两个引导任务的 service 就位后补 |
| 查询串的信封形状 | **已闭合** —— `sm_api::extract::Query` 把 `QueryRejection` 映射成 422 `validation_error` | — |
| 布尔查询参数的字面量集合 | **已闭合** —— `sm_api::query::deser_bool` 还原 pydantic 的 12 个字面量 | — |
