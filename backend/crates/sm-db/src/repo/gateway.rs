//! 字段主权网关（v2-lite）——受保护字段的**唯一**写入口。
//!
//! 两个实体，两套规则，**共用值类型**（[`FieldPatch`] / [`FieldValue`] /
//! [`FieldCodec`]）：
//!
//! | | [`MovieOwnershipGateway`] | [`ActorOwnershipGateway`] |
//! |---|---|---|
//! | 上游 | `service/catalog/movie_ownership_gateway.py` | `service/catalog/actor_ownership_gateway.py` |
//! | 白名单 | `PROTECTED_MOVIE_FIELDS`（6） | `PROTECTED_ACTOR_FIELDS`（9） |
//! | owner | `plugin:{id}` / `host:manual` | 再加 `host:javdb` |
//! | 插件 patch 收 `None` | 否 | 是（除 `gender`）—— 显式清空并保留归属 |
//! | 额外取值校验 | 无 | 正整数、文本 1..255、`gender ∈ {1,2}` |
//! | 释放归属推进 revision | 否 | **是** |
//!
//! 两者都在**仓储层**而不是 service 层：四条入口全是「单条条件 UPDATE +
//! jsonb + 乐观锁」，属仓储的活；service 侧的调用方收一个网关字段即可
//! （见 [`crate::repo::movie::MovieRepository`] 的用法）。
//!
//! ⚠️ `sm-service` 里曾各有一份**同名骨架**（`catalog/movie_ownership_gateway.rs`
//! 与 `catalog/actor_ownership_gateway.rs`），带各自杜撰的白名单 —— 影片那份
//! 只有 2 个字段，与上游的 6 个不符。两份都已删除，**唯一实现在这里**。
//!
//! # 为什么不能用通用 update
//!
//! 四个入口的语义各不相同，而且**都必须是单条原子 UPDATE**：
//!
//! | 方法 | 谁能写 | 原子性靠什么 |
//! |---|---|---|
//! | [`MovieOwnershipGateway::patch_plugin`] | 插件 | `mutation_revision` CAS + 字段级 owner 条件 |
//! | [`MovieOwnershipGateway::update_host_unowned`] | 宿主 | 字段级 `CASE`，不靠行级跳过 |
//! | [`MovieOwnershipGateway::update_host_manual`] | 人工 | 批量 `IN`，无条件覆盖 |
//! | [`MovieOwnershipGateway::release_plugin_owners`] | 管理员 | `jsonb_each_text` 重建映射 |
//!
//! **拆成「先 SELECT 判断、再 UPDATE」就会丢原子性** —— 两个并发写之间会
//! 出现 TOCTOU 窗口。上游注释写得很直接：
//!
//! > 全部走单条条件 UPDATE，靠 PostgreSQL 行级锁 + 条件求值
//! > （READ COMMITTED 下并发行更新会在 EvalPlanQual 阶段重新评估 WHERE）
//! > 保证原子性
//!
//! 所以这里刻意**不用** [`crate::common::update::UpdateSet`]：它的语义是
//! 「无条件覆盖」，而这三个写入口都需要 `WHERE` 里带条件、且返回值是
//! 「是否命中」而非行数据。
//!
//! # 字段主权模型
//!
//! `field_owners`（JSONB）记录每个受保护字段归谁：
//!
//! ```text
//! 缺键        -> 宿主自动管理，任何来源都能写
//! "plugin:x"  -> 插件 x 已接管，宿主自动写路径要跳过
//! "host:manual" -> 人工改过，自动规则不再覆盖
//! ```
//!
//! `mutation_revision` 是**版本号而非锁** —— 注意上游注释：
//! 「注意它不是整行的全局版本」。

use serde_json::Value as Json;
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::{PgPool, Postgres};

use crate::catalog::movie::field_owner;
use crate::error::DbError;

use super::movie::safe_sql;

/// 实体名。
const ENTITY: &str = "Movie";

/// 受保护字段的期望值类型。
///
/// 对应 Python 的 `MOVIE_FIELD_CODECS`。**必须在开放某字段的写入前补上**
/// —— 上游注释写明「真实插件提出、补 MOVIE_FIELD_CODECS 类型校验、
/// 并收敛对应宿主写点后才加入」白名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldCodec {
    /// `text` / `varchar` 列。
    Text,
    /// `bool` 列。
    Bool,
    /// `integer` 列（演员的身高/三围等）。
    Int,
    /// `date` 列（演员生日）。
    Date,
}

impl FieldCodec {
    /// 该 codec 是否接受 `None`。
    ///
    /// 只有**文本**列放行 `None`：`maker_name` / `director_name` / `summary`
    /// 允许 NULL，远端详情缺失时以 NULL 落库是合法数据 —— 而 `is_blacklisted`
    /// 这种布尔列没有「未知」态，`mutation_revision` 之类更不该被写成 NULL。
    ///
    /// ⚠️ 这只约束**影片**那条路径（它的插件 patch 不收 `None`）。演员路径
    /// 的规则不同：只有 `gender` 拒绝 `None`（它必须在 `{1,2}` 里），其余字段
    /// 允许显式清空 —— 见 [`ActorOwnershipGateway::patch_plugin`]。
    pub fn accepts_none(&self) -> bool {
        matches!(self, Self::Text)
    }
}

/// 字段名 -> 期望 codec。
///
/// 覆盖 [`crate::catalog::movie::PROTECTED_MOVIE_FIELDS`] 的全部 6 个字段。
/// 任何不在此表里的受保护字段都会被 `validate_fields`
/// 拒绝 —— 这是有意的：类型校验必须显式声明，不能默认放行。
pub const MOVIE_FIELD_CODECS: [(&str, FieldCodec); 6] = [
    ("title", FieldCodec::Text),
    ("summary", FieldCodec::Text),
    ("maker_name", FieldCodec::Text),
    ("director_name", FieldCodec::Text),
    ("is_collection", FieldCodec::Bool),
    ("is_blacklisted", FieldCodec::Bool),
];

/// 查某字段的 codec。
pub fn codec_of(field: &str) -> Option<FieldCodec> {
    MOVIE_FIELD_CODECS
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, codec)| *codec)
}

/// 受保护字段写入的值。
///
/// 用枚举而非 `serde_json::Value`，让「字段类型与 codec 不匹配」在
/// **构造时**就成为可能，而校验放在 `validate_fields`（模块内私有）。
#[derive(Debug, Clone)]
pub enum FieldValue {
    /// 文本（含 `None`，见 [`FieldCodec::accepts_none`]）。
    Text(Option<String>),
    /// 布尔（不接受 `None`）。
    Bool(bool),
    /// 整数（含 `None`：演员的身高可以清空）。
    Int(Option<i32>),
    /// 日期（含 `None`）。
    Date(Option<chrono::NaiveDate>),
}

impl FieldValue {
    /// 该值是否与 codec 匹配。
    fn matches(&self, codec: FieldCodec) -> bool {
        matches!(
            (self, codec),
            (Self::Text(_), FieldCodec::Text)
                | (Self::Bool(_), FieldCodec::Bool)
                | (Self::Int(_), FieldCodec::Int)
                | (Self::Date(_), FieldCodec::Date)
        )
    }

    /// 绑到查询上的 SQL 类型。
    ///
    /// `Option::<T>::None` 绑成 NULL —— 每个变体都要显式走一次 `Option`，
    /// 否则 sqlx 会按 `T`（非空）推断出 `NOT NULL` 的类型，写 NULL 时直接报
    /// 类型错误。
    fn bind<'q>(self, query: Query<'q, Postgres, PgArguments>) -> Query<'q, Postgres, PgArguments> {
        match self {
            Self::Text(Some(v)) => query.bind(v),
            Self::Text(None) => query.bind(Option::<String>::None),
            Self::Bool(v) => query.bind(v),
            Self::Int(Some(v)) => query.bind(v),
            Self::Int(None) => query.bind(Option::<i32>::None),
            Self::Date(Some(v)) => query.bind(v),
            Self::Date(None) => query.bind(Option::<chrono::NaiveDate>::None),
        }
    }
}

/// 一次受保护字段写入的请求。
///
/// 字段名固定来自 [`crate::catalog::movie::PROTECTED_MOVIE_FIELDS`]，
/// **不接受任意字符串** —— 列名是代码里的字面量，没有任何路径能把外部输入
/// 拼进 SQL。
#[derive(Debug, Clone, Default)]
pub struct FieldPatch {
    fields: Vec<(&'static str, FieldValue)>,
}

impl FieldPatch {
    /// 空 patch。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设一个文本字段。
    pub fn text(&mut self, field: &'static str, value: Option<&str>) -> &mut Self {
        self.push(field, FieldValue::Text(value.map(str::to_owned)));
        self
    }

    /// 设一个布尔字段。
    pub fn flag(&mut self, field: &'static str, value: bool) -> &mut Self {
        self.push(field, FieldValue::Bool(value));
        self
    }

    /// 设一个整数字段（演员的身高/三围等）。
    pub fn int(&mut self, field: &'static str, value: Option<i32>) -> &mut Self {
        self.push(field, FieldValue::Int(value));
        self
    }

    /// 设一个日期字段（演员生日）。
    ///
    /// 接的是**已解析**的日期 —— 从插件 JSON 来的字符串要先过
    /// [`parse_iso_date_exact`]（它带上游那条「必须严格 `YYYY-MM-DD`」的校验）。
    /// 直接 `NaiveDate::parse_from_str` 会把 `2020-1-1` 也收下，而上游拒绝它。
    pub fn date(&mut self, field: &'static str, value: Option<chrono::NaiveDate>) -> &mut Self {
        self.push(field, FieldValue::Date(value));
        self
    }

    fn push(&mut self, field: &'static str, value: FieldValue) -> &mut Self {
        match self.fields.iter_mut().find(|(name, _)| *name == field) {
            Some(slot) => slot.1 = value,
            None => self.fields.push((field, value)),
        }
        self
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// 字段数。
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// 字段名列表。
    pub fn names(&self) -> Vec<&'static str> {
        self.fields.iter().map(|(name, _)| *name).collect()
    }

    /// 取某字段的值。
    pub fn get(&self, field: &str) -> Option<&FieldValue> {
        self.fields
            .iter()
            .find(|(name, _)| *name == field)
            .map(|(_, v)| v)
    }

    /// 借出字段与值，供 SQL 构造。
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &FieldValue)> {
        self.fields.iter().map(|(name, value)| (*name, value))
    }
}

/// 校验 patch：非空、字段受保护、类型匹配 codec。
fn validate_fields(patch: &FieldPatch, allow_none: bool) -> Result<(), DbError> {
    if patch.is_empty() {
        return Err(DbError::business(ENTITY, "fields 不能为空"));
    }

    for (name, value) in patch.iter() {
        if !crate::catalog::movie::PROTECTED_MOVIE_FIELDS.contains(&name) {
            return Err(DbError::business(ENTITY, format!("非受保护字段: {name}")));
        }
        let Some(codec) = codec_of(name) else {
            // 进了白名单但没补 codec —— 拒绝，不能默认放行。
            return Err(DbError::business(
                ENTITY,
                format!("字段 {name} 未声明 codec，按约定必须先补类型校验"),
            ));
        };
        if !value.matches(codec) {
            return Err(DbError::business(ENTITY, format!("字段 {name} 值类型错误")));
        }
        if !allow_none && matches!(value, FieldValue::Text(None)) && codec.accepts_none() {
            return Err(DbError::business(
                ENTITY,
                format!("字段 {name} 不接受 NULL（插件路径）"),
            ));
        }
    }
    Ok(())
}

/// `Movie` 字段主权网关。
#[derive(Debug, Clone)]
pub struct MovieOwnershipGateway {
    pool: PgPool,
}

impl MovieOwnershipGateway {
    /// 构造网关。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// **插件写字段**：每个字段要求「未接管或 owner 是当前插件」，且 revision 匹配。
    ///
    /// 返回 `true` 表示命中。**任一字段条件失败则整次零修改** —— 插件应
    /// 重新读取 snapshot 后决定是否重试。成功时字段值、owner、revision、
    /// `updated_at` 在同一语句中提交。
    ///
    /// 屏蔽已订阅影片返回 `false`（`is_blacklisted = true` 时追加
    /// `AND NOT is_subscribed` 条件）。
    pub async fn patch_plugin(
        &self,
        movie_id: i32,
        plugin_id: &str,
        patch: &FieldPatch,
        expected_revision: i64,
    ) -> Result<bool, DbError> {
        validate_fields(patch, false)?;
        let owner = field_owner::plugin(plugin_id);

        // 占位符编号：字段值 1..n → owner_payload → movie_id → revision
        // → 每字段 3 个 owner 条件。顺序即绑定顺序，不能调换。
        let mut q = 0usize;
        let assignments = patch
            .names()
            .iter()
            .map(|name| {
                q += 1;
                format!("{name} = ${}", q)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let owner_payload_idx = {
            q += 1;
            q
        };
        let movie_idx = {
            q += 1;
            q
        };
        let rev_idx = {
            q += 1;
            q
        };
        // 每个字段两个条件（IS NULL / = owner），共 3 个占位：key,key,owner
        let owner_conditions = patch
            .names()
            .iter()
            .map(|_| {
                let key = {
                    q += 1;
                    q
                };
                let key2 = {
                    q += 1;
                    q
                };
                let own = {
                    q += 1;
                    q
                };
                format!("(field_owners->>${key} IS NULL OR field_owners->>${key2} = ${own})")
            })
            .collect::<Vec<_>>()
            .join(" AND ");

        // 屏蔽已订阅影片：is_blacklisted = true 时追加条件。
        let blacklisting = patch
            .get("is_blacklisted")
            .is_some_and(|v| matches!(v, FieldValue::Bool(true)));
        let subscription_condition = if blacklisting {
            "AND NOT is_subscribed"
        } else {
            ""
        };

        let sql = format!(
            "UPDATE movie SET {assignments}, \
                field_owners = field_owners || ${owner_payload_idx}::jsonb, \
                mutation_revision = mutation_revision + 1, \
                updated_at = now() \
             WHERE id = ${movie_idx} \
               AND mutation_revision = ${rev_idx} \
               AND {owner_conditions} \
               {subscription_condition}"
        );

        let owner_payload =
            owner_map_json(patch.names().iter().copied().map(|nm| (nm, owner.as_str())));
        let query = bind_patch_values(sqlx::query(safe_sql(sql)), patch)
            .bind(&owner_payload)
            .bind(movie_id)
            .bind(expected_revision);
        // 尾部每字段 3 个绑定：key, key, owner
        let query = patch.names().iter().fold(query, |query, name| {
            query.bind(*name).bind(*name).bind(owner.clone())
        });

        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// **宿主只更新尚未被接管的受保护字段。**
    ///
    /// 每个字段仅在当前没有 owner 时改写（owner 条件放 `SET` 的 `CASE` 内，
    /// **避免一个字段被接管导致整条跳过**）；NULL-safe（`IS DISTINCT FROM`）
    /// 变化检测，任一字段「未接管且值真正变化」才递增 revision 并刷新
    /// `updated_at`。返回受影响行数。
    pub async fn update_host_unowned(
        &self,
        movie_id: i32,
        patch: &FieldPatch,
    ) -> Result<u64, DbError> {
        validate_fields(patch, true)?;
        let names = patch.names();
        let n = patch.len();

        // 三个 WHERE/SET 子句各需一遍字段参数：
        //   1) SET 的 CASE          (name, value)
        //   2) revision 的 CASE     (name, value)
        //   3) updated_at 的判定    (name, value)
        // 最后是 movie_id。
        let mut q = 0usize;
        let set_case = names
            .iter()
            .map(|name| {
                let a = {
                    q += 1;
                    q
                };
                let b = {
                    q += 1;
                    q
                };
                format!("{name} = CASE WHEN field_owners->>${a} IS NULL THEN ${b} ELSE {name} END")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let revision_terms = names
            .iter()
            .map(|name| {
                let a = {
                    q += 1;
                    q
                };
                let b = {
                    q += 1;
                    q
                };
                format!(
                    "(CASE WHEN field_owners->>${a} IS NULL AND {name} IS DISTINCT FROM ${b} THEN 1 ELSE 0 END)"
                )
            })
            .collect::<Vec<_>>()
            .join(" + ");
        let changed_terms = names
            .iter()
            .map(|name| {
                let a = {
                    q += 1;
                    q
                };
                let b = {
                    q += 1;
                    q
                };
                format!("(field_owners->>${a} IS NULL AND {name} IS DISTINCT FROM ${b})")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let movie_idx = {
            q += 1;
            q
        };
        debug_assert_eq!(q, n * 6 + 1, "占位符总数应为 6n+1");

        let sql = format!(
            "UPDATE movie SET {set_case}, \
                mutation_revision = mutation_revision + ({revision_terms}), \
                updated_at = CASE WHEN {changed_terms} THEN now() ELSE updated_at END \
             WHERE id = ${movie_idx}"
        );

        let query = bind_patch_triples(sqlx::query(safe_sql(sql)), patch);
        let result = query.bind(movie_id).execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    /// **人工写入**受保护字段，并把这些字段标记为 `host:manual` owner。
    ///
    /// 这是宿主侧的特权写入口：人工操作**可以覆盖**插件 owner。后续自动规则
    /// 会尊重 `host:manual`，但不提供自动恢复或回退语义。
    ///
    /// 批量按 id；`movie_ids` 为空时返回 0（不发 SQL）。
    pub async fn update_host_manual(
        &self,
        movie_ids: &[i32],
        patch: &FieldPatch,
    ) -> Result<u64, DbError> {
        validate_fields(patch, true)?;
        if movie_ids.is_empty() {
            return Ok(0);
        }

        let n = patch.len();
        let owner_payload_idx = n + 1;
        let id_start = n + 2;

        let assignments = patch
            .names()
            .iter()
            .enumerate()
            .map(|(i, name)| format!("{name} = ${}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let id_placeholders = (0..movie_ids.len())
            .map(|i| format!("${}", id_start + i))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "UPDATE movie SET {assignments}, \
                field_owners = field_owners || ${owner_payload_idx}::jsonb, \
                mutation_revision = mutation_revision + 1, \
                updated_at = now() \
             WHERE id IN ({id_placeholders})"
        );

        let owner_payload = owner_map_json(
            patch
                .names()
                .iter()
                .copied()
                .map(|nm| (nm, field_owner::HOST_MANUAL)),
        );
        let query = bind_patch_values(sqlx::query(safe_sql(sql)), patch).bind(&owner_payload);
        let mut query = query;
        for id in movie_ids {
            query = query.bind(*id);
        }
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    /// **管理员解除插件对字段的接管**（清理端点，CLI 调用）。
    ///
    /// `fields` 为 `None` 时清除该插件全部 owner 记录；指定时仅清除列出的
    /// 字段（必须属于白名单且非空）。**只动 `field_owners` 映射，不改字段值
    /// 与 revision。** 返回受影响行数。
    ///
    /// 按 owner 过滤重建映射，只摘除属于目标插件的 key —— 用
    /// `jsonb_each_text` 逐 key 判断。上游注释记录了一个陷阱：
    ///
    /// > 不会误摘其他插件接管的字段（不能用连续减法：`jsonb - NULL`
    /// > 左结合会把整条结果污染成 NULL）
    pub async fn release_plugin_owners(
        &self,
        plugin_id: &str,
        fields: Option<&[&'static str]>,
    ) -> Result<u64, DbError> {
        let owner = field_owner::plugin(plugin_id);

        let Some(fields) = fields else {
            // 清除该插件全部 owner：按 owner 值过滤重建映射。
            let result = sqlx::query(safe_sql(
                "UPDATE movie SET field_owners = COALESCE( \
                    (SELECT jsonb_object_agg(key, value) FROM jsonb_each_text(field_owners) \
                     WHERE value <> $1), \
                    '{}'::jsonb \
                 ) \
                 WHERE EXISTS ( \
                    SELECT 1 FROM jsonb_each_text(field_owners) WHERE value = $2 \
                 )",
            ))
            .bind(&owner)
            .bind(&owner)
            .execute(&self.pool)
            .await?;
            return Ok(result.rows_affected());
        };

        if fields.is_empty() {
            return Err(DbError::business(ENTITY, "fields 不能为空"));
        }
        for name in fields {
            if !crate::catalog::movie::PROTECTED_MOVIE_FIELDS.contains(name) {
                return Err(DbError::business(ENTITY, format!("非受保护字段: {name}")));
            }
        }

        // 字段级独立摘除：任一目标字段属于该插件即整行更新。
        let key_placeholders = (0..fields.len())
            .map(|i| format!("${}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        // 每个字段一个条件：`field_owners->>$key = $owner`。
        // key 占位从 $2 起（$1 是 owner 本身），owner 占位在所有 key 之后。
        let conditions = (0..fields.len())
            .map(|i| {
                let owner_idx = fields.len() + 2 + i;
                format!("field_owners->>${} = ${owner_idx}", i + 2)
            })
            .collect::<Vec<_>>()
            .join(" OR ");

        let sql = format!(
            "UPDATE movie SET field_owners = ( \
                SELECT COALESCE(jsonb_object_agg(k.key, k.value), '{{}}'::jsonb) \
                FROM jsonb_each_text(field_owners) AS k \
                WHERE NOT (k.value = $1 AND k.key IN ({key_placeholders})) \
             ) \
             WHERE {conditions}"
        );

        let mut query = sqlx::query(safe_sql(sql)).bind(&owner);
        for name in fields {
            query = query.bind(*name);
        }
        for _ in fields {
            query = query.bind(&owner);
        }
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }
}

/// 构造 `{field: owner}` 的 JSONB 文本。
fn owner_map_json<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut map = serde_json::Map::new();
    for (field, owner) in pairs {
        map.insert(field.to_owned(), Json::String((*owner).to_owned()));
    }
    Json::Object(map).to_string()
}

/// 绑定一轮字段值（`SET col = $N` 形态）。
fn bind_patch_values<'q>(
    query: Query<'q, Postgres, PgArguments>,
    patch: &FieldPatch,
) -> Query<'q, Postgres, PgArguments> {
    let mut acc = query;
    for (_, value) in patch.iter() {
        acc = value.clone().bind(acc);
    }
    acc
}

/// 绑定三轮 `(key, value)`，对应 SQL 里 `SET` / `revision` / `updated_at`
/// 三段子句各占一轮。
///
/// **每轮必须绑两个值**：`(key, value)` 在 SQL 里是两个独立占位符
/// （`field_owners->>$1 IS NULL THEN $2`），少绑一个就会得到
/// 「supplies 7 parameters, but prepared statement requires 13」。
/// 这条只在真实数据库上暴露过 —— 单元测试不构造查询。
///
/// 顺序必须与 [`MovieOwnershipGateway::update_host_unowned`] 里 `q`
/// 计数器的分配顺序一致：每轮先全部 key，再全部 value。
fn bind_patch_triples<'q>(
    query: Query<'q, Postgres, PgArguments>,
    patch: &FieldPatch,
) -> Query<'q, Postgres, PgArguments> {
    let mut acc = query;
    for _ in 0..3 {
        for (name, _) in patch.iter() {
            acc = acc.bind(name);
        }
        for (_, value) in patch.iter() {
            acc = value.clone().bind(acc);
        }
    }
    acc
}

// ============================================================ 演员

/// 实体名（错误信息里的 `entity` 字段）。actor 与 movie 的错误要能分开。
const ACTOR_ENTITY: &str = "Actor";

/// JavDB 补录的 owner 标记（上游 `JAVDB_ACTOR_FIELD_OWNER`）。
///
/// 存在 `field_owners` 里的**稳定字符串**，跨版本要能识别 —— 改它等于让存量库
/// 上已接管的字段全部变成「别人的」。
///
/// 人工那条用 [`field_owner::HOST_MANUAL`]（上游两个模块各定义一个，值相同）。
pub const HOST_JAVDB_OWNER: &str = "host:javdb";

/// 演员受保护字段的期望值类型。对应 Python 的 `ACTOR_FIELD_CODECS`。
///
/// 覆盖 [`crate::catalog::actor::PROTECTED_ACTOR_FIELDS`] 的**全部 9 个**字段。
/// 白名单与这张表必须一一对应 —— 进了白名单却没有 codec 的字段会被
/// `validate_actor_fields` 拒绝（类型校验不能默认放行）。
pub const ACTOR_FIELD_CODECS: [(&str, FieldCodec); 9] = [
    ("gender", FieldCodec::Int),
    ("birthday", FieldCodec::Date),
    ("height_cm", FieldCodec::Int),
    ("bust_cm", FieldCodec::Int),
    ("waist_cm", FieldCodec::Int),
    ("hips_cm", FieldCodec::Int),
    ("cup", FieldCodec::Text),
    ("birthplace", FieldCodec::Text),
    ("blood_type", FieldCodec::Text),
];

/// 查演员字段的 codec。
pub fn actor_codec_of(field: &str) -> Option<FieldCodec> {
    ACTOR_FIELD_CODECS
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, codec)| *codec)
}

/// `gender` 的取值域。上游 `ACTOR_FIELD_ALLOWED_VALUES = {"gender": {1, 2}}`。
///
/// 只有这一个字段有枚举值，所以没有做成表：多一层 `HashMap` 只是为了一个
/// 元素，读起来反而更绕。
pub const ACTOR_GENDER_VALUES: [i32; 2] = [1, 2];

/// 该字段是否只能取 [`ACTOR_GENDER_VALUES`] 里的值。
fn has_allowed_values(field: &str) -> bool {
    field == "gender"
}

/// 解析严格 ISO `YYYY-MM-DD` 日期（上游 `birthday` 的校验）。
///
/// # 为什么不能直接用 `NaiveDate::parse_from_str(_, "%Y-%m-%d")`
///
/// 那个格式串**不要求补零**：`2020-1-1` 也能解析成功。上游的判据是
/// 「`date.fromisoformat(value).isoformat() == value`」—— 即解析后再格式化
/// 必须**逐字符相同**，于是 `2020-1-1` 被拒。这里的等价做法是：解析成功后
/// 用 `to_string()`（固定 `YYYY-MM-DD`）比一次。
pub fn parse_iso_date_exact(raw: &str) -> Result<chrono::NaiveDate, DbError> {
    let parsed = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| DbError::business(ACTOR_ENTITY, "birthday 必须是 YYYY-MM-DD 日期"))?;
    if parsed.to_string() != raw {
        return Err(DbError::business(
            ACTOR_ENTITY,
            "birthday 必须是 YYYY-MM-DD 日期",
        ));
    }
    Ok(parsed)
}

/// 校验演员 patch：非空、字段在 9 个白名单里、类型匹配 codec + 取值域。
///
/// 与影片那条路径的**两处语义差异**（都来自上游）：
///
/// | | 影片 | 演员 |
/// |---|---|---|
/// | `None` | 插件路径拒绝 | **允许**（除 `gender` 外），"显式清空并保留归属" |
/// | 额外校验 | 无 | 整数必须为正、文本 1..255、`gender ∈ {1,2}` |
fn validate_actor_fields(patch: &FieldPatch) -> Result<(), DbError> {
    if patch.is_empty() {
        return Err(DbError::business(
            ACTOR_ENTITY,
            "fields 必须是非空的演员资料字段集合",
        ));
    }

    for (name, value) in patch.iter() {
        if !crate::catalog::actor::PROTECTED_ACTOR_FIELDS.contains(&name) {
            // 上游对演员侧是**直接报错**（影片侧是静默跳过）：演员字段少，
            // 传错更可能是代码写错，快速失败比静默丢字段好。
            return Err(DbError::business(
                ACTOR_ENTITY,
                format!("字段 {name} 不是演员资料字段"),
            ));
        }
        let Some(codec) = actor_codec_of(name) else {
            return Err(DbError::business(
                ACTOR_ENTITY,
                format!("字段 {name} 未声明 codec，按约定必须先补类型校验"),
            ));
        };
        if !value.matches(codec) {
            return Err(DbError::business(
                ACTOR_ENTITY,
                format!("字段 {name} 值类型错误: 期望 {codec:?}"),
            ));
        }

        match value {
            FieldValue::Int(None) | FieldValue::Text(None) | FieldValue::Date(None) => {
                // `gender` 没有「未知」态：它必须在 {1,2} 里。
                if has_allowed_values(name) {
                    return Err(DbError::business(
                        ACTOR_ENTITY,
                        format!("字段 {name} 值必须是 {:?}", ACTOR_GENDER_VALUES),
                    ));
                }
            }
            FieldValue::Int(Some(number)) => {
                if has_allowed_values(name) {
                    if !ACTOR_GENDER_VALUES.contains(number) {
                        return Err(DbError::business(
                            ACTOR_ENTITY,
                            format!("字段 {name} 值必须是 {:?}", ACTOR_GENDER_VALUES),
                        ));
                    }
                } else if !(1..=i32::MAX).contains(number) {
                    // 上界不是装饰：这些列是 `integer`，而「必须是正整数厘米值」
                    // 是上游写在字段级校验里的规则（0 与负数都非法）。
                    return Err(DbError::business(
                        ACTOR_ENTITY,
                        format!("字段 {name} 必须是正整数厘米值"),
                    ));
                }
            }
            FieldValue::Text(Some(text)) => {
                // 空白串拿 `strip` 判：上游要求「1 到 255 字符的非空文本；
                // 清空请传 None」—— 也就是说 `""` 与 `"   "` 都非法。
                if text.trim().is_empty() || text.chars().count() > 255 {
                    return Err(DbError::business(
                        ACTOR_ENTITY,
                        format!("字段 {name} 必须是 1 到 255 字符的非空文本；清空请传 None"),
                    ));
                }
            }
            FieldValue::Bool(_) => {
                // 演员白名单里没有布尔字段（`is_subscribed` 不可写）。
                return Err(DbError::business(
                    ACTOR_ENTITY,
                    format!("字段 {name} 不该是布尔值"),
                ));
            }
            FieldValue::Date(Some(_)) => {}
        }
    }
    Ok(())
}

/// `Actor` 字段主权网关。
///
/// 与 [`MovieOwnershipGateway`] 同构，但**多一个 owner**：
///
/// | owner | 谁写的 | 谁可以覆盖它 |
/// |---|---|---|
/// | `host:manual` | 人工（改名/换头像/订阅） | 只有人工 |
/// | `host:javdb` | JavDB 补录 | `host:*` 系列的自动来源 |
/// | `plugin:{id}` | 插件 | 宿主来源（`host:javdb`）与人工 |
/// | （无归属） | 谁都行 | —— |
///
/// **身份、头像、订阅不在插件可写白名单内**（上游 docstring 原话）。
/// 放开任何一项都等于把人工决策交给插件 —— `javdb_id` 能改就等于可以把演员
/// 挂到别的 JavDB 条目上。
#[derive(Debug, Clone)]
pub struct ActorOwnershipGateway {
    pool: PgPool,
}

impl ActorOwnershipGateway {
    /// 构造网关。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// **插件写字段**：每个字段要求「未接管或 owner 是当前插件」，且 revision 匹配。
    ///
    /// 返回 `true` 表示命中；**任一字段条件失败则整次零修改**，插件应重新读取
    /// snapshot 再决定是否重试。`None` 是**显式清空**，且保留归属。
    pub async fn patch_plugin(
        &self,
        actor_id: i32,
        plugin_id: &str,
        patch: &FieldPatch,
        expected_revision: i64,
    ) -> Result<bool, DbError> {
        validate_actor_fields(patch)?;
        if expected_revision < 0 {
            // 上游显式拒绝负数版本号（认为它只能是读出来的 snapshot 版本）。
            return Err(DbError::business(
                ACTOR_ENTITY,
                "expected_revision 必须是非负整数",
            ));
        }
        let owner = field_owner::plugin(plugin_id);
        let names = patch.names();

        // 占位符编号：字段值 1..n → owner_payload → actor_id → revision
        // → 每字段 3 个 owner 条件（key, key, owner）。顺序即绑定顺序。
        let mut q = 0usize;
        let assignments = names
            .iter()
            .map(|name| {
                q += 1;
                format!("{name} = ${q}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let owner_payload_idx = {
            q += 1;
            q
        };
        let actor_idx = {
            q += 1;
            q
        };
        let revision_idx = {
            q += 1;
            q
        };
        let owner_conditions = names
            .iter()
            .map(|_| {
                let key = {
                    q += 1;
                    q
                };
                let key2 = {
                    q += 1;
                    q
                };
                let own = {
                    q += 1;
                    q
                };
                format!("(field_owners->>${key} IS NULL OR field_owners->>${key2} = ${own})")
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        // n 个字段值 + owner 载荷 + id + revision + 每字段 3 个 owner 条件 = 4n+3。
        // （写成 3n+4 只在 n=1 时碰巧相等 —— 多字段补丁一跑就 panic。）
        debug_assert_eq!(q, patch.len() * 4 + 3, "占位符总数应为 4n+3");

        let sql = format!(
            "UPDATE actor SET {assignments}, \
                field_owners = field_owners || ${owner_payload_idx}::jsonb, \
                mutation_revision = mutation_revision + 1, \
                updated_at = now() \
             WHERE id = ${actor_idx} \
               AND mutation_revision = ${revision_idx} \
               AND {owner_conditions}"
        );

        let owner_payload = owner_map_json(names.iter().copied().map(|nm| (nm, owner.as_str())));
        let query = bind_patch_values(sqlx::query(safe_sql(sql)), patch)
            .bind(&owner_payload)
            .bind(actor_id)
            .bind(expected_revision);
        let query = names.iter().fold(query, |query, name| {
            query.bind(*name).bind(*name).bind(owner.clone())
        });

        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// **宿主权威来源写字段**（JavDB 补录；人工写入走同一个方法，见下）。
    ///
    /// 只影响**无归属**或**已被同一 owner 占有**的字段；可以替换插件的 owner，
    /// 但**不会覆盖人工 owner**（`host:manual`）。
    ///
    /// ⚠️ **`host:manual` 传不进来** —— 上游 `:89` 显式拒绝它
    /// （`owner == MANUAL_ACTOR_FIELD_OWNER` 直接抛）。也就是说**演员侧没有
    /// 「人工写入」入口**：`host:manual` 只会作为**别人摆在那儿的标记**被读到，
    /// 本方法只会因为它而**不命中**（见下）。
    ///
    /// 骨架期的文档曾写着「传 `MANUAL_ACTOR_FIELD_OWNER` 调它就等价于人工写入」
    /// —— 那是错的，照着写会在第一次调用时拿到一个 `owner 必须是...` 的业务错误。
    /// 演员的人工编辑走 `actor_merge_service` 那条链路（它读
    /// `MANUAL_ACTOR_FIELD_OWNER` 判断能否合并），不经过本方法。
    ///
    /// ⚠️ 也别想用它写 `is_subscribed`：订阅是人工决策，而它**不在** 9 个
    /// 受保护字段里，方法根本收不到它。
    ///
    /// 返回 `true` 表示命中；没有实际变化时返回 `false`（**不是错误** ——
    /// 上游把「值没变」也算不命中）。
    pub async fn update_host_source(
        &self,
        actor_id: i32,
        patch: &FieldPatch,
        owner: &str,
    ) -> Result<bool, DbError> {
        validate_actor_fields(patch)?;
        // 上游：owner 必须是非人工的宿主来源 —— 人工那条要显式传 MANUAL。
        if !owner.starts_with("host:") || owner == field_owner::HOST_MANUAL {
            return Err(DbError::business(
                ACTOR_ENTITY,
                "owner 必须是非人工的宿主来源 owner",
            ));
        }
        let names = patch.names();

        // 三段各占一轮字段参数（与影片的 `update_host_unowned` 同款）：
        //   1) SET 的值                            (value)
        //   2) 字段级 owner 条件                    (name, name, "host:manual")
        //   3) 变化检测（值或归属任一变了才算）      (value, name, owner)
        // 最后是 actor_id。
        let mut q = 0usize;
        let assignments = names
            .iter()
            .map(|name| {
                q += 1;
                format!("{name} = ${q}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let owner_payload_idx = {
            q += 1;
            q
        };
        let actor_idx = {
            q += 1;
            q
        };
        let owner_conditions = names
            .iter()
            .map(|_| {
                let key = {
                    q += 1;
                    q
                };
                let key2 = {
                    q += 1;
                    q
                };
                let manual = {
                    q += 1;
                    q
                };
                // `<> %s` 而不是 `IS DISTINCT FROM`：上游就是 `<>`，而 key 缺失时
                // 左边是 NULL、`<>` 结果为 NULL（不成立）—— 但前一个分支
                // （`IS NULL`）已经把这种情况接住了，两者合起来正是
                // 「无归属或不是人工owner」。
                format!("(field_owners->>${key} IS NULL OR field_owners->>${key2} <> ${manual})")
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let changed_conditions = names
            .iter()
            .map(|name| {
                let value = {
                    q += 1;
                    q
                };
                let key = {
                    q += 1;
                    q
                };
                let own = {
                    q += 1;
                    q
                };
                format!(
                    "({name} IS DISTINCT FROM ${value} \
                      OR field_owners->>${key} IS DISTINCT FROM ${own})"
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        debug_assert_eq!(q, patch.len() * 7 + 2, "占位符总数应为 7n+2");

        let sql = format!(
            "UPDATE actor SET {assignments}, \
                field_owners = field_owners || ${owner_payload_idx}::jsonb, \
                mutation_revision = mutation_revision + 1, \
                updated_at = now() \
             WHERE id = ${actor_idx} \
               AND {owner_conditions} \
               AND ({changed_conditions})"
        );

        let owner_payload = owner_map_json(names.iter().copied().map(|nm| (nm, owner)));
        let mut query = bind_patch_values(sqlx::query(safe_sql(sql)), patch)
            .bind(&owner_payload)
            .bind(actor_id);
        for name in &names {
            query = query.bind(*name).bind(*name).bind(field_owner::HOST_MANUAL);
        }
        for (name, value) in patch.iter() {
            query = value.clone().bind(query).bind(name).bind(owner);
        }

        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// **管理员解除插件对字段的接管**。`fields = None` 时清除该插件**全部**
    /// owner 记录。
    ///
    /// ⚠️ 与影片侧同款方法的一处**行为差异**（上游如此）：演员侧会
    /// **推进 `mutation_revision` 并刷新 `updated_at`**，影片侧不动它们。
    /// 理由是演员的 snapshot 带 revision，释放归属等于「你手里的那份快照
    /// 已经过期了」，所以要让版本往前跳一格。
    pub async fn release_plugin_owners(
        &self,
        plugin_id: &str,
        fields: Option<&[&'static str]>,
    ) -> Result<u64, DbError> {
        let owner = field_owner::plugin(plugin_id);

        // 两个分支共用的一句：按 owner（可选再按字段名）过滤重建映射。
        if let Some(fields) = fields {
            validate_release_fields(fields)?;
            // 参数分两轮，内层过滤与外层 `EXISTS` 各用一轮（上游是
            // `[*params, *params]`）：
            //   $1 = owner，$2..$1+n = 字段名（内层）
            //   $2+n = owner，$3+n..$2+2n = 字段名（EXISTS）
            // 两轮**不能共享**占位符 —— 共享会让 `EXISTS` 判的是「内层过滤前
            // 的映射」，而那正是被改掉的那份。
            let n = fields.len();
            let inner_keys = (2..=n + 1)
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let exists_owner_idx = n + 2;
            let exists_keys = (n + 3..=n * 2 + 2)
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "UPDATE actor SET field_owners = ( \
                    SELECT COALESCE(jsonb_object_agg(key, value), '{{}}'::jsonb) \
                    FROM jsonb_each_text(field_owners) \
                    WHERE NOT (value = $1 AND key IN ({inner_keys})) \
                 ), \
                 mutation_revision = mutation_revision + 1, \
                 updated_at = now() \
                 WHERE EXISTS ( \
                    SELECT 1 FROM jsonb_each_text(field_owners) \
                    WHERE value = ${exists_owner_idx} AND key IN ({exists_keys}) \
                 )"
            );

            let mut query = sqlx::query(safe_sql(sql)).bind(&owner);
            for name in fields {
                query = query.bind(*name);
            }
            query = query.bind(&owner);
            for name in fields {
                query = query.bind(*name);
            }
            let result = query.execute(&self.pool).await?;
            return Ok(result.rows_affected());
        }

        let result = sqlx::query(safe_sql(
            "UPDATE actor SET field_owners = COALESCE( \
                (SELECT jsonb_object_agg(key, value) FROM jsonb_each_text(field_owners) \
                 WHERE value <> $1), \
                '{}'::jsonb \
             ), \
             mutation_revision = mutation_revision + 1, \
             updated_at = now() \
             WHERE EXISTS ( \
                SELECT 1 FROM jsonb_each_text(field_owners) WHERE value = $2 \
             )",
        ))
        .bind(&owner)
        .bind(&owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// 校验「释放哪些字段」这一参数：非空且都属于演员白名单。
fn validate_release_fields(fields: &[&'static str]) -> Result<(), DbError> {
    if fields.is_empty() {
        return Err(DbError::business(
            ACTOR_ENTITY,
            "fields 必须是非空的演员资料字段集合",
        ));
    }
    for name in fields {
        if !crate::catalog::actor::PROTECTED_ACTOR_FIELDS.contains(name) {
            return Err(DbError::business(
                ACTOR_ENTITY,
                format!("字段 {name} 不是演员资料字段"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_patch() -> FieldPatch {
        let mut p = FieldPatch::new();
        p.text("title", Some("新标题"));
        p
    }

    #[test]
    fn codecs_cover_every_protected_field() {
        // 白名单非空后 codec 必须齐全 —— 缺一个，那个字段的插件写入口
        // 就无法做类型校验。
        for field in crate::catalog::movie::PROTECTED_MOVIE_FIELDS {
            assert!(codec_of(field).is_some(), "{field} 在白名单里但没有 codec");
        }
        assert_eq!(MOVIE_FIELD_CODECS.len(), 6);
        // 非受保护字段没有 codec
        assert!(codec_of("watched_count").is_none());
        assert!(codec_of("movie_number").is_none());
    }

    #[test]
    fn only_text_fields_accept_none() {
        assert!(FieldCodec::Text.accepts_none());
        assert!(!FieldCodec::Bool.accepts_none());
    }

    #[test]
    fn empty_patch_is_rejected() {
        let empty = FieldPatch::new();
        assert!(validate_fields(&empty, true).is_err());
        assert!(validate_fields(&empty, false).is_err());
    }

    #[test]
    fn codec_mismatch_is_rejected() {
        let mut p = FieldPatch::new();
        p.flag("title", true); // title 是 Text，塞 bool
        let err = validate_fields(&p, true).unwrap_err();
        assert!(err.to_string().contains("title"), "{err}");

        let mut p = FieldPatch::new();
        p.text("is_collection", Some("yes")); // is_collection 是 Bool
        assert!(validate_fields(&p, true).is_err());
    }

    #[test]
    fn plugin_path_forbids_null_but_host_path_allows_it() {
        let mut p = FieldPatch::new();
        p.text("maker_name", None);
        // 宿主写路径：maker_name 允许 NULL（远端详情缺失是合法数据）
        assert!(validate_fields(&p, true).is_ok());
        // 插件 patch 路径：不允许
        assert!(validate_fields(&p, false).is_err());
    }

    #[test]
    fn owner_key_format_matches_backend() {
        assert_eq!(
            field_owner::plugin("actor-metadata"),
            "plugin:actor-metadata"
        );
        let p = text_patch();
        let json = owner_map_json(p.names().iter().copied().map(|n| (n, "plugin:x")));
        assert_eq!(json, r#"{"title":"plugin:x"}"#);
    }

    #[test]
    fn owner_map_json_escapes_non_ascii_keys() {
        // ensure_ascii=False 的等价：中文键直接出现在 JSON 里
        let json = owner_map_json([("title", "host:manual")].into_iter());
        assert!(json.contains("host:manual"));
    }

    #[test]
    fn patch_deduplicates_fields() {
        let mut p = FieldPatch::new();
        p.text("title", Some("a"));
        p.text("title", Some("b"));
        assert_eq!(p.len(), 1);
        match p.get("title") {
            Some(FieldValue::Text(Some(v))) => assert_eq!(v, "b"),
            other => panic!("expected Text(Some(\"b\")), got {other:?}"),
        }
    }

    #[test]
    fn patch_tracks_both_text_and_flag_shapes() {
        let mut p = FieldPatch::new();
        p.text("title", Some("t"))
            .text("maker_name", None)
            .flag("is_blacklisted", true);
        assert_eq!(p.len(), 3);
        assert!(p.names().contains(&"is_blacklisted"));
        assert!(matches!(p.get("maker_name"), Some(FieldValue::Text(None))));
        assert!(matches!(
            p.get("is_blacklisted"),
            Some(FieldValue::Bool(true))
        ));
    }

    #[test]
    fn release_rejects_empty_and_unprotected_field_lists() {
        // 语义在 validate_fields 之外单独测：这里检查的是 fields 参数校验分支
        let unprotected: Vec<&'static str> = vec!["watched_count"];
        assert!(!crate::catalog::movie::PROTECTED_MOVIE_FIELDS.contains(&unprotected[0]));
    }

    // ---------------------------------------------------------- 演员

    /// ★ codec 表与白名单**一一对应**。
    ///
    /// 少一个的后果不是编译失败：那个字段进不了 `actor_codec_of`，于是
    /// [`validate_actor_fields`] 会拿它当「未声明 codec」拒掉 —— 插件写一个
    /// 本来合法的字段却拿到错误。多一个则更糟：白名单里没有的字段能被拼进 SQL。
    #[test]
    fn actor_codecs_cover_the_whitelist_exactly() {
        let mut whitelist = crate::catalog::actor::PROTECTED_ACTOR_FIELDS.to_vec();
        whitelist.sort_unstable();
        let mut codecs: Vec<&str> = ACTOR_FIELD_CODECS.iter().map(|(name, _)| *name).collect();
        codecs.sort_unstable();
        assert_eq!(codecs, whitelist);
    }

    /// ★ 严格 ISO 日期：`2020-1-1` 必须被拒。
    ///
    /// 这条是**手写解析最容易放过的一格**：`NaiveDate::parse_from_str(_, "%Y-%m-%d")`
    /// 收得下它（格式串不要求补零），而上游靠
    /// 「解析后再 `isoformat()` 必须逐字符相同」把这种写法挡掉。
    #[test]
    fn iso_dates_must_be_zero_padded() {
        assert!(parse_iso_date_exact("2020-01-01").is_ok());
        for bad in ["2020-1-1", "2020-01-1", "1-1-1", "2020/01/01", "生"] {
            assert!(parse_iso_date_exact(bad).is_err(), "{bad} 应被拒");
        }
    }

    fn actor_patch() -> FieldPatch {
        let mut patch = FieldPatch::new();
        patch.int("height_cm", Some(160));
        patch
    }

    /// 白名单外的字段**直接报错**（影片侧是静默跳过 —— 演员字段少，传错更
    /// 可能是代码写错）。身份/头像/订阅三者都不可写。
    #[test]
    fn actor_validation_rejects_fields_outside_the_whitelist() {
        for field in ["javdb_id", "profile_image_id", "is_subscribed", "name"] {
            let mut patch = FieldPatch::new();
            match field {
                "profile_image_id" => patch.int(field, Some(1)),
                "is_subscribed" => patch.flag(field, true),
                _ => patch.text(field, Some("x")),
            };
            assert!(
                validate_actor_fields(&patch).is_err(),
                "{field} 不该允许（白名单外）"
            );
        }
    }

    /// `gender` 只认 `{1, 2}`，且**不接受 `None`** —— 它没有「未知」态。
    #[test]
    fn actor_gender_is_a_two_value_enum() {
        for good in [1, 2] {
            let mut patch = FieldPatch::new();
            patch.int("gender", Some(good));
            assert!(validate_actor_fields(&patch).is_ok(), "{good} 合法");
        }
        for bad in [0, 3, -1] {
            let mut patch = FieldPatch::new();
            patch.int("gender", Some(bad));
            assert!(validate_actor_fields(&patch).is_err(), "{bad} 非法");
        }
        let mut patch = FieldPatch::new();
        patch.int("gender", None);
        assert!(validate_actor_fields(&patch).is_err(), "gender 不能清空");
    }

    /// 三围这类整数必须是**正整数**（`0` / 负数都不行），而 `None` 可以。
    #[test]
    fn actor_measurements_are_positive_and_clearable() {
        for bad in [0, -1] {
            let mut patch = FieldPatch::new();
            patch.int("height_cm", Some(bad));
            assert!(validate_actor_fields(&patch).is_err(), "{bad} 非法");
        }
        let mut patch = FieldPatch::new();
        patch.int("height_cm", None);
        assert!(
            validate_actor_fields(&patch).is_ok(),
            "清空身高是合法的（上游：None 显式清空并保留归属）"
        );
    }

    /// 文本必须 1..255 且非空白；**要清空请传 `None`**。
    #[test]
    fn actor_text_fields_reject_blanks_and_overlong_values() {
        let mut blank = FieldPatch::new();
        blank.text("cup", Some("   "));
        assert!(validate_actor_fields(&blank).is_err(), "空白串不是「有值」");

        let mut too_long = FieldPatch::new();
        too_long.text("birthplace", Some(&"x".repeat(256)));
        assert!(validate_actor_fields(&too_long).is_err(), "超过 255 字符");

        let mut boundary = FieldPatch::new();
        boundary.text("birthplace", Some(&"x".repeat(255)));
        assert!(validate_actor_fields(&boundary).is_ok(), "255 是合法的上界");

        let mut cleared = FieldPatch::new();
        cleared.text("cup", None);
        assert!(validate_actor_fields(&cleared).is_ok());
    }

    /// 空 patch 一律拒 —— 它只会白扫一趟库。
    #[test]
    fn actor_validation_rejects_an_empty_patch() {
        assert!(validate_actor_fields(&FieldPatch::new()).is_err());
        assert!(validate_actor_fields(&actor_patch()).is_ok());
    }

    /// 两个 owner 标记是**稳定字符串**（存在 jsonb 里，跨版本要能识别）。
    #[test]
    fn actor_owner_tags_are_pinned() {
        assert_eq!(HOST_JAVDB_OWNER, "host:javdb");
        assert_eq!(field_owner::HOST_MANUAL, "host:manual");
        assert_eq!(field_owner::plugin("local"), "plugin:local");
    }

    /// 释放归属：字段列表非空且必须在白名单里。
    #[test]
    fn actor_release_checks_its_field_list() {
        assert!(validate_release_fields(&[]).is_err(), "空列表该拒");
        assert!(validate_release_fields(&["height_cm"]).is_ok());
        assert!(
            validate_release_fields(&["javdb_id"]).is_err(),
            "白名单外的字段不该进释放列表"
        );
    }
}
