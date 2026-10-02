//! UPDATE 语句的 SET 构造器。
//!
//! # 为什么需要这个类型
//!
//! 上游 `TimestampedMixin` 有一段 `save()` 覆写，解决过一个真实 bug：
//!
//! ```python
//! def save(self, *args, **kwargs):
//!     """更新已有行时自动推进 updated_at。
//!
//!     peewee 的 ``default=`` 只在 INSERT 期生效、不会在 save() 时重算，所以在没有这段覆写之前
//!     updated_at 的实际语义是"创建时刻"——任何按它排序的"最近修改优先"列表（playlist、
//!     clip_collection、media、background_task_run、download_task 等）排的其实是创建顺序，而且
//!     不会报任何错。
//!     """
//!     if self._pk is not None:
//!         self.updated_at = utc_now_for_db()
//! ```
//!
//! Rust 侧的对应风险**更大**：SQL 是手写的，漏写 `SET updated_at` 不会有
//! 任何编译器提示或运行时报错，表现和上游曾经那个 bug 一模一样 ——
//! 五处「最近修改优先」列表静默排成创建顺序。
//!
//! 所以本模块把 `SET updated_at = now()` 固化进 API 形状：Repository 的
//! `update` 只接受 [`UpdateSet`]，不接收裸 `&[&str]`。漏写时间戳在**编译期**
//! 就不可能发生。
//!
//! # 与 peewee 的两种写路径对应
//!
//! 上游覆写只对 `save()` 生效，`Model.update(...)` / `insert_many(...)`
//! 绕过实例方法，仍需调用方自己带上 `updated_at`。Rust 侧同样区分：
//!
//! - 单行 insert / update：走本模块，时间戳自动
//! - 批量 insert / update：调用方必须显式传入时间戳，见 [`UpdateSet::touch`] 的文档

use std::borrow::Cow;

use chrono::NaiveDateTime;
use sqlx::types::Json;

use crate::error::DbError;

/// 更新字段的值。
///
/// 之所以统一包成 [`Json`] 而不是分开维护「字符串 / 整数 / 时间」三套
/// `Vec`：sqlx 的 `encode` 对这��类型都是 trait based，用 `Json` 作为
/// 中间表示可以让 [`sqlx::query`] 的 bind 逻辑保持单一路径。
pub type Value<'a> = Json<Cow<'a, ValueInner>>;

/// 列值的内部表示。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum ValueInner {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Timestamp(NaiveDateTime),
    Json(serde_json::Value),
}

impl<'a> From<&'a str> for ValueInner {
    fn from(v: &'a str) -> Self {
        Self::Text(v.to_owned())
    }
}

impl From<String> for ValueInner {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<i64> for ValueInner {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<i32> for ValueInner {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<bool> for ValueInner {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<f64> for ValueInner {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<f32> for ValueInner {
    fn from(v: f32) -> Self {
        Self::Float(f64::from(v))
    }
}

impl From<NaiveDateTime> for ValueInner {
    fn from(v: NaiveDateTime) -> Self {
        Self::Timestamp(v)
    }
}

impl From<serde_json::Value> for ValueInner {
    fn from(v: serde_json::Value) -> Self {
        Self::Json(v)
    }
}

impl<T> From<Option<T>> for ValueInner
where
    T: Into<ValueInner>,
{
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => Self::Null,
        }
    }
}

/// UPDATE 语句的 SET 构造器。
///
/// Repository 的 `update` 只接受本类型，不接收裸字段列表，因此
/// `SET updated_at = now()` 在类型层面无法被省略。
///
/// ```
/// # use sm_db::common::update::UpdateSet;
/// let mut set = UpdateSet::new();
/// set.set("title", "新标题").set("score", 8.5);
/// assert_eq!(set.len(), 2); // 此刻还没有 updated_at
///
/// // Repository 在提交前强制 touch，同名列会被去重
/// set.touch().touch();
/// assert_eq!(set.len(), 3); // 两个字段 + updated_at
/// ```
#[derive(Debug, Default)]
pub struct UpdateSet<'a> {
    fields: Vec<(&'a str, Value<'a>)>,
}

impl<'a> UpdateSet<'a> {
    /// 空构造。调用方通过 [`UpdateSet::set`] 与 [`UpdateSet::touch`] 填充。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个待更新列。
    ///
    /// `NULL` 不会被跳过 —— 传 `None::<i32>` 就是要把列置空，
    /// 调用方必须显式表达。想表达「本次不改动」用 [`UpdateSet::set_opt`]。
    ///
    /// 接受 `impl Into<ValueInner>` 而不是 `impl Serialize`：前者是编译期
    /// 的类型检查，列类型写错立刻报错；后者只能靠运行时的 serde 转换，
    /// 而本 crate 已经因为不能用 `query!` 宏失去了编译期列名校验，
    /// 不该再退让一步。
    pub fn set(&mut self, column: &'a str, value: impl Into<ValueInner>) -> &mut Self {
        // 同名列只保留最后一个：调用方重复 set 同一列（或重复 touch）时
        // 不该在 SET 子句里出现两次赋值。PostgreSQL 接受重复赋值但取最后
        // 一个，而重复的 updated_at 会让生成的 SQL 出现 `updated_at = $2,
        // updated_at = $3`，可读性与调试体验都差。
        let inner = value.into();
        match self.fields.iter_mut().find(|(name, _)| *name == column) {
            Some(slot) => slot.1 = Json(Cow::Owned(inner)),
            None => self.fields.push((column, Json(Cow::Owned(inner)))),
        }
        self
    }

    /// 追加 `updated_at = now()`。
    ///
    /// Repository 在提交前**强制调用一次**，因此调用方重复调用是无害的
    /// 幂等操作 —— 同名列会被去重，只保留最后一列。
    ///
    /// 批量写路径（对应 peewee 的 `Model.update()` / `insert_many()`，
    /// 它们绕过 `save()` 覆写）**必须**显式调用这个方法，否则时间戳不会推进。
    pub fn touch(&mut self) -> &mut Self {
        self.set("updated_at", crate::common::time::now_utc())
    }

    /// 追加 `created_at = now()`。
    ///
    /// 正常 INSERT 不需要这个（数据库默认值会填）；它存在是为了
    /// 「把一行复制成新行」这类场景。
    pub fn touch_created(&mut self) -> &mut Self {
        self.set("created_at", crate::common::time::now_utc())
    }

    /// 仅在 `Some` 时追加该列。
    ///
    /// 批量 PATCH 语义：`None` 表示「本次不改动这个字段」，
    /// 与 [`UpdateSet::set`] 传 `None` 表示「置空」是**不同**的意思。
    pub fn set_opt(&mut self, column: &'a str, value: Option<impl Into<ValueInner>>) -> &mut Self {
        if let Some(value) = value {
            self.set(column, value);
        }
        self
    }

    /// 移除某个列。
    ///
    /// 用于「调用方传了但被护栏拒绝」的情况：先记下来再删掉，
    /// 比根本不记更安全 —— 能验证护栏确实拦住了。
    pub fn remove(&mut self, column: &str) -> &mut Self {
        self.fields.retain(|(name, _)| *name != column);
        self
    }

    /// 是否包含指定列。
    pub fn contains(&self, column: &str) -> bool {
        self.fields.iter().any(|(name, _)| *name == column)
    }

    /// 字段数（不含 `updated_at`）。
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// 是否为空。
    ///
    /// Repository 在提交前应检查：空 UPDATE 会命中 `rows_affected = 0`，
    /// 与「行不存在」难以区分，直接返回 `NotFound` 会误导调用方。
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// 取出字段与值，供 Repository 拼 SQL。
    pub fn into_fields(self) -> Vec<(&'a str, Value<'a>)> {
        self.fields
    }

    /// 借用字段与值。
    pub fn fields(&self) -> &[(&'a str, Value<'a>)] {
        &self.fields
    }

    /// 生成 `SET a = $1, b = $2, ...` 片段。
    ///
    /// 占位符从 `start` 开始编号，便于嵌进已有 `$N` 的查询里。
    pub fn assignments(&self, start: usize) -> String {
        self.fields
            .iter()
            .enumerate()
            .map(|(i, (name, _))| format!("{name} = ${}", start + i))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 校验并取出，用于 UPDATE。
    ///
    /// 空 `UpdateSet` 直接报错而不是静默执行 —— 那会返回
    /// `rows_affected = 0`，和「行不存在」无法区分。
    pub fn finish(self, entity: &'static str) -> Result<Vec<(&'a str, Value<'a>)>, DbError> {
        if self.fields.is_empty() {
            return Err(DbError::business(entity, "没有要更新的字段"));
        }
        Ok(self.fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn touch_is_idempotent() {
        // Repository 会强制调一次 touch，所以重复调用必须无害，
        // 否则 SET 子句里会出现两个 updated_at。
        let mut set = UpdateSet::new();
        set.set("title", "a").touch().touch().touch();
        assert_eq!(
            set.fields()
                .iter()
                .filter(|(name, _)| *name == "updated_at")
                .count(),
            1,
            "updated_at 只应出现一次"
        );
    }

    #[test]
    fn set_opt_distinguishes_absent_from_null() {
        let mut set = UpdateSet::new();
        set.set_opt("title", Some("x"));
        set.set_opt("summary", Option::<String>::None); // 不动这个字段
        assert_eq!(set.len(), 1);
        assert!(set.contains("title"));
        assert!(!set.contains("summary"));
    }

    #[test]
    fn set_with_none_writes_null() {
        // 与 set_opt 相反：显式 None 就是要把列置空.
        let mut set = UpdateSet::new();
        set.set("summary", Option::<String>::None);
        assert_eq!(set.len(), 1);
        let fields = set.into_fields();
        assert!(
            matches!(&*fields[0].1 .0, ValueInner::Null),
            "应写 NULL 而不是跳过"
        );
    }

    #[test]
    fn empty_update_is_rejected() {
        // 空 UPDATE 会返回 rows_affected = 0，与「行不存在」无法区分。
        let err = UpdateSet::new().finish("Movie").unwrap_err();
        assert!(matches!(err, DbError::Business { .. }));
        assert!(err.to_string().contains("没有要更新的字段"));
    }

    #[test]
    fn assignments_number_from_offset() {
        let mut set = UpdateSet::new();
        set.set("title", "x").set("score", 1.0);
        assert_eq!(set.assignments(1), "title = $1, score = $2");
        // 嵌进已有占位符的查询时从 $3 起
        assert_eq!(set.assignments(3), "title = $3, score = $4");
    }

    #[test]
    fn remove_lets_guard_reject_silently() {
        // 护栏拦下越权字段后，调用方可以把它从 SET 里摘掉。
        let mut set = UpdateSet::new();
        set.set("title", "x").set("is_blacklisted", true);
        assert!(set.contains("is_blacklisted"));
        set.remove("is_blacklisted");
        assert!(!set.contains("is_blacklisted"));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn json_value_can_be_written_directly() {
        // JSONB 列（Movie.field_owners）需要直传 serde_json::Value。
        let mut set = UpdateSet::new();
        set.set("field_owners", json!({"title": "plugin:x"}));
        let fields = set.into_fields();
        match &*fields[0].1 .0 {
            ValueInner::Json(value) => assert_eq!(*value, json!({"title": "plugin:x"})),
            other => panic!("expected Json variant, got {other:?}"),
        }
    }
}
