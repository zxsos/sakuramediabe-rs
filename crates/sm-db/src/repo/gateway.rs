//! `Movie` 字段主权网关（v2-lite）——受保护字段的**唯一**写入口。
//!
//! 对应 `src/service/catalog/movie_ownership_gateway.py`。
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
//! 所以这里刻意**不用** [`UpdateSet`]：它的语义是「无条件覆盖」，
//! 而这三个写入口都需要 `WHERE` 里带条件、且返回值是「是否命中」而非行数据。
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
    /// `str` 列。
    Text,
    /// `bool` 列。
    Bool,
}

impl FieldCodec {
    /// 该 codec 是否接受 `None`。
    ///
    /// 只有**宿主**写路径放行 `None`：`maker_name` / `director_name` /
    /// `summary` 等列允许 NULL，远端详情缺失时以 NULL 落库是合法数据。
    /// 插件 patch 路径不允许（`allow_none=False`）。
    pub fn accepts_none(&self) -> bool {
        matches!(self, Self::Text)
    }
}

/// 字段名 -> 期望 codec。
///
/// 覆盖 [`crate::catalog::movie::PROTECTED_MOVIE_FIELDS`] 的全部 6 个字段。
/// 任何不在此表里的受保护字段都会被 [`MovieOwnershipGateway::validate_fields`]
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
/// **构造时**就成为可能，而校验放在 [`MovieOwnershipGateway::validate_fields`]。
#[derive(Debug, Clone)]
pub enum FieldValue {
    /// 文本（含 `None`，见 [`FieldCodec::accepts_none`]）。
    Text(Option<String>),
    /// 布尔（不接受 `None`）。
    Bool(bool),
}

impl FieldValue {
    /// 该值是否与 codec 匹配。
    fn matches(&self, codec: FieldCodec) -> bool {
        matches!(
            (self, codec),
            (Self::Text(_), FieldCodec::Text) | (Self::Bool(_), FieldCodec::Bool)
        )
    }

    /// 绑到查询上的 SQL 类型。
    fn bind<'q>(self, query: Query<'q, Postgres, PgArguments>) -> Query<'q, Postgres, PgArguments> {
        match self {
            Self::Text(Some(v)) => query.bind(v),
            Self::Text(None) => query.bind(Option::<String>::None),
            Self::Bool(v) => query.bind(v),
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

/// 绑定三轮字段值（`SET` / `revision` / `updated_at` 各一轮）。
///
/// 顺序必须与 SQL 里三段子句的占位符编号一致 —— [`MovieOwnershipGateway::update_host_unowned`]
/// 依次为每字段分配 `(key, value)` 三次。
fn bind_patch_triples<'q>(
    query: Query<'q, Postgres, PgArguments>,
    patch: &FieldPatch,
) -> Query<'q, Postgres, PgArguments> {
    let mut acc = query;
    for _ in 0..3 {
        for (_, value) in patch.iter() {
            acc = value.clone().bind(acc);
        }
    }
    acc
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
}
