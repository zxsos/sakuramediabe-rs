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
        $member_key:literal,
        $member_entity:literal
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
            ///
            /// # 错误码用 `$member_entity`，**不是** `$member_key`
            ///
            /// 上游 `require_by_id(MediaClip, id, "media_clip",
            /// error_details_key="clip_id")` 的规则是：默认码由**实体名**生成
            /// （`{entity}_not_found`），而 `error_details_key` 只管详情键。
            /// 两者是不同字符串 —— 码是 `media_clip_not_found`，详情键是
            /// `clip_id`。
            ///
            /// 此前这里用 `$member_key` 拼码，于是产生 `clip_id_not_found`
            /// （moment 侧同病：`point_id_not_found`）。客户端按 `code` 分支，
            /// 所以那是一个**可见的契约偏差**，而不是文案差异。
            async fn require_member(&self, id: i32) -> Result<(), ServiceError> {
                if self.origin.find_by_id(id).await?.is_some() {
                    Ok(())
                } else {
                    Err(ServiceError::not_found(
                        concat!($member_entity, "_not_found"),
                        $member_missing,
                        $member_key,
                        id,
                    ))
                }
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
            /// 幂等加入一个成员。**已在合集里就直接返回，不改位置。**
            ///
            /// # 用 `find_by_member` 而不是「拉全量再扫」
            ///
            /// 「重复加入要幂等」这条规则要求**先查后插**，而
            /// `list_by_collection` 是把整个合集读出来在内存里扫 ——
            /// 复杂度 O(合集规模)，每次加人都重来一遍。万级成员的合集在上游
            /// 是真实存在的（`moment_collection_item` 同理），所以那不是理论问题。
            ///
            /// `find_by_member` 走唯一索引 `(collection_id, <成员外键>)`，
            /// 是 O(1) 点查。仓储层那个方法的文档本来就写明它是为这条规则存在的。
            pub async fn add(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<(), ServiceError> {
                self.require_collection(collection_id).await?;
                self.require_member(member_id).await?;
                if self
                    .members
                    .find_by_member(collection_id, member_id)
                    .await?
                    .is_some()
                {
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
                let outcome = async {
                    let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, &self.pool);
                    self.members.clear_in(&mut ctx, collection_id).await?;
                    for (position, id) in ordered.iter().enumerate() {
                        self.members
                            .insert_at_in(&mut ctx, collection_id, *id, position as i32)
                            .await?;
                    }
                    Ok::<(), ServiceError>(())
                }
                .await;
                sm_db::repo::commit_or_rollback(tx, outcome).await?;
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
    "point_id",
    "media_point"
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
    "clip_id",
    "media_clip"
);

/// 片段合集所有 422 的错误码。与上游
/// `validate_page(..., error_code="invalid_clip_collection_filter")` 一致。
///
/// 刻意不用 `sm_core::pagination` 的默认码（`invalid_page` /
/// `invalid_page_size`）—— 那是别的域的契约，客户端按 `code` 分支。
pub const INVALID_CLIP_COLLECTION_FILTER: &str = "invalid_clip_collection_filter";

/// 一个成员行加上它所属的片段。
///
/// 宏生成的 `list_members` 只返回 `ClipCollectionItem`，而合集列表的
/// `clip_count` 与封面都需要片段本体（要 `movie_number`、`file_path`、以及
/// 封面解析用的 `start_offset_seconds`）。
///
/// **不是表镜像**，所以不声明成 `pub struct` + `FromRow` —— 那在本仓库意味着
/// 「我映射一张表」，schema 对拍会要求一个上游 Peewee 模型（见
/// `sm_db::repo::movie::MovieResolutionLevelRow` 的文档：投影行用元组，
/// 具名类型放 service）。
/// **刻意不派生 `PartialEq`**
///
/// 与 `sm_db::repo::ClaimedTask` 同一个理由：两个内含数据库行的结构体
/// 「相等」不是一个有意义的问题。要断言就断言 `item.id` 或 `clip.id`。
#[derive(Debug, Clone)]
pub struct MemberWithClip {
    pub item: sm_db::collections::ClipCollectionItem,
    pub clip: sm_db::playback::media::MediaClip,
}

/// 合集连同它的**有效**成员数。
#[derive(Debug, Clone)]
pub struct CollectionWithCount {
    pub collection: ClipCollection,
    /// **只数产物有效的成员。** 见 [`ClipCollectionService::valid_members`]。
    pub clip_count: i32,
}

/// 片段合集专属的读路径。
///
/// # 为什么不放进 `impl_ordered_collection_service!`
///
/// 宏是 `MomentCollectionService` 与 `ClipCollectionService` 共用的，而
/// 「成员是否有效」只有片段合集有 —— 时刻点没有产物文件。所以有效性判定与
/// 回收只能落在这个**只针对 clip 的 impl 块**里。
///
/// 它能访问 `parent` / `members` / `origin` / `pool` 这些私有字段，是因为
/// 与宏展开处在**同一个模块**。
///
/// # 合集的读写**也会回收失效片段**
///
/// 上游每个读路径都先过 `_valid_collection_items`，而它内部调
/// `MediaClipService.valid_clips` —— 与片段列表端点同一套判定。因此：
///
/// - `clip_count` 只数**产物仍然存在**的成员；
/// - 合集封面取「按 position 排最前**且有效**」那个片段的封面；
/// - 顺带把失效成员从库里删掉、把文件删掉。
///
/// 这不是「顺手清理」，而是 `clip_count` 口径的一部分：客户端按它决定要不要
/// 显示数字，而一个点开播不出来的片段不该被计数。
impl ClipCollectionService {
    /// 确认合集存在，否则 404 `clip_collection`。
    ///
    /// 宏里的 `require_collection` 是私有的，而 `list_clips_paged` 之前需要
    /// 先做存在性校验 —— 上游 `_require_collection` 在 `validate_page`
    /// **之前**，所以「合集不存在」不能被报成分页错误。
    pub async fn require(&self, collection_id: i32) -> Result<ClipCollection, ServiceError> {
        self.require_collection(collection_id).await
    }

    /// 取**一个**合集，带它的有效成员数。
    ///
    /// 刻意不借用 `list_collections` 再筛 —— 那会把**所有**合集的成员都拉出来
    /// 判有效性，而这里只关心一个。
    pub async fn get_with_count(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
        collection_id: i32,
    ) -> Result<CollectionWithCount, ServiceError> {
        let collection = self.require_collection(collection_id).await?;
        let clip_count = i32::try_from(self.valid_members(media, &[collection_id]).await?.len())
            .unwrap_or(i32::MAX);
        Ok(CollectionWithCount {
            collection,
            clip_count,
        })
    }

    /// 列出**给定这些合集**的**有效**成员，按 `(collection_id, position, id)` 排。
    ///
    /// 对应上游 `_valid_collection_items`。**顺带回收失效片段**。
    ///
    /// 两次批量查询而不是 N+1：成员行一次拿全，片段按去重后的 `clip_id`
    /// 一次拿全 —— 同一片段可能加入多个合集，所以要先按 `clip_id` 去重。
    pub async fn valid_members(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
        collection_ids: &[i32],
    ) -> Result<Vec<MemberWithClip>, ServiceError> {
        use std::collections::{HashMap, HashSet};

        if collection_ids.is_empty() {
            return Ok(Vec::new());
        }
        let items = self.members.list_by_collections(collection_ids).await?;

        let mut wanted: Vec<i32> = items.iter().map(|item| item.clip_id).collect();
        wanted.sort_unstable();
        wanted.dedup();

        let mut clips: HashMap<i32, sm_db::playback::media::MediaClip> = HashMap::new();
        let mut candidates = Vec::with_capacity(wanted.len());
        for clip_id in wanted {
            if let Some(clip) = self.origin.find_by_id(clip_id).await? {
                clips.insert(clip_id, clip.clone());
                candidates.push(clip);
            }
        }

        // 有效性判定 + 回收，与片段列表端点**同一个函数**。
        let (valid, _reclaimed) = media.retain_valid(candidates).await?;
        let valid_ids: HashSet<i32> = valid.into_iter().map(|clip| clip.id).collect();

        let mut members: Vec<MemberWithClip> = items
            .into_iter()
            .filter_map(|item| {
                let clip = clips.get(&item.clip_id)?;
                valid_ids.contains(&clip.id).then(|| MemberWithClip {
                    item,
                    clip: clip.clone(),
                })
            })
            .collect();
        // 跨合集时 `position` 各自独立，所以先按合集分组、组内再按位置 ——
        // 上游 `sort(key=(position, id))` 只在单合集时才有意义。
        members.sort_by(|a, b| {
            a.item
                .collection_id
                .cmp(&b.item.collection_id)
                .then(a.item.position.cmp(&b.item.position))
                .then(a.item.id.cmp(&b.item.id))
        });
        Ok(members)
    }

    /// 列出全部合集，按 `updated_at DESC, id DESC`，各带**有效**成员数。
    pub async fn list_collections(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
    ) -> Result<Vec<CollectionWithCount>, ServiceError> {
        let collections = self.parent.list_ordered_by_recency().await?;
        let ids: Vec<i32> = collections.iter().map(|row| row.id).collect();

        let mut counts: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
        for member in self.valid_members(media, &ids).await? {
            *counts.entry(member.item.collection_id).or_insert(0) += 1;
        }

        Ok(collections
            .into_iter()
            .map(|collection| {
                let clip_count = counts.get(&collection.id).copied().unwrap_or(0);
                CollectionWithCount {
                    collection,
                    clip_count,
                }
            })
            .collect())
    }

    /// 一个合集的**有效**成员数。
    pub async fn valid_clip_count(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
        collection_id: i32,
    ) -> Result<i32, ServiceError> {
        Ok(
            i32::try_from(self.valid_members(media, &[collection_id]).await?.len())
                .unwrap_or(i32::MAX),
        )
    }

    /// 合集封面：按 position 排最前的**有效**成员的封面。
    ///
    /// 对应上游 `_collection_cover`。**没有有效成员时返回 `None`**。
    ///
    /// 孤立片段（`media_id` 为空）也返回 `None` —— `load_cover_map` 会跳过
    /// 它们，所以没有键可查。
    pub async fn cover(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
        collection_id: i32,
    ) -> Result<Option<sm_db::catalog::asset::Image>, ServiceError> {
        let members = self.valid_members(media, &[collection_id]).await?;
        let Some(first) = members.first() else {
            return Ok(None);
        };
        let Some(media_id) = first.clip.media_id else {
            return Ok(None);
        };
        let covers = media
            .load_cover_map(std::slice::from_ref(&first.clip))
            .await?;
        Ok(covers
            .get(&(media_id, first.clip.start_offset_seconds))
            .cloned())
    }

    /// 合集成员分页。**先过滤回收，再计数，再切片** —— 与片段列表同序。
    ///
    /// 返回 `(本页成员, 过滤后的总数)`。切在内存里做，因为有效性判定要看
    /// 文件系统，数据库不知道。
    pub async fn list_clips_paged(
        &self,
        media: &crate::playback::media_clip::MediaClipService,
        collection_id: i32,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<MemberWithClip>, i64), ServiceError> {
        // 分页校验与上游同一个错误码。
        if page <= 0 {
            return Err(ServiceError::validation_with(
                INVALID_CLIP_COLLECTION_FILTER,
                "page must be greater than 0",
                crate::error::details_of("page", page),
            ));
        }
        if page_size <= 0 || page_size > 100 {
            return Err(ServiceError::validation_with(
                INVALID_CLIP_COLLECTION_FILTER,
                "page_size must be between 1 and 100",
                crate::error::details_of("page_size", page_size),
            ));
        }

        let members = self.valid_members(media, &[collection_id]).await?;
        let total = i64::try_from(members.len()).unwrap_or(i64::MAX);
        let start = usize::try_from(sm_core::pagination::page_offset(page, page_size))
            .unwrap_or(usize::MAX)
            .min(members.len());
        let end = start
            .saturating_add(usize::try_from(page_size).unwrap_or(usize::MAX))
            .min(members.len());
        Ok((members[start..end].to_vec(), total))
    }
}

/// 时刻合集所有 422 的错误码。与上游
/// `validate_page(..., error_code="invalid_moment_collection_filter")` 一致。
///
/// 与 [`INVALID_CLIP_COLLECTION_FILTER`] 是**两个**码：客户端按 `code` 分支，
/// 合并会让「哪个域的筛选条件有问题」这条信息丢失。
pub const INVALID_MOMENT_COLLECTION_FILTER: &str = "invalid_moment_collection_filter";

/// 时刻合集连同它的成员数与封面。
///
/// # `point_count` 的口径与 [`CollectionWithCount::clip_count`] **不同**
///
/// 片段有产物文件、可能失效，所以 `clip_count` 只数**有效**成员；时刻点没有
/// 产物，`point_count` 就是全部成员。照抄 clip 侧的 `valid_members` 会把
/// 成员数算成 0（时刻点根本没有 `retain_valid` 这条路径）。
#[derive(Debug, Clone)]
pub struct MomentCollectionWithCount {
    pub collection: MomentCollection,
    /// 成员数（**全部**成员，不过滤有效性）。
    pub point_count: i32,
    /// 封面：按 `(position, id)` 排最前那个成员的点位图。空合集为 `None`。
    pub cover: Option<sm_db::catalog::asset::Image>,
}

/// 一个时刻合集成员，连同它的点位与图片。
///
/// 三张表（`moment_collection_item` / `media_point` / `image`）在 **Rust 侧
/// 拼**，不是一次三表 JOIN —— `sqlx` 的元组 `FromRow` 按位置解码、要求每个
/// 元素是 `Decode`，而这里三个都是具名模型（实现的是 `FromRow`）。
/// 分三次 `= ANY($1)` 批量查既避开重复列名，也避开 N+1。
#[derive(Debug, Clone)]
pub struct MomentPointWithImage {
    pub item: sm_db::collections::MomentCollectionItem,
    pub point: sm_db::playback::media::MediaPoint,
    pub image: sm_db::catalog::asset::Image,
}

/// 分页校验。违规 → 422 [`INVALID_MOMENT_COLLECTION_FILTER`]。
///
/// 与 [`validate_page`]（videos 域）的区别**只有错误码** —— 上游时刻合集
/// 那一处传的是 `error_code="invalid_moment_collection_filter"`。
fn validate_moment_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
    sm_core::pagination::validate_page(page, page_size).map_err(|err| {
        let details = match err.details() {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("page".to_owned(), other);
                map
            }
        };
        ServiceError::validation_with(INVALID_MOMENT_COLLECTION_FILTER, err.message(), details)
    })
}

/// 时刻合集专属的读路径。
///
/// # 为什么不放进 `impl_ordered_collection_service!`
///
/// 宏的 `list_members` 只返回成员行，而列表与详情还要**成员数**与**封面**
/// —— 封面要 `media_point.image_id`，所以那两件事都要点位本体。宏是
/// moment 与 clip 共用的，而 clip 的有效性判定只有 clip 有（见上）。
impl MomentCollectionService {
    /// 确认合集存在，否则 404 `moment_collection`。
    ///
    /// 宏里的 `require_collection` 是私有的，而点位列表端点要先做存在性校验
    /// —— 上游 `_require_collection` 在 `validate_page` **之前**，所以
    /// 「合集不存在」不能被报成分页错误。
    pub async fn require(&self, collection_id: i32) -> Result<MomentCollection, ServiceError> {
        self.require_collection(collection_id).await
    }

    fn images(&self) -> sm_db::repo::ImageRepository {
        sm_db::repo::ImageRepository::new(self.pool.clone())
    }

    /// 一批合集的**成员数**与**封面**。
    ///
    /// 三次查询而不是「每个合集三次」：成员行一次（`list_by_collections`）、
    /// 点位一次（去重后的 `point_id`）、图片一次（去重后的 `image_id`）。
    ///
    /// **只取每个合集的首个成员**作为封面判据，而不是把整批成员的点位都拉
    /// 回来 —— 一个合集可能有上千成员，而封面只看第一个。
    async fn counts_and_covers(
        &self,
        collection_ids: &[i32],
    ) -> Result<
        std::collections::HashMap<i32, (i32, Option<sm_db::catalog::asset::Image>)>,
        ServiceError,
    > {
        use std::collections::HashMap;

        if collection_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let items = self.members.list_by_collections(collection_ids).await?;

        // `list_by_collections` 只保证 `collection_id` 有序，**组内不排序**
        // （跨合集时 `position` 没有可比性，仓储层注释写明了）。而封面的判据
        // 就是组内的 `(position, id)` 最小者，所以这里自己扫一遍取最小。
        //
        // 顺手把成员数也数出来 —— 同一圈里两件事，不再多遍历一次。
        let mut best: HashMap<i32, (i32, i32, i32)> = HashMap::new();
        let mut counts: HashMap<i32, i32> = HashMap::new();
        for item in &items {
            let (position, id) = item.playback_order_key();
            *counts.entry(item.collection_id).or_insert(0) += 1;
            best.entry(item.collection_id)
                .and_modify(|slot| {
                    if (position, id) < (slot.1, slot.2) {
                        *slot = (item.point_id, position, id);
                    }
                })
                .or_insert((item.point_id, position, id));
        }

        let mut point_ids: Vec<i32> = best.values().map(|slot| slot.0).collect();
        point_ids.sort_unstable();
        point_ids.dedup();
        let points = self.origin.find_by_ids(&point_ids).await?;

        let mut image_ids: Vec<i32> = point_ids
            .iter()
            .filter_map(|id| points.get(id).map(|point| point.image_id))
            .collect();
        image_ids.sort_unstable();
        image_ids.dedup();
        let images = self.images().find_by_ids(&image_ids).await?;

        Ok(collection_ids
            .iter()
            .map(|id| {
                let cover = best
                    .get(id)
                    .and_then(|slot| points.get(&slot.0))
                    .and_then(|point| images.get(&point.image_id))
                    .cloned();
                (*id, (counts.get(id).copied().unwrap_or(0), cover))
            })
            .collect())
    }

    /// 一个合集 + 成员数 + 封面。
    ///
    /// 刻意只查这一个合集，不借用 [`Self::list_collections`] 再筛 ——
    /// 那会把**所有**合集的成员都拉出来。
    pub async fn get_with_count(
        &self,
        collection_id: i32,
    ) -> Result<MomentCollectionWithCount, ServiceError> {
        let collection = self.require_collection(collection_id).await?;
        let mut stats = self.counts_and_covers(&[collection_id]).await?;
        let (point_count, cover) = stats.remove(&collection_id).unwrap_or((0, None));
        Ok(MomentCollectionWithCount {
            collection,
            point_count,
            cover,
        })
    }

    /// 全部合集 + 成员数 + 封面，按 `updated_at DESC, id DESC`。
    pub async fn list_collections(&self) -> Result<Vec<MomentCollectionWithCount>, ServiceError> {
        let collections = self.parent.list_ordered_by_recency().await?;
        let ids: Vec<i32> = collections.iter().map(|row| row.id).collect();
        let mut stats = self.counts_and_covers(&ids).await?;
        Ok(collections
            .into_iter()
            .map(|collection| {
                let (point_count, cover) = stats.remove(&collection.id).unwrap_or((0, None));
                MomentCollectionWithCount {
                    collection,
                    point_count,
                    cover,
                }
            })
            .collect())
    }

    /// 成员分页。返回 `(本页成员连同点位与图片, 总数)`。
    ///
    /// # 排序与切片
    ///
    /// `list_by_collection` 已经是 `ORDER BY position, id`，与上游
    /// `.order_by(position, id)` 同序。切片在**内存**里做，而上游用 SQL 的
    /// `OFFSET/LIMIT` —— 两者结果一致（时刻点没有有效性过滤，`total` 就是
    /// 行数），差别只在超大合集上多读了几行。合集是用户手建的，长度有界。
    ///
    /// # 点位或图片查不到的行会被跳过
    ///
    /// 外键保证它们存在；真丢了（并发删）就当这行不存在。上游那条
    /// `JOIN MediaPoint JOIN Image` 同样会把它漏掉，而 `total` 仍然按
    /// 成员行数算 —— 两边一致。
    pub async fn list_points_paged(
        &self,
        collection_id: i32,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<MomentPointWithImage>, i64), ServiceError> {
        self.require_collection(collection_id).await?;
        validate_moment_page(page, page_size)?;

        let items = self.members.list_by_collection(collection_id).await?;
        let total = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let start = usize::try_from(sm_core::pagination::page_offset(page, page_size))
            .unwrap_or(usize::MAX)
            .min(items.len());
        let end = start
            .saturating_add(usize::try_from(page_size).unwrap_or(usize::MAX))
            .min(items.len());
        let page_items = &items[start..end];

        let mut point_ids: Vec<i32> = page_items.iter().map(|item| item.point_id).collect();
        point_ids.sort_unstable();
        point_ids.dedup();
        let points = self.origin.find_by_ids(&point_ids).await?;

        let mut image_ids: Vec<i32> = point_ids
            .iter()
            .filter_map(|id| points.get(id).map(|point| point.image_id))
            .collect();
        image_ids.sort_unstable();
        image_ids.dedup();
        let images = self.images().find_by_ids(&image_ids).await?;

        let mut out = Vec::with_capacity(page_items.len());
        for item in page_items {
            let Some(point) = points.get(&item.point_id) else {
                continue;
            };
            let Some(image) = images.get(&point.image_id) else {
                continue;
            };
            out.push(MomentPointWithImage {
                item: item.clone(),
                point: point.clone(),
                image: image.clone(),
            });
        }
        Ok((out, total))
    }
}

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
