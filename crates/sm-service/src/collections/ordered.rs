//! 时刻合集与片段合集的 service。
//!
//! 对应上游 `src/service/collections/moment_collection_service.py`（343 行）
//! 与 `clip_collection_service.py`（271 行）。
//!
//! # 为什么用宏而不是两个独立文件
//!
//! 两份的规则**逐条相同**，只差三样：
//!
//! | | moment | clip |
//! |---|---|---|
//! | 父表 | `moment_collection` | `clip_collection` |
//! | 成员 | `media_point` | `media_clip` |
//! | 名称冲突错误码 | `moment_collection_name_conflict` | `clip_collection_name_conflict` |
//!
//! 「只差表名与成员类型」正是 `sm_db` 侧 `impl_collection_repo!` 与
//! `impl_ordered_member_repo!` 已处理过的形状。这里沿用同一套路：七个方法
//! 必然一致，抄两遍只多两处会分叉的地方。
//!
//! # 三条容易搞反的规则
//!
//! **`add` 在已是成员时无操作**，且**不**改位置。已存在就 `return` —— 不是
//! 「移到末尾」。这三张表**有** `position`，所以重复加入需要决定位置；上游
//! 的选择是「不动」。
//!
//! **`remove` 只在真删掉行时才推进父表的 `updated_at`。** 否则「移出一部
//! 不在合集里的东西」会刷新排序时间，把合集顶到「最近活跃」最前面。
//!
//! **`set_members` 去重且保留顺序。** 上游用一个 `seen` 集合边走边滤，
//! 保留**首次出现**的位置 —— 不是 `sorted(set(...))`，那会把顺序丢掉，
//! 而拖拽排序的结果必须原样保留。然后逐个校验存在性，再在一个事务里
//! 「清空 + 按序插入」。
//!
//! # 成员存在性查的是成员的**父**表
//!
//! `require_member` 校验的是 `media_point` / `media_clip` 行存在，而不是
//! `moment_collection_item` / `clip_collection_item` 行存在。后者是**关联
//! 表** —— 它的行存在只说明「这个关联存在」，而「关联存在」蕴含「父行
//! 存在」（外键保证）。所以查关联表恒为真，查父表才是有意义的检查。
//!
//! 这也是为什么宏参数里有 `$origin_repo`：成员表仓储回答不了这个问题。

use std::collections::HashSet;

use sm_db::collections::{ClipCollection, MomentCollection};
use sm_db::repo::playback::{MediaClipRepository, MediaPointRepository};
use sm_db::repo::{
    ClipCollectionItemRepository, ClipCollectionRepository, MomentCollectionItemRepository,
    MomentCollectionRepository, NewCollection,
};

use crate::error::{details_of, ServiceError};

/// 更新合集的请求。两个字段都可缺省 —— 缺省表示不改动。
#[derive(Debug, Clone, Default)]
pub struct CollectionUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl CollectionUpdate {
    /// 是否有任何字段被给出。上游对空更新返回 422。
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.description.is_none()
    }
}

/// 按首次出现去重，保留顺序。
///
/// 与 `sorted(set(...))` 的区别是**顺序**：拖拽排序的结果必须原样保留，
/// 而 `HashSet` 是无序的。
///
/// 单独成函数而不是内联进宏：它本身值得测，而宏展开的代码不便于直接测。
fn dedup_preserving_order(ids: &[i32]) -> Vec<i32> {
    let mut seen: HashSet<i32> = HashSet::with_capacity(ids.len());
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

/// 生成时刻合集与片段合集两个 service。
#[macro_export]
macro_rules! impl_ordered_collection_service {
    (
        $svc:ident,
        $parent_repo:ty,
        $member_repo:ty,
        $origin_repo:ty,
        $parent_model:ty,
        $member_model:ty,
        $entity:literal,
        $name_conflict:literal,
        $collection_missing:literal,
        $member_missing:literal,
        $member_field:ident,
        $member_key:literal
    ) => {
        #[doc = concat!("`", $entity, "` 表的 service。")]
        ///
        /// 规则与上游 `src/service/collections/` 下同名文件逐条对应。
        pub struct $svc {
            parent: $parent_repo,
            members: $member_repo,
            origin: $origin_repo,
            pool: sm_db::Db,
        }

        impl $svc {
            pub fn new(db: &sm_db::Db) -> Self {
                Self {
                    parent: <$parent_repo>::new(db.clone()),
                    members: <$member_repo>::new(db.clone()),
                    origin: <$origin_repo>::new(db.clone()),
                    pool: db.clone(),
                }
            }

            // ---------------------------------------------------------- 规则

            /// 名称归一：trim，空则 422。
            fn normalize_name(name: &str) -> Result<String, ServiceError> {
                let normalized = name.trim();
                if normalized.is_empty() {
                    return Err(ServiceError::validation(
                        "validation_error",
                        concat!($entity, " name cannot be empty"),
                    ));
                }
                Ok(normalized.to_owned())
            }

            /// 描述归一：`None` → 空串。空描述合法。
            fn normalize_description(description: Option<&str>) -> String {
                description.unwrap_or_default().trim().to_owned()
            }

            /// 名称唯一性。`exclude_id` 用于更新时排除自己。
            async fn ensure_name_available(
                &self,
                name: &str,
                exclude_id: Option<i32>,
            ) -> Result<(), ServiceError> {
                match self.parent.find_by_name(name).await? {
                    Some(existing) if Some(existing.id) == exclude_id => Ok(()),
                    Some(_) => Err(ServiceError::conflict(
                        $name_conflict,
                        concat!($entity, " name already exists"),
                        Some(details_of("name", name)),
                    )),
                    None => Ok(()),
                }
            }

            /// 取合集，不存在则 404。
            async fn require_collection(&self, id: i32) -> Result<$parent_model, ServiceError> {
                self.parent.find_by_id(id).await?.ok_or_else(|| {
                    ServiceError::not_found(
                        concat!($entity, "_not_found"),
                        $collection_missing,
                        "collection_id",
                        id,
                    )
                })
            }

            /// 校验成员存在 —— 查的是**成员的父表**，见模块文档。
            async fn require_member(&self, id: i32) -> Result<(), ServiceError> {
                if self.origin.find_by_id(id).await?.is_some() {
                    Ok(())
                } else {
                    Err(ServiceError::not_found(
                        concat!($member_key, "_not_found"),
                        $member_missing,
                        $member_key,
                        id,
                    ))
                }
            }

            /// 从成员行取出外键值。两个成员模型的字段名不同
            /// （`point_id` / `clip_id`），所以分成两个宏参数：
            /// `$member_field` 是**标识符**（做字段访问），`$member_key` 是
            /// **字面量**（拼错误码与详情键）。
            ///
            /// 分成两个是因为 `m.$x` 要求 `x` 是 `ident`，而
            /// `concat!($x, "_not_found")` 要求 `x` 是 `literal` —— 一个参数
            /// 满足不了两者。
            fn member_key_of(m: &$member_model) -> i32 {
                m.$member_field
            }

            // ---------------------------------------------------------- 父表

            /// 新建合集。
            pub async fn create(
                &self,
                name: &str,
                description: Option<&str>,
            ) -> Result<$parent_model, ServiceError> {
                let name = Self::normalize_name(name)?;
                self.ensure_name_available(&name, None).await?;
                Ok(self
                    .parent
                    .insert(&NewCollection::host_owned(
                        &name,
                        &Self::normalize_description(description),
                    ))
                    .await?)
            }

            /// 更新合集。名字未变时**跳过**唯一性检查。
            pub async fn update(
                &self,
                id: i32,
                payload: CollectionUpdate,
            ) -> Result<$parent_model, ServiceError> {
                let mut current = self.require_collection(id).await?;
                if payload.is_empty() {
                    return Err(ServiceError::validation(
                        "validation_error",
                        "At least one field must be provided",
                    ));
                }
                if let Some(name) = payload.name {
                    let name = Self::normalize_name(&name)?;
                    if name != current.name {
                        self.ensure_name_available(&name, Some(id)).await?;
                    }
                    self.parent.rename(id, &name).await?;
                    current.name = name;
                }
                if let Some(description) = payload.description {
                    let description = Self::normalize_description(Some(&description));
                    self.parent.set_description(id, &description).await?;
                    current.description = description;
                }
                Ok(current)
            }

            /// 删除合集，成员随外键 `CASCADE` 一并消失。
            pub async fn delete(&self, id: i32) -> Result<(), ServiceError> {
                self.require_collection(id).await?;
                self.parent.delete(id).await?;
                Ok(())
            }

            // ---------------------------------------------------------- 成员

            /// 加入一个成员。**已在合集里则无操作**（不改位置）。
            ///
            /// 先查存在性而不是直接插入撞唯一索引：结果一样，但白跑一次
            /// 事务，而上游明确先查。
            pub async fn add(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<(), ServiceError> {
                self.require_collection(collection_id).await?;
                self.require_member(member_id).await?;
                let existing = self.members.list_by_collection(collection_id).await?;
                if existing.iter().any(|m| Self::member_key_of(m) == member_id) {
                    return Ok(());
                }
                self.members.append(collection_id, member_id).await?;
                self.parent.touch(collection_id).await?;
                Ok(())
            }

            /// 移出一个成员。**只有真删掉行才推进父表时间。**
            pub async fn remove(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<(), ServiceError> {
                self.require_collection(collection_id).await?;
                if self.members.unlink(collection_id, member_id).await? {
                    self.parent.touch(collection_id).await?;
                }
                Ok(())
            }

            /// 把成员**替换**为给定顺序的一组。
            ///
            /// 去重且保留首次出现的顺序；逐个校验存在性；清空与插入在
            /// **一个事务**里 —— 否则中间那一瞬合集是空的，用户刷新会看到
            /// 空合集。
            pub async fn set_members(
                &self,
                collection_id: i32,
                member_ids: &[i32],
            ) -> Result<(), ServiceError> {
                self.require_collection(collection_id).await?;
                let ordered = $crate::collections::ordered::dedup_preserving_order(member_ids);
                for id in &ordered {
                    self.require_member(*id).await?;
                }

                let mut tx = self.pool.begin().await?;
                {
                    let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, &self.pool);
                    self.members.clear_in(&mut ctx, collection_id).await?;
                    for (position, id) in ordered.iter().enumerate() {
                        self.members
                            .insert_at_in(&mut ctx, collection_id, *id, position as i32)
                            .await?;
                    }
                }
                tx.commit().await?;
                self.parent.touch(collection_id).await?;
                Ok(())
            }

            /// 按顺序列出全部成员。**刻意不分页** —— 合集长度有界。
            pub async fn list_members(
                &self,
                collection_id: i32,
            ) -> Result<Vec<$member_model>, ServiceError> {
                self.require_collection(collection_id).await?;
                Ok(self.members.list_by_collection(collection_id).await?)
            }
        }
    };
}

impl_ordered_collection_service!(
    MomentCollectionService,
    MomentCollectionRepository,
    MomentCollectionItemRepository,
    MediaPointRepository,
    MomentCollection,
    sm_db::collections::MomentCollectionItem,
    "moment_collection",
    "moment_collection_name_conflict",
    "Moment collection not found",
    "Media point not found",
    point_id,
    "point_id"
);

impl_ordered_collection_service!(
    ClipCollectionService,
    ClipCollectionRepository,
    ClipCollectionItemRepository,
    MediaClipRepository,
    ClipCollection,
    sm_db::collections::ClipCollectionItem,
    "clip_collection",
    "clip_collection_name_conflict",
    "Clip collection not found",
    "Media clip not found",
    clip_id,
    "clip_id"
);

#[cfg(test)]
mod tests {
    use super::dedup_preserving_order;

    #[test]
    fn dedup_keeps_the_first_occurrence_order() {
        // 与上游 `_normalize` 边走边滤的语义一致：保留**首次**出现的位置。
        assert_eq!(dedup_preserving_order(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
        // 而 `sorted(set(...))` 会得到 [1, 2, 3] —— 拖拽顺序被丢掉。
        assert_ne!(dedup_preserving_order(&[3, 1, 2]), vec![1, 2, 3]);
    }

    #[test]
    fn dedup_handles_empty_and_single() {
        assert!(dedup_preserving_order(&[]).is_empty());
        assert_eq!(dedup_preserving_order(&[7]), vec![7]);
        assert_eq!(dedup_preserving_order(&[7, 7, 7]), vec![7]);
    }
}
