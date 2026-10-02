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
| `collections` | 6 | 0 | 待做 |
| `discovery` | 5 | 0 | 待做 |
| `playback` | 6 | 6 | **完成** |
| `videos` | 3 | 3 | **完成** |
| `collections` | 6 | 6 | **完成** |
| `system` | 5 | 2 | 进行中（`User` / `UserRefreshToken` 已映射） |
| `transfers` | 6 | 0 | 待做 |
| `videos` | 3 | 0 | 待做 |
| **合计** | **40** | **26** | **65%** |

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




