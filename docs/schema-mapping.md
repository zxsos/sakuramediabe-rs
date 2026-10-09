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
| `catalog` | 9 | 1 | 进行中（`Movie` / `MovieSeries` 已映射） |
| `collections` | 6 | 0 | 待做 |
| `discovery` | 5 | 0 | 待做 |
| `playback` | 6 | 0 | 待做 |
| `system` | 5 | 2 | 进行中（`User` / `UserRefreshToken` 已映射） |
| `transfers` | 6 | 0 | 待做 |
| `videos` | 3 | 0 | 待做 |
| **合计** | **40** | **4** | **10%** |

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

