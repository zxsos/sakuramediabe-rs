# Peewee → sqlx 模型映射进度

后端 `src/model/__init__.py` 导出 **40 个模型**，分 7 个域。schema 完全保留，
不引入迁移框架，因此每个模型都需要逐字段映射且列名/类型不可漂移。

## 类型映射约定

| Peewee | PostgreSQL | Rust | 备注 |
|---|---|---|---|
| `CharField(max_length=N)` | `varchar(N)` | `String` | N 不影响 Rust 侧 |
| `TextField` | `text` | `String` | |
| `IntegerField` | `integer` | `i32` | |
| `BigIntegerField` | `bigint` | `i64` | |
| `FloatField` | `double precision` | `f64` | |
| `BooleanField` | `boolean` | `bool` | |
| `DateTimeField` | `timestamp` | `NaiveDateTime` | **naive UTC**，非 `DateTime<Utc>` |
| `ForeignKeyField` | `<field>_id integer` | `Option<i64>` | Peewee 虚拟外键为真实整型列 |
| `JsonbField` | `jsonb` | `serde_json::Value` | |
| `JsonTextField` | `text` | `Option<String>` | **JSON 存 TEXT**，需手动序列化 |

### 两个容易踩的坑

1. **`JsonTextField` 不是 JSONB。** 它把 JSON 序列化成字符串存进 `text`，
   且空串视为 `None`。只有 `JsonbField` 才是真正的 `jsonb` 列。
2. **时间是 naive 的。** Peewee 写 naive UTC，列类型 `timestamp without time zone`。
   用 `DateTime<Utc>` 会让 sqlx 按 `timestamptz` 解码而报错。

## 进度

| 域 | 模型数 | 已完成 | 状态 |
|---|---|---|---|
| `catalog` | 9 | 9 | **完成** |
| `collections` | 6 | 6 | **完成** |
| `discovery` | 5 | 5 | **完成** |
| `playback` | 6 | 6 | **完成** |
| `system` | 5 | 5 | **完成** |
| `transfers` | 6 | 6 | **完成** |
| `videos` | 3 | 3 | **完成** |
| **合计** | **40** | **40** | **100%** |

> 这张表此前把 `collections` 与 `videos` 各列了**两行**（一行「0 / 待做」、
> 一行「N / 完成」），`system` 那行的状态还串到了下一行，于是「合计 40/40」
> 与逐行相加对不上。域清单以 `src/model/` 的实际目录为准：
> `catalog` / `collections` / `discovery` / `playback` / `system` /
> `transfers` / `videos`，共 7 个域 40 张表。

## 已映射

### `catalog::Movie`（40 字段，样板）

选它做样板是因为它同时包含：全部字段类型、`JSONB`、多个可空外键、
`CHECK` 约束、以及「字段主权」这类别处的业务语义。

映射时确认的细节：

- `javdb_id` 空串在 save 时归一为 `NULL` → `Option<String>`
- `movie_number` **不做归一化改写**（分隔符与大小写都是有效信息）→ `String`
- `mutation_revision` 只覆盖受保护字段，**不是整行版本** → 与 proto 的 `MovieSnapshot.revision` 同语义
- `CHECK (NOT (is_subscribed AND is_blacklisted))` 在 Rust 侧镜像为
  `satisfies_blacklist_constraint()`，让 service 层能提前拦截而不是等数据库报 500

## 验证方式

- `cargo test -p sm-db` 校验受保护字段白名单与 CHECK 约束语义
- schema 一致性最终由集成测试保证：连接真实 PostgreSQL 后逐表比对列名与类型

> 当前环境无 PostgreSQL 实例，因此 `sqlx::query!` 的编译期校验尚未启用。
> 接入实例后应改用宏形式，让 schema 漂移在编译期暴露。

## 非结构信息（schema 之外，但必须保留）

### `system::User` / `system::UserRefreshToken`（认证链路）

### `playback` 域全部 6 个（完成）

| 模型 | 表 | 关键点 |
|---|---|---|
| `MediaLibrary` | `media_library` | `provider_config` 是 JsonTextField |
| `Media` | `media` | 外键指向 `Movie.movie_number`（字符串） |
| `MediaThumbnail` | `media_thumbnail` | `(media, offset)` 唯一 |
| `MediaProgress` | `media_progress` | `media` 唯一（一条 Media 至多一条进度） |
| `MediaPoint` | `media_point` | 三种删除行为并存 |
| `MediaClip` | `media_clip` | 独立资产，来源删除后 SET NULL |

**Media 归属不变量**：`movie_number` 与 `video_item_id` 恰好其一非空。
两者都空或都非空都会被 `Media.save` 拒绝 —— 解耦后一条 Media 归属
movie（JAV）或 video_item（非 JAV）之一。已镜像为 `satisfies_owner_constraint()`。

**缩略图状态机在 `Media` 而非 `MediaThumbnail`**：后者是成功产物，承担不了失败、
退避与人工重试。索引 `(thumbnail_generation_state, thumbnail_next_retry_at)`
决定只有 `retry_wait` 会被退避扫描命中。

**`MediaThumbnail` 比 `MoviePlotImage` 多一个状态**：多出 `SKIPPED = 3`，
因为非 JAV 媒体的缩略图不参与图像检索向量索引，需要落明确终态
避免长期滞留 PENDING。两个表的同名字段取值范围不同，不要混用。

**删除行为差异（改动 schema 会破坏语义）**：

| 表 | 关系 | on_delete |
|---|---|---|
| `media` | movie / video_item / library | CASCADE |
| `media_point` | media / thumbnail | SET NULL |
| `media_point` | image | **RESTRICT** |
| `media_clip` | media | SET NULL |

`MediaPoint` / `MediaClip` 的 `movie_number` 是**快照**，不建外键，
所以删除影片后时刻点与片段仍可归属与展示。

### `videos` + `collections` 域全部 9 个（完成）

| 模型 | 表 | 关键点 |
|---|---|---|
| `VideoItem` | `video_item` | 非 JAV 条目，与 `Movie` 平行 |
| `VideoCollection` | `video_collection` | 实际在 `videos` 目录，不在 `collections` 域 |
| `VideoCollectionItem` | `video_collection_item` | 有 `position` |
| `Playlist` | `playlist` | **唯一有 `kind` 字段** |
| `PlaylistMovie` | `playlist_movie` | **无 `position`** |
| `MomentCollection` | `moment_collection` | — |
| `MomentCollectionItem` | `moment_collection_item` | 指向 `MediaPoint` |
| `ClipCollection` | `clip_collection` | — |
| `ClipCollectionItem` | `clip_collection_item` | 指向 `MediaClip` |

**三种合集同构但有三处差异**，迁移时不能当成同一张表：

| | `Playlist` | `MomentCollection` | `ClipCollection` |
|---|---|---|---|
| 成员指向 | `Movie`（JAV 影片） | `MediaPoint` | `MediaClip` |
| **`position`** | **无** | 有 | 有 |
| **`kind`** | **有** | 无 | 无 |

① `PlaylistMovie` 没有 `position` —— JAV 侧播放顺序只能靠加入先后，
视频侧与时刻/片段侧都显式维护。`playback_order_key()` 返回类型因此不同
（`i64` vs `(i32, i64)`）。

② 只有 `Playlist` 有 `kind` 区分系统列表（`recently_played`）。数据库无
CHECK 约束，脏值只能靠 `is_valid_kind()` 挡住。

③ 三者都有 `(owner_plugin_id, plugin_key)` 唯一索引，但 **NULL 不参与
唯一约束**，所以它防的是「同一插件重复注册同一 key」，不保证 `name`
唯一（`name` 自身带 unique）。Rust 侧用 `PluginOwned` trait 统一。

**排序次级键不可省**：`MomentCollectionItem` / `ClipCollectionItem` / 
`VideoCollectionItem` 都用 `(position, id)` 复合键 —— 删除后重排会让多条
成员 `position` 相同，只按 `position` 排序会导致播放列表抖动。

### `transfers` + `system` 剩余 9 个（完成）

| 模型 | 表 | 关键点 |
|---|---|---|
| `DownloadClient` | `download_client` | provider_config 不透明 |
| `Indexer` | `indexer` | `api_key` 为空则不带 apikey 参数 |
| `IndexerDownloadClient` | `indexer_download_client` | 多对多，`(indexer, client)` 唯一 |
| `DownloadTask` | `download_task` | **两个独立状态机** |
| `DownloadSubmissionRecord` | `download_submission_record` | **裸整数，非外键** |
| `DownloadResourceBlacklist` | `download_resource_blacklist` | 40 位 v1 info hash |
| `BackgroundTaskRun` | `background_task_run` | **任务队列 + 租约回收** |
| `SystemNotification` | `system_notification` | 新旧两套关联字段并存 |
| `SchemaMigration` | `schema_migration` | **无 created_at/updated_at** |

**① `DownloadTask` 有两个互不相干的状态机**

| 列 | 归属 | 默认值 |
|---|---|---|
| `state` | provider 的远端下载状态 | `queued` |
| `import_status` | 宿主自己的导入流程 | `pending` |

源码注释：「导入是宿主自己的业务流程，不能与 provider 的远端状态混用」。
合并成一个 status 列会丢掉「下载完了但导入失败」这个真实存在的状态组合 ——
`is_stuck_after_download()` 就是为这个组合准备的。

**② `download_submission_record` 用裸整数而非外键**

`client_id` / `task_id` 都是 `IntegerField` 而非 `ForeignKeyField`。
注释：「保留提交历史，不随下载任务或下载器删除」。若在迁移时
「顺手」改成真外键 + CASCADE，提交历史会被连带删除，而这正是该表
存在的意义。`is_orphaned()` 标记任务已删但记录仍在的情形。

**③ `background_task_run` 是任务队列，不是日志表**

表头注释：「pending 行即队列元素；lease_expires_at 过期即可回收」。
配套索引 `(state, scheduled_at)` 服务于领取路径：

```sql
WHERE state = 'pending' AND scheduled_at <= now ORDER BY id
```

`mutex_key` 是**单列唯一索引**（NULL 不参与唯一约束，故多个无互斥
需求的任务可共存）。租约过期回收是必需机制 —— 没有它，崩溃的
worker 会让任务永久卡在 `running`。`is_stale_lease()` 检这个状态。

**④ `system_notification` 新旧两套关联字段并存**

| 用途 | 字段 |
|---|---|
| 事件身份 | `event_type` / `resource_type` / `resource_id` |
| 展示关联（遗留） | `related_resource_type` / `related_resource_id` / `related_task_run_id` |

源码注释：「事件身份与展示关联分离：旧 related_resource_* 继续服务现有 API」。
这是过渡期的有意设计，合并会破坏现有 API 契约。

**⑤ `SchemaMigration` 是全库唯一没有 `TimestampedMixin` 的表**

它继承 `BaseModel` 而非 `TimestampedMixin`，所以**没有 `created_at` /
`updated_at`**，只有一个 `applied_at`。迁移记录只追加，语义上不需要
「创建时间」与「更新时间」之分。Rust 侧的 `applied_at` 因此是非 `Option`。

**待对齐**：`task_state` 的终态字面量（`succeeded` / `failed`）在
`src/model/` 下没有常量定义，只有 `pending` 可从源码取证。实际字面量由
service 层决定，迁移服务层时需与上游核对。

### `catalog` 域全部 9 个（完成）

| 模型 | 表 | 字段数 | 备注 |
|---|---|---|---|
| `Movie` | `movie` | 40 | 样板：字段主权 + CHECK 约束 |
| `MovieSeries` | `movie_series` | 4 | |
| `Actor` | `actor` | 24 | `birthday` 是 **date** 不是 timestamp |
| `Image` | `image` | 4 | `origin` 前缀索引 |
| `Tag` | `tag` | 4 | |
| `MovieActor` | `movie_actor` | 3 | `(movie,actor)` 唯一 |
| `MovieTag` | `movie_tag` | 3 | `(movie,tag)` 唯一 |
| `MoviePlotImage` | `movie_plot_image` | 4 | 图搜索引状态 0/1/2 |
| `Subtitle` | `subtitle` | 4 | `(movie,file_path)` 唯一 |

映射时确认的细节：

- **`birthday` 是 `DateField`**（PostgreSQL `date`），不是 `DateTimeField`。用错类型会让 sqlx 按 timestamptz 解码。
- **别名合并是大小写不敏感去重**：`merge_alias_name` 用 `casefold` 判重但保留首次写法，主名恒排首位。读时按 `/` 拆分，写时用 ` / ` 连接。
- **墓碑指针 `merged_into` 可能成环**（并发合并被打断），`resolve_canonical_ids` 带环检测，停在当前记录而非死循环。
- **`Actor` 的受护栏字段比插件白名单多 5 个**：`field_owners` / `mutation_revision` / `display_name_override` / `profile_image_override` / `merged_into` 受护栏约束但不可被插件写。
- **`gender` 只接受 1 和 2**，0 表示未知；`ACTOR_FIELD_ALLOWED_VALUES` 明确限定。

这两张表是认证状态的唯一持久化位置。确认的细节：

- `status` 列存**字符串**（`active` / `revoked` / `expired`），不是数字。改成整数枚举会让既有数据无法反序列化。
- 未知状态**不降级为 `active`**。`from_str_lossy` 返回 `Option`，遇 `None` 必须拒绝 —— 降级会让已失效令牌被当成有效令牌，这是认证绕过。
- 刷新令牌是**轮换**模型：`replaced_by_token_id` 指向接替者，`revoked_at` 记录吊销时刻。
- `client_ip` / `user_agent` 必须保留（审计留痕），不能因为「日志里也有」就省掉。
- `password_hash` / `token_hash` **永不返回给客户端**。

Peewee 模型里有一批**行为**不在表结构中，重写时不能丢：

| 行为 | 位置 | 说明 |
|---|---|---|
| 受保护字段护栏 | `Movie.save/update` | 已持久化行必须显式窄更新 |
| 系列名归一 | `MovieSeries.save` | 统一 strip 防重复实体 |
| 番号归一 | `Movie.save` | 只 strip，不改写 |
| 番号匹配 | `movie_number_match_expression` | `UPPER()` 等值匹配 + 函数索引 |
| 排序索引 | `movie_release_date_sort` 等 | `DESC NULLS LAST` 与排序表达式同向 |

这些属于 service 层职责，已在 `sm-db` 的类型注释中标注，实现时逐条落地。







---

## Schema 一致性对拍

`parity/schema_contract.py` 从 Peewee 源码用 `ast` 提取 40 张表的契约，
`parity/compare_schema.py` 再与 Rust 结构体逐字段比对列名、规范化类型与可空性。

**为什么需要它**：本机 Docker engine 未运行，无法连真实 PostgreSQL 验证。
静态对拍是在「不连库」前提下发现映射漂移的唯一手段。

**首次运行抓到 9 处真实缺陷**：

| 表 | 列 | 问题 |
|---|---|---|
| `background_task_run` | `progress_current` / `progress_total` | int4 映射成 i64 |
| `download_submission_record` | `client_id` / `task_id` | int4 映射成 i64 |
| `image_search_index_state` | `id` | int4 映射成 i64 |
| `image_search_session` | `page_size` | int4 映射成 i64 |
| `media_point` | `video_item_id` | int4 映射成 i64 |
| `system_notification` | `resource_id` / `related_resource_id` | int4 映射成 i64 |

这些列全持小整数（`page_size=20`、单例 `id=1`、计数与资源 id），用 i64 读在
PostgreSQL 上是安全的 widening，不会出错——但**契约就是契约**，放宽的类型会让
未来的 schema 变更无声积累。已全部收敛为 i32。

### 工具自身的三个修正

首版报 30 处差异，其中 21 处是工具缺陷：

1. **`column_name` 只对外键生效** —— `DownloadTask.movie` 是
   `CharField(..., column_name="movie_number")`，列名与字段名不同。只在 FK 上读
   column_name 会误报「多一列少一列」。
2. **`text/json` 未与 `text` 归一** —— `JsonTextField` 落的是 TEXT 列（内容为
   JSON 文本），Rust 的 `Option<String>` 是正确映射。不归一会让 13 处
   JsonTextField 全部误报。
3. **宏生成的类型解析不到** —— 三张合集表由 `macro_rules!` 生成，静态解析器
   看不到字段。已在 `collections::columns` 显式导出 `COLUMNS` 与
   `TABLE_NAME`，使其进入检查范围。

### Jsonb 与 JsonText 必须区分

- `JsonbField`（真 JSONB 列）：`Movie.metadata_source`、`Movie.field_owners`、
  `Actor.field_owners` → 映射 `serde_json::Value`。
- `JsonTextField`（TEXT 列装 JSON 文本）→ 映射 `Option<String>`。

映射阶段已正确区分，对拍工具也按此归类。

**当前结果：40/40 张表通过。**

## 真实 PostgreSQL 验证（L2）

DDL 生成正确不等于仓储正确。`crates/sm-db/tests/` 下的集成测试在真实
PG 16 上跑，这是唯一能发现「契约自洽但数据库拒绝」这类缺陷的层次。

### 怎么跑

```text
# 本地 PG 16（原生即可，不依赖容器）
initdb -D data -U sakuramedia --auth-local=trust --auth-host=trust -E UTF8
# postgresql.conf: port = 5433 / timezone = 'UTC'

python parity/gen_ddl.py --out docker/schema.sql
psql -h localhost -p 5433 -U sakuramedia -d sakuramedia_test -v ON_ERROR_STOP=1 -f schema.sql

$env:SMDB_TEST_DATABASE_URL = "postgres://sakuramedia:sakuramedia@localhost:5433/sakuramedia_test"
cargo test -p sm-db --test repo_integration --test gateway_integration
```

无 `DATABASE_URL` 时测试**跳过而非失败**，所以没起库的环境 `cargo test`
仍然是全绿。

### 只有真实数据库能发现的六类缺陷

全部由集成测试首次执行时暴露，静态检查（对拍 + clippy + 单测）一个都看不到。

| # | 缺陷 | 症状 | 修复 |
|---|---|---|---|
| 1 | 保留字列名未加引号 | `media_thumbnail.offset` 建表语法错误 | `RESERVED_WORDS` + `quote_ident()` |
| 2 | 外键指向源列名 | `REFERENCES media_library (library_id)` —— 该列不存在 | 从目标表主键解析 |
| 3 | `field=Model.attr` 解析失败 | 外键退化成 `movie(id)`，类型不匹配 | 认 `ast.Attribute` |
| 4 | `default=dict` 丢失 | 列变 `NOT NULL` 且无默认值 | 裸名字映射为空 JSON 值 |
| 5 | `NOT NULL DEFAULT` 列发 NULL | 23502 not_null_violation | Rust 侧填默认值 |
| 6 | 空 `UpdateSet` 绕过检查 | 报 NotFound（真实原因是「没东西可改」） | 检查移到 `touch()` 之前 |

第 2 条最隐蔽：只有 `media.movie_number` 碰巧对（目标字段恰好同名），
把 bug 掩盖住了。第 4 条影响上游 5 列。

### 两条不能重试的弯路

**① 在 SQL 里写 `DEFAULT` 关键字让数据库填默认值**

看起来更「正确」，但 sqlx 的 `bind` 按位置追加、**跳不过 `DEFAULT` 那一项**，
占位符编号随即错位，`$5` 类型无法推断（42P18）。最终选择 Rust 侧填默认值，
并在此记录以免重试。

**② `UPDATE ... RETURNING *`**

0 行命中与解码失败在那里都表现为同一个 `Err`，排查时无法区分。三个
update 路径改为「先 UPDATE 查 `rows_affected`，再单独 SELECT」，命中判定
变成一个确定的数字。

## 仓储层约定

### 字段主权网关（`repo/gateway.rs`）

受保护字段的**唯一**写入口，四个方法语义各不相同，且都必须是**单条原子
UPDATE** —— 拆成「先 SELECT 判断再 UPDATE」会丢原子性：

| 方法 | 谁能写 | 原子性靠什么 | 返回 |
|---|---|---|---|
| `patch_plugin` | 插件 | `mutation_revision` CAS + 字段级 owner 条件 | 是否命中 |
| `update_host_unowned` | 宿主 | 字段级 `CASE` 放 SET 内 | 受影响行数 |
| `update_host_manual` | 人工 | 批量 `IN`，无条件覆盖 | 受影响行数 |
| `release_plugin_owners` | 管理员 | `jsonb_each_text` 重建映射 | 受影响行数 |

三个容易写错的细节：

- **owner 条件必须放 `SET` 的 `CASE` 里**。放 `WHERE` 里的话，一个字段被
  接管就会让整条跳过，其他未接管字段也写不进去。
- **NULL-safe 变化检测**（`IS DISTINCT FROM`）—— 值没变就不递增
  `mutation_revision`、不刷新 `updated_at`。
- **不能用连续减法摘 owner**。上游记录了这个陷阱：`jsonb - NULL` 左结合
  会把整条结果污染成 NULL，必须逐 key 过滤重建。

`MOVIE_FIELD_CODECS` 里没有 codec 的受保护字段会被**拒绝**而非默认放行 ——
上游约定是「补了类型校验才加入白名单」，默认放行等于让字段绕过校验落库。

### 占位符编号约定

**字段从 `$1` 起，`WHERE` 条件放最后。** 与 SQL 书写顺序一致，读者不需要
在脑子里做逆序映射。三个 update 路径早期都写成「`id=$1`、字段从 `$2` 起」，
这个反直觉设计是编号错位的温床。

