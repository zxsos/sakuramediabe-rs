//! 演员资料的受控写入（上游 `catalog/actor_ownership_gateway.py`，141 行）。
//!
//! # 与 [`super::movie_ownership_gateway`] 同构，但**多一个 owner**
//!
//! | owner | 谁写的 | 优先级 |
//! |---|---|---|
//! | `host:manual` | 人工（改名/换头像/订阅） | 最高 |
//! | `host:javdb` | JavDB 补录 | 中 |
//! | `plugin:{id}` | 插件 | 低 |
//! | （无归属） | 宿主通用规则 | 最低 |
//!
//! # 身份、头像、订阅**不在插件可写白名单内**
//!
//! 上游 docstring 原话：「身份、头像和订阅不在插件写入白名单内」。
//! 放开任何一项都等于把人工决策交给插件 —— `javdb_id` 能改就等于可以把影片
//! 挂到别的 JavDB 条目上（伪造身份）。
//!
//! # 字段不在白名单 → `ValueError`（快速失败），与影片侧不同
//!
//! 影片网关对不可写字段是**静默跳过**（字段多、误传概率低）；演员字段少，
//! 误传更可能是代码写错，所以直接报编程错误。
//!
//! # `update_host_source` 用「owner 参数」而不是两个方法
//!
//! 传 [`MANUAL_ACTOR_FIELD_OWNER`] 调它就等价于人工写入。⚠️ 别用它写
//! `is_subscribed` —— 订阅是人工决策。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// JavDB 补录的 owner 标记。
pub const JAVDB_ACTOR_FIELD_OWNER: &str = "host:javdb";
/// 人工写入的 owner 标记。
pub const MANUAL_ACTOR_FIELD_OWNER: &str = "host:manual";

/// **插件不可写**的字段（上游白名单的反面）。
pub const PLUGIN_UNWRITABLE_FIELDS: [&str; 3] = ["javdb_id", "profile_image_id", "is_subscribed"];

/// 该字段是否允许插件写入。
pub fn plugin_may_write(field: &str) -> bool {
    !PLUGIN_UNWRITABLE_FIELDS.contains(&field)
}

/// 写入结果。`false` = 乐观锁不匹配。
pub type GatewayResult = bool;

/// 演员字段主权网关。
pub struct ActorOwnershipGateway;

impl ActorOwnershipGateway {
    /// 插件写入。`expected_revision` 不匹配返回 `Ok(false)`。
    pub async fn patch_plugin(
        actor_id: i64,
        plugin_id: &str,
        fields: &serde_json::Value,
        expected_revision: i64,
    ) -> Result<GatewayResult, ServiceError> {
        let _ = (actor_id, plugin_id, fields, expected_revision);
        todo!("骨架：字段白名单校验（javdb_id/头像/订阅不可写）-> 乐观锁 -> jsonb 合并")
    }

    /// JavDB 补录（或人工，写法见模块文档）写入。
    ///
    /// 只影响**无归属**或**已被同一 owner 占有**的字段。
    pub async fn update_host_source(
        actor_id: i64,
        fields: &serde_json::Value,
        owner: &str,
    ) -> Result<GatewayResult, ServiceError> {
        let _ = (actor_id, fields, owner);
        todo!("骨架：owner 置为给定标记；只覆盖无归属或同 owner 的字段")
    }

    /// 释放某 owner 占用的字段。`fields = None` = 释放其**全部**。
    ///
    /// 不释放的后果：插件被移除后字段永久显示「已被占用」，自动规则再也写不进去。
    pub async fn release_plugin_owners(
        plugin_id: &str,
        fields: Option<&[&str]>,
    ) -> Result<u64, ServiceError> {
        let _ = (plugin_id, fields);
        todo!("骨架：从 field_owners 摘掉该 owner；fields=None 时全摘")
    }
}

/// 字段归属快照（读侧）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorFieldOwnership {
    pub field: String,
    /// `host:manual` / `host:javdb` / `plugin:xxx` / `None`。
    pub owner: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 身份、头像、订阅三者禁止插件写入。
    #[test]
    fn identity_avatar_and_subscription_are_off_limits_to_plugins() {
        for field in ["javdb_id", "profile_image_id", "is_subscribed"] {
            assert!(!plugin_may_write(field), "{field} 不该允许插件写");
        }
    }

    /// 描述性字段**允许**插件写。
    #[test]
    fn descriptive_fields_are_writable_by_plugins() {
        for field in ["name", "alias", "summary", "birthday"] {
            assert!(plugin_may_write(field), "{field} 应该允许插件写");
        }
    }

    /// 两个 owner 标记是**稳定字符串**（存在 jsonb 里，跨版本要能识别）。
    #[test]
    fn the_owner_tags_are_pinned() {
        assert_eq!(JAVDB_ACTOR_FIELD_OWNER, "host:javdb");
        assert_eq!(MANUAL_ACTOR_FIELD_OWNER, "host:manual");
    }

    /// 归属快照能表达四种 owner 状态。
    #[test]
    fn ownership_distinguishes_all_four_states() {
        let states = [
            None,
            Some(MANUAL_ACTOR_FIELD_OWNER.to_owned()),
            Some(JAVDB_ACTOR_FIELD_OWNER.to_owned()),
            Some("plugin:local".to_owned()),
        ];
        let mapped: Vec<ActorFieldOwnership> = states
            .into_iter()
            .map(|owner| ActorFieldOwnership {
                field: "name".to_owned(),
                owner,
            })
            .collect();
        assert_eq!(mapped.len(), 4);
        assert!(mapped[0].owner.is_none(), "无归属 = 宿主通用规则可写");
        assert_eq!(mapped[1].owner.as_deref(), Some("host:manual"));
        assert_eq!(mapped[2].owner.as_deref(), Some("host:javdb"));
    }
}
