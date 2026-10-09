//! 视频合集（`VideoCollection`）的 service，对应上游
//! `src/service/videos/video_collection_service.py`（389 行）。
//!
//! # 落地的九条规则
//!
//! | 规则 | 上游符号 | 后果 |
//! |---|---|---|
//! | 合集不存在 | `_require_collection` | 404 `video_collection_not_found`（details 键是 **`collection_id`**） |
//! | 名称重复 | `_ensure_name_available` | 409 `video_collection_name_conflict` + `{"name": …}` |
//! | 空更新 | `if not update_data` | 422 `validation_error` |
//! | 改名时才查重 | `if name != collection.name` | 名字未变不查 |
//! | 成员重复加入 | `existing is not None: return` | **幂等成功**，不改位置、不 touch |
//! | 成员不存在 | `_require_video` | 404 `video_item_not_found` |
//! | 移除不存在的成员 | `deleted == 0` | **静默成功**，且不 touch |
//! | 重排必须恰好覆盖全体 | `set(normalized) != existing_ids` | 422 `invalid_collection_reorder` |
//! | 合集封面取首位成员封面 | `_collection_cover` | **不落地**，随列表端点一起（需要 JOIN + 批量统计） |
//!
//! # `add_item` 的并发窗口
//!
//! 上游是「先查后插，撞唯一约束就 `return`」。本实现照抄：
//!
//! ```text
//! find_by_member 命中 → Ok(None)（幂等，不 touch）
//! 未命中 → 事务里 append_in + touch_in
//!          └─ 撞 UNIQUE(collection_id, video_item_id) → 也当 Ok(None)
//! ```
//!
//! 最后那一条是**必须**的：两个并发 `add_item` 都会在查完之后才插，
//! 只靠前置查询拦不住。而「已经被人加过了」不是错误 —— 幂等返回。
//!
//! # `remove_item` 用关联行 id
//!
//! 上游 `VideoCollectionItem.id == item_id`。本方法的参数名因此是
//! `item_id` 而不是 `video_item_id`，与 `remove_items_by_video_ids` 区分。
//! 两者都传错都不会报错（id 空间相同、外观相同），所以名字与文档是唯一的
//! 防线。

use sm_db::repo::{
    Ctx, NewVideoCollection, VideoCollectionItemRepository, VideoCollectionRepository,
    VideoItemRepository,
};
use sm_db::videos::{VideoCollection, VideoCollectionItem};
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::videos::Field;

/// 更新合集。两个字段各自独立，缺省表示不改动。
#[derive(Debug, Clone, Default)]
pub struct VideoCollectionUpdate {
    /// 名称。`Null` 被忽略（上游 `is not None` 才赋值）。
    pub name: Field<String>,
    /// 简介。`Null` 被忽略。`Value("")` 是合法的「清空简介」。
    pub description: Field<String>,
}

impl VideoCollectionUpdate {
    /// 是否是空更新 —— 两个字段都没给。
    pub fn is_empty(&self) -> bool {
        !self.name.is_given() && !self.description.is_given()
    }
}

/// `add_item` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// 本次真的加了一行，`position` 是它拿到的位置。
    Added { id: i32, position: i32 },
    /// 此前已是成员，什么都没做。
    AlreadyPresent,
}

/// 视频合集 service。
pub struct VideoCollectionService {
    collections: VideoCollectionRepository,
    members: VideoCollectionItemRepository,
    videos: VideoItemRepository,
    pool: Db,
}

impl VideoCollectionService {
    pub fn new(db: &Db) -> Self {
        Self {
            collections: VideoCollectionRepository::new(db.clone()),
            members: VideoCollectionItemRepository::new(db.clone()),
            videos: VideoItemRepository::new(db.clone()),
            pool: db.clone(),
        }
    }

    // ---------------------------------------------------------- 规则

    /// 取合集，不存在则 404。
    ///
    /// details 键是 `collection_id` 而不是 `video_collection_id` ——
    /// 上游显式传了 `error_details_key="collection_id"`（`require_by_id`
    /// 的默认会是 `{entity}_id`）。这个键名客户端要用，别改。
    async fn require_collection(&self, id: i32) -> Result<VideoCollection, ServiceError> {
        self.collections.find_by_id(id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "video_collection_not_found",
                "Video collection not found",
                "collection_id",
                id,
            )
        })
    }

    /// 名称归一。空白 → 422。
    ///
    /// 上游这条校验在 pydantic 层（`min_length=1` + `field_validator`），
    /// 与条目标题同一处理方式，见 [`crate::videos::item`]。
    fn normalize_name(name: &str) -> Result<String, ServiceError> {
        let normalized = name.trim();
        if normalized.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "name cannot be blank",
            ));
        }
        Ok(normalized.to_owned())
    }

    /// 名称唯一性。`exclude_id` 用于更新时排除自己。
    async fn ensure_name_available(
        &self,
        name: &str,
        exclude_id: Option<i32>,
    ) -> Result<(), ServiceError> {
        match self.collections.find_by_name(name).await? {
            Some(existing) if Some(existing.id) == exclude_id => Ok(()),
            Some(_) => Err(ServiceError::conflict(
                "video_collection_name_conflict",
                "Video collection name already exists",
                Some(details_of("name", name)),
            )),
            None => Ok(()),
        }
    }

    // ---------------------------------------------------------- 父表

    /// 新建合集。
    pub async fn create(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<VideoCollection, ServiceError> {
        let name = Self::normalize_name(name)?;
        self.ensure_name_available(&name, None).await?;
        Ok(self
            .collections
            .insert(&NewVideoCollection {
                name,
                description: description.unwrap_or_default().trim().to_owned(),
            })
            .await?)
    }

    /// 更新合集。**名字未变时跳过唯一性检查** ——
    /// 少了这个判断，「只改描述不改名字」会撞到自己而失败。
    pub async fn update(
        &self,
        collection_id: i32,
        payload: VideoCollectionUpdate,
    ) -> Result<VideoCollection, ServiceError> {
        let current = self.require_collection(collection_id).await?;
        if payload.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "At least one field must be provided",
            ));
        }
        if let Some(name) = payload.name.as_value() {
            let name = Self::normalize_name(name)?;
            if name != current.name {
                self.ensure_name_available(&name, Some(collection_id))
                    .await?;
            }
        }
        self.collections
            .update(
                collection_id,
                payload.name.as_value().map(String::as_str),
                payload.description.as_value().map(String::as_str),
            )
            .await?;
        // 返回更新后的行，而不是复用 `current`：上游 `update_collection`
        // 结尾是 `return cls.get_collection(collection.id)`，读的是库。
        self.require_collection(collection_id).await
    }

    /// 删除合集。**成员随外键 `CASCADE` 消失，视频条目本身保留。**
    pub async fn delete(&self, collection_id: i32) -> Result<(), ServiceError> {
        self.require_collection(collection_id).await?;
        self.collections.delete(collection_id).await?;
        Ok(())
    }

    // ---------------------------------------------------------- 成员

    /// 加入一个视频。**已是成员则幂等返回**（不改位置、不 touch）。
    pub async fn add_item(
        &self,
        collection_id: i32,
        video_item_id: i32,
    ) -> Result<Added, ServiceError> {
        self.require_collection(collection_id).await?;
        if self.videos.find_by_id(video_item_id).await?.is_none() {
            return Err(ServiceError::not_found(
                "video_item_not_found",
                "Video item not found",
                "video_item_id",
                video_item_id,
            ));
        }
        if self
            .members
            .find_by_member(collection_id, video_item_id)
            .await?
            .is_some()
        {
            return Ok(Added::AlreadyPresent);
        }

        let mut tx = self.pool.begin().await?;
        let appended = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            match self
                .members
                .append_in(&mut ctx, collection_id, video_item_id)
                .await
            {
                Ok(link) => {
                    self.collections.touch_in(&mut ctx, collection_id).await?;
                    link
                }
                // 与上面那次查询并发：唯一索引兜住了。语义上仍然是
                // 「已经加过了」，所以幂等而不是 409。
                Err(err) if is_unique_violation(&err) => return Ok(Added::AlreadyPresent),
                Err(err) => return Err(err.into()),
            }
        };
        tx.commit().await?;
        Ok(Added::Added {
            id: appended.id,
            position: appended.position,
        })
    }

    /// 移除一个成员，按**关联行 id**。
    ///
    /// **不存在的 `item_id` 静默成功**，且不推进合集时间 —— 上游就是这个
    /// 行为：结果状态（「这个成员不在合集里」）已经达成，报错没有意义。
    pub async fn remove_item(&self, collection_id: i32, item_id: i32) -> Result<(), ServiceError> {
        self.require_collection(collection_id).await?;
        if self
            .members
            .unlink_by_link_id(collection_id, item_id)
            .await?
        {
            self.collections.touch(collection_id).await?;
        }
        Ok(())
    }

    /// 按视频 id 批量移除成员。不是成员的静默跳过。
    ///
    /// 重复的 `video_item_id` 不必先去重：`= ANY($2)` 对重复值不敏感。
    pub async fn remove_items_by_video_ids(
        &self,
        collection_id: i32,
        video_ids: &[i32],
    ) -> Result<(), ServiceError> {
        self.require_collection(collection_id).await?;
        if video_ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            let deleted = self
                .members
                .unlink_by_member_ids_in(&mut ctx, collection_id, video_ids)
                .await?;
            // 只有真删掉行才 touch —— 否则「移除一批本来就不在里面的视频」
            // 会把合集顶到「最近活跃」最前面。
            if deleted > 0 {
                self.collections.touch_in(&mut ctx, collection_id).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// 重排成员顺序。返回重排后的全部成员。
    ///
    /// # 为什么要「恰好覆盖全体」而不是「覆盖即可」
    ///
    /// 上游的校验是 `set(normalized) != existing_ids` —— **多一个少一个都
    /// 拒绝**。宽松版（「只处理给出的那几个」）会让漏排的成员停在旧位置，
    /// 而前端已经按新顺序播放了。严格版把这个错误挡在服务层，返回
    /// 422 `invalid_collection_reorder` 并带上 `collection_id`。
    ///
    /// 重复的 `item_id` 先按首次出现去重（与 `collections` 域的
    /// `set_members` 同一套做法），否则 `[1, 1, 2]` 会被去重成 `[1, 2]`
    /// 再与全体比较 —— 集合相等，但顺序里的重复是调用方的 bug。
    pub async fn reorder(
        &self,
        collection_id: i32,
        ordered_item_ids: &[i32],
    ) -> Result<Vec<VideoCollectionItem>, ServiceError> {
        self.require_collection(collection_id).await?;
        // 上游 `VideoCollectionReorderRequest.ordered_item_ids` 是
        // `Field(min_length=1)`，空列表在 pydantic 层就被拒（422
        // `validation_error`），到不了这里。
        if ordered_item_ids.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "ordered_item_ids must contain at least one item",
            ));
        }
        let ordered = dedup_preserving_order(ordered_item_ids);
        let existing = self.members.list_by_collection(collection_id).await?;
        let covers_all = existing.len() == ordered.len()
            && existing.iter().all(|link| ordered.contains(&link.id));
        if !covers_all {
            return Err(ServiceError::validation_with(
                "invalid_collection_reorder",
                "ordered_item_ids must cover exactly all collection items",
                details_of("collection_id", collection_id),
            ));
        }

        let mut tx = self.pool.begin().await?;
        {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            for (position, link_id) in ordered.iter().enumerate() {
                if !self
                    .members
                    .set_position_in(&mut ctx, collection_id, *link_id, position as i32)
                    .await?
                {
                    // 覆盖校验刚过，行就没了：只能是被并发删掉。
                    // 上游 `get_by_id` 抛 Peewee 的 DoesNotExist，不是
                    // ApiError，所以那也是 500 —— 这里保持一致。
                    return Err(ServiceError::from(sm_db::DbError::business(
                        "VideoCollectionItem",
                        "重排过程中成员行消失",
                    )));
                }
            }
            self.collections.touch_in(&mut ctx, collection_id).await?;
        }
        tx.commit().await?;
        Ok(self.members.list_by_collection(collection_id).await?)
    }

    // ---------------------------------------------------------- 读

    /// 取合集。不存在则 404。
    ///
    /// 上游 `get_collection` 还要回填 `item_count` 与 `cover_image`，那两个
    /// 需要跨表聚合（`count_by_owner` + 按 position 取首位成员的封面），
    /// 随列表端点一起做，见 [`crate::videos`]。
    pub async fn get(&self, collection_id: i32) -> Result<VideoCollection, ServiceError> {
        self.require_collection(collection_id).await
    }

    /// 按播放顺序列出全部成员。**刻意不分页** —— 拖拽编辑页要的是全量顺序。
    ///
    /// 面向连播页的分页版本（`list_collection_items`，上游为了万级成员合集
    /// 专门加的）随列表端点一起做，见 [`crate::videos`] 的「不落查询编排」。
    pub async fn list_items(
        &self,
        collection_id: i32,
    ) -> Result<Vec<VideoCollectionItem>, ServiceError> {
        self.require_collection(collection_id).await?;
        Ok(self.members.list_by_collection(collection_id).await?)
    }

    /// 批量清空成员。**保留合集本身** —— 编辑页「全选取消」的操作。
    ///
    /// 上游没有这个方法（`moment` / `clip` 域的 `set_members([])` 走的是
    /// 「清空 + 按序插入」，空列表即清空）。这里显式提供，因为
    /// `replace_all` 的语义是「成员就是这些」，与「保留壳子」不是一回事。
    pub async fn clear_items(&self, collection_id: i32) -> Result<u64, ServiceError> {
        self.require_collection(collection_id).await?;
        let removed = self.members.clear(collection_id).await?;
        if removed > 0 {
            self.collections.touch(collection_id).await?;
        }
        Ok(removed)
    }
}

/// 按首次出现去重，保留顺序。
///
/// 拖拽排序的结果必须原样保留，而 `HashSet` 是无序的 —— 所以不能用
/// `sorted(set(...))`。
fn dedup_preserving_order(ids: &[i32]) -> Vec<i32> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

/// 是不是唯一约束违例（PostgreSQL `23505`）。
///
/// 走 [`sm_db::DbError::is_unique_violation`]：只认 SQLSTATE，不看约束名。
/// `add_item` 撞的约束只可能是 `UNIQUE(collection_id, video_item_id)`，
/// 而约束名会随上游 DDL 调整 —— 绑死名字会让这条幂等路径静默失效。
/// 反过来也不能只看 `ConstraintViolation` 变体：外键与 CHECK 违例同样落在
/// 里面，把它们当「已存在」跳过会掩盖真实缺陷。
fn is_unique_violation(err: &sm_db::DbError) -> bool {
    err.is_unique_violation()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_the_first_occurrence_order() {
        assert_eq!(dedup_preserving_order(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
        // 而 `sorted(set(...))` 会得到 [1, 2, 3] —— 拖拽顺序被丢掉。
        assert_ne!(dedup_preserving_order(&[3, 1, 2]), vec![1, 2, 3]);
        assert!(dedup_preserving_order(&[]).is_empty());
    }

    #[test]
    fn only_unique_violations_are_treated_as_idempotent() {
        let unique = sm_db::DbError::ConstraintViolation {
            entity: "VideoCollectionItem",
            code: sm_db::error::UNIQUE_VIOLATION,
            constraint: "video_collection_item_collection_id_video_item_id_key".to_owned(),
        };
        assert!(is_unique_violation(&unique));

        // 外键 / CHECK 违例必须原样冒泡 —— 把它们当幂等会掩盖真实缺陷。
        for code in [
            sm_db::error::FOREIGN_KEY_VIOLATION,
            sm_db::error::CHECK_VIOLATION,
        ] {
            let other = sm_db::DbError::ConstraintViolation {
                entity: "VideoCollectionItem",
                code,
                constraint: "video_collection_item_collection_id_fkey".to_owned(),
            };
            assert!(!is_unique_violation(&other), "{code} 不是唯一违例");
        }
        assert!(!is_unique_violation(&sm_db::DbError::business(
            "VideoCollectionItem",
            "x"
        )));
    }

    #[test]
    fn blank_names_are_rejected_with_the_upstream_message() {
        for blank in ["", "  ", "\t"] {
            let err = VideoCollectionService::normalize_name(blank).unwrap_err();
            assert_eq!((err.status, err.code()), (422, "validation_error"));
            assert_eq!(err.api.message, "name cannot be blank");
        }
    }

    #[test]
    fn a_null_only_update_is_not_an_empty_update() {
        let only_null = VideoCollectionUpdate {
            description: Field::Null,
            ..Default::default()
        };
        assert!(!only_null.is_empty());
        assert!(VideoCollectionUpdate::default().is_empty());
    }
}
