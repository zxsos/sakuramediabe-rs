//! 字段护栏：阻止越权字段写入。
//!
//! # 两种护栏的区别
//!
//! | | 受保护字段（`PROTECTED_MOVIE_FIELDS`） | 插件白名单 |
//! |---|---|---|
//! | 谁能写 | 插件 + 人工 | 插件 |
//! | 谁能读 | 所有人 | 所有人 |
//! | 宿主持久化 | 拒绝（须走 gateway） | 允许 |
//!
//! 上游注释写得很直接：
//!
//! ```python
//! # 受保护字段白名单（v2-lite 字段主权）：插件可写字段的宿主固定名单。
//! # 开放文案、厂商/导演、合集判定与屏蔽状态；宿主刷新和人工修改均收敛走 gateway。
//! # 白名单非空后，已持久化 Movie 的裸 save() 会被护栏拒绝。
//! ```
//!
//! 「白名单非空后裸 `save()` 被拒绝」是**关键**：一旦某字段进入白名单，
//! 绕过 gateway 的写入路径必须失败，而不是静默生效。

use crate::error::DbError;

/// 写入来源。决定它能碰哪些字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSource {
    /// 宿主内部代码（service 层、gateway）。
    Host,
    /// 人工修改（后台表单）。
    Manual,
    /// 插件写入。
    Plugin {
        /// 插件 ID，对应 `field_owners` 里的 owner key。
        plugin_id: &'static str,
    },
}

impl WriteSource {
    /// 日志标签。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Manual => "host:manual",
            Self::Plugin { .. } => "plugin",
        }
    }
}

/// 字段护栏。
///
/// 只做一件事：判断「这个来源能不能写这个字段」。用 `&[&str]` 白名单而非
/// 黑名单 —— 新增受保护字段时忘了加进黑名单，那次写入会**静默成功**；
/// 用白名单则默认拒绝，只有显式列出的才放行。
#[derive(Debug, Clone)]
pub struct FieldGuard {
    entity: &'static str,
    /// 插件可写、但宿主持久化必须拒绝的字段。
    plugin_writable: &'static [&'static str],
    /// 完全禁止任何来源写入的字段（护栏自身状态列）。
    host_only: &'static [&'static str],
}

impl FieldGuard {
    /// 构造护栏。
    ///
    /// * `plugin_writable` —— 对应上游的「受保护字段白名单」：插件可写，
    ///   但宿主持久化与人工裸写都要走 gateway。
    /// * `host_only` —— 护栏自身的状态列（如 `field_owners`、
    ///   `mutation_revision`），任何插件都不能写。
    pub const fn new(
        entity: &'static str,
        plugin_writable: &'static [&'static str],
        host_only: &'static [&'static str],
    ) -> Self {
        Self {
            entity,
            plugin_writable,
            host_only,
        }
    }

    /// 字段是否被某来源允许写入。
    pub fn allows(&self, field: &str, source: WriteSource) -> bool {
        // 护栏自身状态列：只有宿主能写。
        if self.host_only.contains(&field) {
            return matches!(source, WriteSource::Host);
        }
        match source {
            // 宿主内部代码不受护栏约束 —— 它是 gateway 的实现方。
            WriteSource::Host => true,
            WriteSource::Manual => {
                // 人工修改受保护字段是允许的（后台表单就是干这个的），
                // 但必须走 gateway 以同步 field_owners。
                true
            }
            WriteSource::Plugin { .. } => self.plugin_writable.contains(&field),
        }
    }

    /// 校验一批字段，返回第一个被拒的。
    ///
    /// 批量更新时**不做部分写入** —— 宁可整批失败，也不要写进去一半：
    /// 那会留下「title 更新了但 is_blacklisted 没更新」的不一致状态。
    pub fn check_all<'a>(
        &self,
        fields: impl IntoIterator<Item = &'a str>,
        source: WriteSource,
    ) -> Result<(), DbError> {
        for field in fields {
            if !self.allows(field, source) {
                return Err(DbError::business(
                    self.entity,
                    format!(
                        "字段 `{field}` 不允许 {} 写入（受保护字段须走 gateway）",
                        match source {
                            WriteSource::Plugin { plugin_id } => {
                                format!("插件 {plugin_id}")
                            }
                            _ => source.label().to_owned(),
                        }
                    ),
                ));
            }
        }
        Ok(())
    }

    /// 该实体是否对插件开放写入。
    pub fn accepts_plugin_writes(&self) -> bool {
        !self.plugin_writable.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::actor::GUARDED_ACTOR_FIELDS;
    use crate::catalog::movie::PROTECTED_MOVIE_FIELDS;

    fn movie_guard() -> FieldGuard {
        FieldGuard::new(
            "Movie",
            &PROTECTED_MOVIE_FIELDS,
            &["field_owners", "mutation_revision"],
        )
    }

    #[test]
    fn plugin_can_write_protected_fields_only() {
        let guard = movie_guard();
        let plugin = WriteSource::Plugin {
            plugin_id: "actor-metadata",
        };

        assert!(guard.allows("title", plugin));
        assert!(guard.allows("is_blacklisted", plugin));

        // 不在白名单的字段插件不能碰
        assert!(!guard.allows("watched_count", plugin));
        assert!(!guard.allows("movie_number", plugin));
    }

    #[test]
    fn host_owns_guard_state_columns_exclusively() {
        let guard = movie_guard();

        // field_owners / mutation_revision 是护栏自身的状态，
        // 插件写了就能伪造「这个字段归我所有」。
        for source in [WriteSource::Plugin { plugin_id: "p" }, WriteSource::Manual] {
            assert!(!guard.allows("field_owners", source));
            assert!(!guard.allows("mutation_revision", source));
        }
        assert!(guard.allows("field_owners", WriteSource::Host));
    }

    #[test]
    fn host_bypasses_the_guard_entirely() {
        // 宿主是 gateway 的实现方，护栏拦它只会把正常流程堵死。
        let guard = movie_guard();
        for field in ["title", "field_owners", "watched_count", "任何不存在的列"] {
            assert!(guard.allows(field, WriteSource::Host), "{field}");
        }
    }

    #[test]
    fn batch_rejects_wholesale_not_partially() {
        let guard = movie_guard();
        let plugin = WriteSource::Plugin { plugin_id: "p" };

        // 混了一批合法与非法字段 —— 整批失败，不能写进去一半。
        let err = guard
            .check_all(["title", "is_blacklisted", "watched_count"], plugin)
            .unwrap_err();
        match err {
            DbError::Business { entity, reason } => {
                assert_eq!(entity, "Movie");
                assert!(reason.contains("watched_count"), "{reason}");
                assert!(reason.contains("插件 p"), "{reason}");
            }
            other => panic!("expected Business, got {other:?}"),
        }

        // 全合法才通过
        assert!(guard.check_all(["title", "summary"], plugin).is_ok());
    }

    #[test]
    fn actor_guard_uses_its_own_whitelist() {
        // GUARDED_ACTOR_FIELDS 是 **host-only** 列表（上游注释：「不在插件可写
        // 白名单内，但也禁止裸写」），所以它是 host_only 参数，不是
        // plugin_writable —— Actor 根本没开放插件写入。
        let guard = FieldGuard::new("Actor", &[], &GUARDED_ACTOR_FIELDS);
        let plugin = WriteSource::Plugin { plugin_id: "p" };

        for field in GUARDED_ACTOR_FIELDS {
            assert!(!guard.allows(field, plugin), "{field} 应禁止插件写入");
            assert!(guard.allows(field, WriteSource::Host), "{field} 宿主可写");
        }
        assert!(!guard.accepts_plugin_writes(), "Actor 未开放插件写入");

        // 与 Movie 的差异：Movie 开放插件写 title，Actor 不开放任何字段。
        assert!(movie_guard().allows("title", plugin));
        assert!(!guard.allows("title", plugin));
    }

    #[test]
    fn guard_with_empty_whitelist_closes_plugin_writes() {
        // 白名单为空 = 未开放插件写入，所有插件写都拒。
        let guard = FieldGuard::new("Thing", &[], &[]);
        let plugin = WriteSource::Plugin { plugin_id: "p" };
        assert!(!guard.allows("anything", plugin));
        assert!(!guard.accepts_plugin_writes());
        assert!(guard.allows("anything", WriteSource::Host));
    }
}
