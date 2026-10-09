//! 影片字段主权的**唯一**写入门（上游 `catalog/movie_ownership_gateway.py`，240 行）。
//!
//! # 为什么必须是「网关」而不是工具函数
//!
//! `movie` 表有一列 `field_owners`（jsonb），记录**每个字段归谁写**。三种写入者：
//!
//! | 写入者 | `field_owners` 里的 owner | 能写什么 |
//! |---|---|---|
//! | 插件 | `plugin:{plugin_id}` | 该插件声明的字段 |
//! | 宿主自动规则 | （无归属） | **仅限未被占用的字段** |
//! | 人工 | `host:manual` | 含受保护字段（见下） |
//!
//! 只要存在任何一条直接 `UPDATE movie SET ...` 的旁路，人工标记就会被自动
//! 规则覆盖 —— **用户设的黑名单会自己消失**。所以三个入口都必须经过这里。
//!
//! # 受保护字段只有两个：`is_collection` / `is_blacklisted`
//!
//! 它们是**人工决策**的表达。自动路径碰它们时的行为是**静默跳过**
//! （[`MovieOwnershipGateway::update_host_unowned`]），不是报错 —— 自动流程不该
//! 因为「这个字段归人工管」而整体失败。
//!
//! ⚠️ **别往这个列表里加字段**：加进去意味着某条现有自动流程会突然开始
//! 静默失效，而那极难排查（表现为「元数据再也刷不出来」）。
//!
//! # `expected_revision` 是乐观锁，返回 `false` 而非报错
//!
//! [`MovieOwnershipGateway::patch_plugin`] 要求传调用方读到的版本号；不匹配
//! 返回 `Ok(false)`。没有它，两个插件同时补录同一部影片会互相覆盖，且最后
//! 写入的那份元数据可能**更旧**。
//!
//! # 抛 `ValueError` 而不是 `ApiError`
//!
//! 这个网关只在 service 内部调用，不直接对 HTTP。所以「字段名不在白名单里」
//! 是**编程错误**（调用方拼错了），不是用户请求的问题。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 人工写入的 owner 标记。
pub const MANUAL_MOVIE_FIELD_OWNER: &str = "host:manual";

/// 受保护字段 —— **只能人工写**。见模块文档。
pub const PROTECTED_MOVIE_FIELDS: [&str; 2] = ["is_collection", "is_blacklisted"];

/// 该字段是否受保护。
pub fn is_protected(field: &str) -> bool {
    PROTECTED_MOVIE_FIELDS.contains(&field)
}

/// 写入结果。`false` = 乐观锁不匹配（期间被改过），**这不是错误**。
pub type GatewayResult = bool;

/// 影片字段主权网关。
pub struct MovieOwnershipGateway;

impl MovieOwnershipGateway {
    /// 插件写入。
    ///
    /// 上游 `patch_plugin(cls, movie_id, plugin_id, fields, expected_revision)`。
    /// **只写该插件已拥有归属的字段** —— 抢别人的字段是 403 语义，但这里
    /// 表现为「该字段被跳过」（见 [`Self::update_host_unowned`] 的同款语义）。
    pub async fn patch_plugin(
        movie_id: i64,
        plugin_id: &str,
        fields: &serde_json::Value,
        expected_revision: i64,
    ) -> Result<GatewayResult, ServiceError> {
        let _ = (movie_id, plugin_id, fields, expected_revision);
        todo!("骨架：字段白名单校验 -> 乐观锁 -> 只改该 plugin 已拥有的字段（jsonb 合并）")
    }

    /// 宿主**自动**规则写入。只影响**无归属**的字段，返回受影响行数。
    ///
    /// 上游 `update_host_unowned(cls, movie_id, fields)`。受保护字段与已被
    /// 插件占有的字段在这里**都跳过**，不报错。
    pub async fn update_host_unowned(
        movie_id: i64,
        fields: &serde_json::Value,
    ) -> Result<u64, ServiceError> {
        let _ = (movie_id, fields);
        todo!("骨架：只写 field_owners 中无归属的字段；受保护/已占用字段静默跳过")
    }

    /// ★ 人工写入（批量）。**可以**写受保护字段，返回受影响行数。
    ///
    /// 上游 `update_host_manual(cls, movie_ids, fields)`。
    /// 这是黑名单、加入合集这类操作的**唯一**合法路径。
    pub async fn update_host_manual(
        movie_ids: &[i64],
        fields: &serde_json::Value,
    ) -> Result<u64, ServiceError> {
        let _ = (movie_ids, fields);
        todo!("骨架：owner 置为 host:manual 后写；这是受保护字段的唯一入口")
    }

    /// 插件卸载或字段弃用时**释放主权**，返回释放的字段数。
    ///
    /// 上游 `release_plugin_owners(cls, plugin_id, fields)`；`fields = None`
    /// 表示释放该插件的**全部**字段。
    ///
    /// **不释放的后果**：插件被移除后那些字段永远显示「已被占用」，自动规则
    /// 再也写不进去 —— 用户会看到「元数据再也刷不出来」，且没有任何报错。
    pub async fn release_plugin_owners(
        plugin_id: &str,
        fields: Option<&[&str]>,
    ) -> Result<u64, ServiceError> {
        let _ = (plugin_id, fields);
        todo!("骨架：从 field_owners 摘掉该 owner；fields=None 时全摘")
    }
}

/// 字段归属快照（读侧）。给前端展示「这个字段是谁写的」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldOwnership {
    pub field: String,
    /// `host:manual` / `plugin:xxx` / `None`（无归属 = 宿主可写）。
    pub owner: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 受保护字段**恰好两个**，且就是那两个。
    #[test]
    fn exactly_two_fields_are_protected() {
        assert_eq!(PROTECTED_MOVIE_FIELDS.len(), 2);
        assert!(is_protected("is_collection"));
        assert!(is_protected("is_blacklisted"));
    }

    /// 其它字段**不受保护** —— 包括看起来也像人工决策的那些。
    ///
    /// `title` / `summary` / `release_date` 都会被自动流程写（补录/刷新），
    /// 把它们误列为受保护会让元数据永远刷不出来。
    #[test]
    fn ordinary_metadata_fields_stay_writable_by_automation() {
        for field in ["title", "summary", "release_date", "javdb_id", "heat"] {
            assert!(!is_protected(field), "{field} 不该受保护");
        }
    }

    /// 人工 owner 标记是**稳定字符串** —— 它存在 jsonb 里，跨版本要能识别。
    #[test]
    fn the_manual_owner_tag_is_pinned() {
        assert_eq!(MANUAL_MOVIE_FIELD_OWNER, "host:manual");
    }

    /// 归属快照能表达「无归属」—— 那是宿主自动规则**可以**写的状态。
    #[test]
    fn ownership_can_be_absent() {
        let free = FieldOwnership {
            field: "title".to_owned(),
            owner: None,
        };
        assert!(free.owner.is_none());
        let manual = FieldOwnership {
            field: "is_blacklisted".to_owned(),
            owner: Some(MANUAL_MOVIE_FIELD_OWNER.to_owned()),
        };
        assert_eq!(manual.owner.as_deref(), Some("host:manual"));
    }
}
