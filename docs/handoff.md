# 续做交接

给「下一次接着干」的人（或新会话里的 AI）看。按本文档开工，不需要重读整个会话历史。

## 一、当前状态

| 项 | 值 |
|---|---|
| API 端点 | **76 / 126（~60%）** —— 方法级口径，实测 2026-10-05 |
| 服务域 | 6 个域有代码（`system` 17 文件 / `catalog` 7 / `playback` 6 / `collections` 4 / `transfers` 3 / `videos` 3），`discovery` 未开工 |
| 调度 | 19 个内建任务，cron **16/16 全注册**（3 个是 `manual_only`，按 `contracts.py:43-49` 本就不该有 cron）；worker 骨架已落地，**handler 1/21** |
| 门禁 | 七道全绿（fmt / doc / clippy / workspace test / schema 40-40 对拍 / hash 44-44 / core 64-64 / paged wrappers） |
| 代码量 | `sm-service` src 43 文件 / 13,715 行；tests 15 文件 / 6,809 行 |

开工前先跑一遍 `bash scripts/verify.sh` 确认基线是绿的，再动手。
**`upstream/` 是目录联接（junction）**指向 `../sakuramediabe` 等真实仓库，
不占额外磁盘 —— 别把它当成副本删掉。

## 二、上游参照物（都已克隆在 `upstream/`，`.gitignore` 忽略）

| 目录 | 是什么 |
|---|---|
| `upstream/sakuramediabe` | **后端 Python**（重写对象）。契约的唯一权威来源 |
| `upstream/sakuramedia` | **前端 Flutter 客户端**（Dart）。想验契约对拍时用 |
| `upstream/sakuramedia_local_provider` | **真实插件**（本地存储 + qBittorrent）。插件契约样本 |
| `upstream/sakuramedia_115_provider` / `sakuramedia_javbus_metadata` | 另两个官方插件 |

对齐顺序：**先读上游 router → schema → 每一处 `ApiError(...)` 调用点**，再动手。不要凭印象推契约。

## 三、继续做：按块走，一次做完一块

### 块 A：插件宿主（进行中，①② ③④⑥ 已完成）

已完成：注册校验、能力注册表、加载器（连接 + Register + 收声明）、错误映射、任务注册表、`RunJob` 的流式执行与事件收敛、插件任务接进 cron 触发。
待做（按序）：

1. ~~**`RunJob` 的流式调用**~~ **已完成**（`runner.rs` + `scheduling.rs`）：`stream JobEvent` 收敛成终态，超时即 `drop(stream)` 表达取消（proto 的「宿主直接断开流」）；cron 触发那半是 `JobDefinition.default_cron` / `manual_only` → `sm_scheduler::JobSpec`，与内建任务共用同一套到点判定与 coalesce。
   组合根**已接上**（原先这里写的是「仍刻意不依赖 `sm-plugins`」，与下面第 3 步的「看门狗与组合根也已接上」自相矛盾 —— `sm-server/Cargo.toml` 里 `sm-plugins = { workspace = true }`，`sm-server/src/plugins.rs` 存在，插件任务已并进调度表）。
   剩一处刻意的局限：插件重启后**新增**的 cron 任务要等下次进程启动才生效（`Scheduler` 的任务清单构造时定死），已在表里的不受影响。
2. **三个扩展点的调用面**：`media.provider`（已有注册表）/ `catalog.metadata_source` / `discovery.ranking_source`。
   **已完成**（`extensions.rs` + `extension_calls.rs`）：两个扩展点的载荷校验、`source_key` / `board_key` 形状、缺 capability 不收、排行榜 `source_key` 冲突时该插件的榜单全部不收（对齐 `apply_plugin_ranking_sources`）；调用面真发 rpc，并把「未收录」（`found=false`）与「调用失败」分成两类结果。
   **交付校验已补**（`movie_delivery.rs`）：图片必须落在 `FetchMovieRequest.delivery_dir` 内、是普通文件、再深一层且同一请求目录；`release_date` 严格 `YYYY-MM-DD`、`duration > 0`；用完 `cleanup_delivery`。判据是 proto 给的，不依赖 `plugins.root_dir`。
   两处**还没做**：一是「冲突时连该插件的任务一起不注册」—— 要等加载器把任务表与扩展点表串起来；二是**番号一致性**不在这一层判（`normalize_movie_number` 住在 `sm-service`，那条依赖边将来会成环），由导入方比；三是**入库路径**还没有（catalog 域缺插件导入服务，拿到校验过的结果也没处写）。
3. **进程生命周期**：拉起进程、握端口、重启看门狗（`loader.rs` 刻意没做）。
   **已完成大半**（`supervisor.rs` + 参考插件可执行文件）：宿主分配端口并经 `SAKURAMEDIA_PLUGIN_GRPC_ADDR` / `SAKURAMEDIA_PLUGIN_ID` 注入 → 拉起 → 用 `Register` 探活 → `wait()` 发现崩溃 → `restart_backoff` 给退避。协议是自定的（上游是进程内 import，proto 无此约定），依据见 `docs/adr/2026-10-05-plugin-lifecycle.md`。
   **看门狗与组合根也已接上**（`sm-server/src/plugins.rs`）：`sm-server` 现在依赖 `sm-plugins`，按 `plugins.enabled` 逐个拉起、收三张注册表、把插件任务并进调度表（`cron_info` 里看得到），并起一个看门狗轮询崩溃 → 退避重启 → 重建注册表。
   **一处刻意的局限**：重启后不重挂调度表（`Scheduler` 的任务清单构造时定死），所以插件重启后**新增**的 cron 任务要等下次进程启动才生效；已在表里的不受影响。
4. **数据面**（`data_plane_endpoint`）：**经查证上游 Python 与 proto 均无协议定义**，是预留设计位 —— 协议定了再做，不要凭空发明。

### 块 B：`system` 域 —— `jobs` 与 `activity` 已落地

原写「剩 11 个文件」，实测已经不成立：

- **`jobs`**：`GET /system/jobs`（列表）与 `POST /system/jobs/{task_key}/run`（触发）两条都在 `routes/jobs.rs`。列表的 `last_task_run` 用一次子查询（`MAX(id) GROUP BY task_key`）拿全部，不是 N+1。
- **`activity`**：整包 6 个端点在 `routes/activity.rs` —— bootstrap / notifications 列表 / 批量已读 / 全部已读 / task-runs / task-runs-active。DB → service → 路由三层都在。

剩下的 `telemetry`（约 188 行）可做；`plugins` 那部分要等插件宿主，`image_search_reset` 与 `metadata_provider_probe` 要等 Qdrant 与 metadata source。

### 块 C：插件 ABI 之后才解锁的（约 40 条端点）

transfers 编排、`/files/*` 与 `/media/{id}/play/{path}` 签名路由、multipart 上传、`system/plugins`、`{n}/reviews`、JavDB 导入。

### 卡死的（不用试）

- `GET /movies/{n}/subtitles` —— 要读媒体文件系统（provider 族）
- `GET /movies/{n}` 详情 —— 要 playback 的进度/打点 + rankings
- `discovery` —— 要 Qdrant

## 四、纪律（踩过坑才定的）

1. **直接推 `main`**。本地分支就是 `main`，提交后直接 `git push cnb main`，
   不开主题分支、不走 PR。

   > 这条**替换**掉了原先的「一个主题一条分支一个 PR」。原规则的理由是
   > 「PR 开着时往同一分支继续推新提交，对方中途合并会让后面的提交**静默
   > 掉队**（实测掉过 4 笔）」—— 那条风险只在**有 PR 评审**时成立：合并
   > 动作由别人触发，你的提交会落在一个已经移动过的分支上。没有 PR 就没有
   > 「别人中途合并」这个环节，风险不存在。
   >
   > 保留一句提醒：**推之前先 `git fetch` 看 `cnb/main` 有没有动过。** 多个
   > 执行体（CNB 的 auto 分支 agent、你自己）可能同时在写主线。
2. **先读跨文件依赖再开工**。三次半路撞墙（`subtitles`、`subscriptions`、proto 能力）都是因为只读了当前文件。尤其是 proto：`plugin.proto` **早就有 `enum Capability` 与 `PluginControl.Register`**，不要按「需要拆 service」的假定去改。
3. **不要发明协议**。上游没实现的（数据面）、proto 没定义的，先查证再动手。
4. **SQL 用 `QueryBuilder`**，不要拼字符串（占位符编号会静默错位）。
5. **置空只能用 SQL 字面量 `NULL`**：`UpdateSet` 的 `ValueInner::Null` 在绑定层是 text 类型，对 timestamp 列直接报错。
6. **受保护字段**（`is_collection` / `is_blacklisted`）必须经 `MovieOwnershipGateway`，否则自动规则会覆盖人工标记。
7. **重复常量是缺陷**：`ABI_MAJOR` 之类的只留一份（现在复用 `sm_plugin_api::ABI_MAJOR`）。

## 五、上游的两处「缺陷」，已刻意照抄

- `/movies/latest` 的 `total` 不带黑名单过滤（与当页口径不一致）
- 订阅端点的 `updated_count` 是双重计数（跳过没写的也算进去了）

照抄理由与测试都写在对应模块文档里。**要修请单独开一个 fix**（会影响客户端已渲染的数字），不要顺手改。

## 六、待确认/待办

- ~~`scripts/run-tests.sh`（未跟踪）引用了不存在的 `scripts/test_targets.py`~~ —— **已失效**：工作区 0 个未跟踪文件，`scripts/run-tests.sh` 本身已不存在。
- `stash@{0}` 还在，内容**不是**上面那条 —— 是「契约层拆仓」那批（根 `Cargo.toml` 改 `git + tag = "v0.1.0"` 依赖、删 `crates/sm-plugin-api/` 与 `proto/`，10 文件 -2315 行）。契约 crate 本身已验证能编译，**三个引用方（`sm-plugins` / `sm-server` / `plugin-ref-local`）当时未验证**就存起来了。取出前先跑一遍那三个 crate 的 `cargo check`。
- 前端契约对拍还没做（前端已在 `upstream/sakuramedia`）。已知两处可能与前端不一致：分页响应多一个 `synced_at: null`；时间戳是 naive UTC 而上游是运行时本地时区。
