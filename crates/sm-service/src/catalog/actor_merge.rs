//! 演员合并，对应上游 `src/service/catalog/actor_merge_service.py`（208 行）。
//!
//! # 语义
//!
//! 把「来源」演员归并到「保留」记录，来源退化为**墓碑指针**
//! （`merged_into_id = target`）。演员**不被删除**。
//!
//! 只允许人工显式触发。合并后所有查询/写入通过 `_require_actor` 的
//! 「跳一跳」收敛到保留记录（见 [`super::actor`]）。
//!
//! # 整体在一个事务里
//!
//! 上游是 `with get_database().atomic():`。Rust 侧没有隐式事务，所以这里
//! 显式 `pool.begin()` + [`Ctx::in_tx`]，把六步与两次 `FOR UPDATE` 加锁
//! 圈在同一个事务里。任何一步失败都会连带回滚 —— 否则会留下「影片关联搬了
//! 一半」或「墓碑打了但来源订阅没清」这种**半合并**状态，且没有任何机制
//! 会发现。
//!
//! # 六步（顺序不能换）
//!
//! 1. **搬影片关联**：`INSERT ... SELECT ... ON CONFLICT DO NOTHING` 再删来源行。
//!    先插后删；顺序反了会先丢关联。
//! 2. **合并别名**：来源的主名 / 别名 / 显示名覆盖都并进别名串。
//! 3. **合并订阅**：任一来源已订阅则目标也订阅，`subscribed_at` 取**最早**。
//! 4. **填空受保护字段**：目标为空的字段由**第一个**非空来源补上；跳过人工
//!    归属（`host:manual`）的来源字段。
//! 5. **搬头像**：目标没有任何头像时，取第一个有覆盖 / 头像的来源搬过来。
//! 6. **打墓碑并压平链**：来源指向目标，且原本指向来源的墓碑一并重指向目标。
//!
//! 头像若被搬走，来源行上那一列要**清掉**（第 8 步的收尾）—— 不然同一个
//! 头像会被两个演员引用，删图时引用计数会多算。
//!
//! # 两个 422 的分叉点
//!
//! - `merge_self`：把演员合并到自身；
//! - `source_already_merged`：来源已合并到**别的**演员。
//!
//! 而「来源已合并到**本**目标」不是错误 —— 那是幂等的重复提交，跳过即可。
//! 这条分叉很容易写反（把两种情况都当错误，于是重放一次合并请求就 422）。

use chrono::NaiveDateTime;
use serde_json::{Map as JsonMap, Value as Json};
use sm_db::catalog::actor::{merge_alias_name, split_alias_name, Actor, PROTECTED_ACTOR_FIELDS};
use sm_db::common::time::now_utc;
use sm_db::repo::actor::{ActorRepository, ActorUpdate};
use sm_db::repo::{commit_or_rollback, Ctx};
use sm_db::Db;

use super::actor::{ActorService, ActorView, ACTOR_NOT_FOUND, MANUAL_FIELD_OWNER};
use crate::error::ServiceError;

/// 合并类校验错误码。上游 `ActorMergeService` 的两个 422 共用它。
pub const INVALID_ACTOR_MERGE: &str = "invalid_actor_merge";

/// 演员合并 service。
#[derive(Debug, Clone)]
pub struct ActorMergeService {
    pool: Db,
}

impl ActorMergeService {
    pub fn new(db: &Db) -> Self {
        Self { pool: db.clone() }
    }

    /// `POST /actors/{id}/merge`。
    ///
    /// 返回合并后的**保留记录**详情（不是来源）。详情在事务**提交之后**
    /// 重新查 —— 与上游 `return ActorService.get_actor_detail(target.id)`
    /// 在 `atomic()` 之外一致。
    pub async fn merge_actors(
        &self,
        target_actor_id: i32,
        source_actor_ids: &[i32],
    ) -> Result<ActorView, ServiceError> {
        let mut tx = self.pool.begin().await.map_err(ServiceError::from)?;
        let outcome = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            self.resolve_and_merge(&mut ctx, target_actor_id, source_actor_ids)
                .await
        };
        // **显式**回滚，不依赖 `Transaction` 的 `Drop` —— 理由见
        // [`commit_or_rollback`]（这个坑正是本模块的集成测试踩出来的）。
        let target_id = commit_or_rollback(tx, outcome).await?;
        ActorService::new(&self.pool).detail(target_id).await
    }

    /// 加锁、分类来源、必要时执行合并。返回保留记录的 id。
    ///
    /// 返回的 id 可能**不等于** `target_actor_id`：目标是墓碑时跳一跳
    /// （上游 `if target.merged_into_id is not None: target = _locked_actor(...)`）。
    async fn resolve_and_merge(
        &self,
        ctx: &mut Ctx<'_>,
        target_actor_id: i32,
        source_actor_ids: &[i32],
    ) -> Result<i32, ServiceError> {
        let actors = ActorRepository::new(self.pool.clone());

        // `list(dict.fromkeys(...))`：去重且保留首次出现顺序。
        let mut source_ids: Vec<i32> = Vec::new();
        for id in source_actor_ids {
            if !source_ids.contains(id) {
                source_ids.push(*id);
            }
        }

        // 目标先上锁，再跳一跳。
        let mut target = actors
            .lock_in(ctx, target_actor_id)
            .await?
            .ok_or_else(|| actor_not_found(target_actor_id))?;
        if let Some(next) = target.merged_into_id {
            target = actors
                .lock_in(ctx, next)
                .await?
                .ok_or_else(|| actor_not_found(next))?;
        }

        // 逐个来源上锁并分类。**顺序敏感**：先判自身、再判是否已指向本目标、
        // 最后才是「已合并到别的演员」。
        let mut active: Vec<Actor> = Vec::new();
        for id in &source_ids {
            let source = actors
                .lock_in(ctx, *id)
                .await?
                .ok_or_else(|| actor_not_found(*id))?;
            if source.id == target.id {
                return Err(merge_self(source.id));
            }
            match source.merged_into_id {
                None => active.push(source),
                // 已经合并到本目标：幂等跳过，不参与本次合并。
                Some(merged) if merged == target.id => continue,
                Some(_) => return Err(source_already_merged(source.id)),
            }
        }

        if !active.is_empty() {
            self.apply_merge(ctx, &actors, &target, &active).await?;
        }
        Ok(target.id)
    }

    /// 六步合并的落库实现。整个方法共享调用方的 `ctx`（同一个事务）。
    async fn apply_merge(
        &self,
        ctx: &mut Ctx<'_>,
        actors: &ActorRepository,
        target: &Actor,
        sources: &[Actor],
    ) -> Result<(), ServiceError> {
        let source_ids: Vec<i32> = sources.iter().map(|source| source.id).collect();

        // ① 搬影片关联：同一部影片两边都有时按 (movie, actor) 唯一约束去重。
        actors
            .move_movie_links_in(ctx, target.id, &source_ids)
            .await?;
        actors.delete_movie_links_in(ctx, &source_ids).await?;

        // ② 别名合并。
        let mut alias_names: Vec<String> = Vec::new();
        for source in sources {
            alias_names.push(source.name.clone());
            alias_names.extend(
                split_alias_name(&source.alias_name)
                    .into_iter()
                    .map(str::to_owned),
            );
            let override_name = source
                .display_name_override
                .as_deref()
                .unwrap_or_default()
                .trim();
            if !override_name.is_empty() {
                alias_names.push(override_name.to_owned());
            }
        }
        let alias_refs: Vec<&str> = alias_names.iter().map(String::as_str).collect();
        let merged_alias = merge_alias_name(&target.name, &alias_refs, &target.alias_name);

        // ③ 订阅合并：任一来源已订阅 → 订阅；`subscribed_at` 取最早。
        let subscribed_sources: Vec<&Actor> = sources
            .iter()
            .filter(|source| source.is_subscribed)
            .collect();
        let is_subscribed = target.is_subscribed || !subscribed_sources.is_empty();
        let mut subscribed_at = target.subscribed_at;
        let mut subscribed_dates: Vec<NaiveDateTime> = target.subscribed_at.into_iter().collect();
        for source in &subscribed_sources {
            if let Some(at) = source.subscribed_at {
                subscribed_dates.push(at);
            }
        }
        if is_subscribed && !subscribed_dates.is_empty() {
            if let Some(earliest) = subscribed_dates.into_iter().min() {
                if subscribed_at.is_none_or(|current| earliest < current) {
                    subscribed_at = Some(earliest);
                }
            }
        } else if is_subscribed && subscribed_at.is_none() {
            // 「已订阅但没有时间戳」：补当前时间，不能留 NULL —— 否则
            // 「按订阅时间倒序」的列表会把这条排到最后。
            subscribed_at = Some(now_utc());
        }

        // ④ 填空受保护字段。
        let mut update = ActorUpdate::new();
        let mut owner_updates: JsonMap<String, Json> = JsonMap::new();
        let mut filled = 0usize;
        for field in PROTECTED_ACTOR_FIELDS {
            if !field_is_empty(target, field) {
                continue;
            }
            for source in sources {
                if field_is_empty(source, field) {
                    continue;
                }
                let owner = source
                    .field_owners
                    .as_object()
                    .and_then(|owners| owners.get(field))
                    .and_then(Json::as_str);
                if owner == Some(MANUAL_FIELD_OWNER) {
                    continue;
                }
                copy_field(&mut update, field, source);
                if let Some(owner) = owner {
                    owner_updates.insert(field.to_owned(), Json::from(owner));
                }
                filled += 1;
                break;
            }
        }

        // ⑤ 头像搬运：仅当目标**两张都空**时才搬。
        let mut profile_image_id = target.profile_image_id;
        let mut profile_override_id = target.profile_image_override_id;
        let mut moved_image: Option<(i32, &'static str)> = None;
        if profile_image_id.is_none() && profile_override_id.is_none() {
            for source in sources {
                if source.profile_image_override_id.is_some() {
                    profile_override_id = source.profile_image_override_id;
                    moved_image = Some((source.id, "profile_image_override_id"));
                    break;
                }
            }
            if moved_image.is_none() {
                for source in sources {
                    if source.profile_image_id.is_some() {
                        profile_image_id = source.profile_image_id;
                        moved_image = Some((source.id, "profile_image_id"));
                        break;
                    }
                }
            }
        }

        // ⑥ 组装并应用目标更新。
        update.set_text("alias_name", Some(merged_alias));
        update.set_bool("is_subscribed", is_subscribed);
        update.set_timestamp("subscribed_at", subscribed_at);
        if is_subscribed {
            // 同步任务会覆盖墓碑的 javdb_id，强制下一次全量以补齐来源 ID 的历史影片。
            update.set_null("subscribed_movies_full_synced_at");
        }
        if !owner_updates.is_empty() {
            update.merge_field_owners(owner_updates);
        }
        if filled > 0 {
            update.bump_mutation_revision();
        }
        if profile_image_id != target.profile_image_id {
            update.set_int("profile_image_id", profile_image_id);
        }
        if profile_override_id != target.profile_image_override_id {
            update.set_int("profile_image_override_id", profile_override_id);
        }
        update.touch();
        actors.apply_update_in(ctx, target.id, &update).await?;

        // ⑦ 打墓碑并压平链。
        actors
            .tombstone_sources_in(ctx, target.id, &source_ids)
            .await?;
        actors
            .redirect_tombstones_in(ctx, target.id, &source_ids)
            .await?;

        // ⑧ 头像若被搬走，来源行上那一列要清掉。
        if let Some((source_id, field)) = moved_image {
            let mut clear = ActorUpdate::new();
            clear.set_null(field);
            clear.touch();
            actors.apply_update_in(ctx, source_id, &clear).await?;
        }

        Ok(())
    }
}

/// 404 构造。与 `super::actor` 里同名文案一致（上游传的是中文「演员不存在」）。
fn actor_not_found(actor_id: i32) -> ServiceError {
    ServiceError::not_found(ACTOR_NOT_FOUND, "演员不存在", "actor_id", actor_id)
}

/// 422 `merge_self`。
fn merge_self(actor_id: i32) -> ServiceError {
    invalid_actor_merge("merge_self", "不能把演员合并到自身", actor_id)
}

/// 422 `source_already_merged`。
fn source_already_merged(actor_id: i32) -> ServiceError {
    invalid_actor_merge(
        "source_already_merged",
        "来源演员已合并到其他演员",
        actor_id,
    )
}

fn invalid_actor_merge(reason: &str, message: &str, actor_id: i32) -> ServiceError {
    let mut details = JsonMap::new();
    details.insert("reason".to_owned(), Json::from(reason));
    details.insert("actor_id".to_owned(), Json::from(actor_id));
    ServiceError::validation_with(INVALID_ACTOR_MERGE, message, details)
}

/// 某个受保护字段在这位演员上是否「空」。
///
/// `gender` 的「空」是 `0`（未知）而不是 `NULL` —— 上游
/// `_field_is_empty` 对 `gender` 特判 `value in (None, 0)`。
fn field_is_empty(actor: &Actor, field: &str) -> bool {
    match field {
        "gender" => actor.gender == 0,
        "birthday" => actor.birthday.is_none(),
        "height_cm" => actor.height_cm.is_none(),
        "bust_cm" => actor.bust_cm.is_none(),
        "waist_cm" => actor.waist_cm.is_none(),
        "hips_cm" => actor.hips_cm.is_none(),
        "cup" => actor.cup.is_none(),
        "birthplace" => actor.birthplace.is_none(),
        "blood_type" => actor.blood_type.is_none(),
        other => unreachable!("PROTECTED_ACTOR_FIELDS 里的 {other} 没有空判定"),
    }
}

/// 把来源的一个受保护字段写进目标更新。
fn copy_field(update: &mut ActorUpdate, field: &str, source: &Actor) {
    match field {
        "gender" => {
            update.set_int("gender", Some(source.gender));
        }
        "birthday" => {
            update.set_date("birthday", source.birthday);
        }
        "height_cm" => {
            update.set_int("height_cm", source.height_cm);
        }
        "bust_cm" => {
            update.set_int("bust_cm", source.bust_cm);
        }
        "waist_cm" => {
            update.set_int("waist_cm", source.waist_cm);
        }
        "hips_cm" => {
            update.set_int("hips_cm", source.hips_cm);
        }
        "cup" => {
            update.set_text("cup", source.cup.clone());
        }
        "birthplace" => {
            update.set_text("birthplace", source.birthplace.clone());
        }
        "blood_type" => {
            update.set_text("blood_type", source.blood_type.clone());
        }
        other => unreachable!("PROTECTED_ACTOR_FIELDS 里的 {other} 没有拷贝分支"),
    }
}
