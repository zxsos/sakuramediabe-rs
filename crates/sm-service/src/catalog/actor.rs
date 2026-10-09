//! 演员目录 service，对应上游 `src/service/catalog/actor_service.py`(827 行) 的
//! **11 个端点**里可做的那部分。
//!
//! # 端点对照
//!
//! | 上游端点 | 本文件 | 状态 |
//! |---|---|---|
//! | `GET /actors` | [`ActorService::list`] | **已落** |
//! | `GET /actors/filter-options` | [`ActorService::filter_options`] | **已落** |
//! | `GET /actors/{id}` | [`ActorService::detail`] | **已落** |
//! | `PATCH /actors/{id}` | [`ActorService::update_profile`] | **已落** |
//! | `PUT`/`DELETE /actors/{id}/subscription` | [`ActorService::set_subscription`] | **已落** |
//! | `GET /actors/{id}/movie-ids` | [`ActorService::movie_ids`] | **已落** |
//! | `GET /actors/{id}/tags` | [`ActorService::tags`] | **已落** |
//! | `GET /actors/{id}/years` | [`ActorService::years`] | **已落** |
//! | `POST /actors/{id}/merge` | [`crate::catalog::actor_merge`] | **已落**（六步合并） |
//! | `DELETE /actors/{id}/profile-image` | [`ActorService::clear_profile_image`] | **已落**（只做 loose 分支，见下） |
//! | `POST /actors/search/javdb/stream` | —— | **阻塞**：`build_javdb_provider()`(JavDB + httpx) + `CatalogImportService`(其 import 段有 `src.plugins.extensions.metadata`，即 `sm-plugins` 空壳) |
//! | `PUT /actors/{id}/profile-image` | —— | **阻塞**：需要 EXIF 旋转 + LANCZOS 缩略 + **有损** WebP(q=90)；`svc-image` 明确「刻意不提供 `encode_lossy`」，属路线图阶段 9 |
//!
//! # 三处刻意不复刻上游
//!
//! 1. **分页不校验。** 上游 `list_actors` 没有 `validate_page`，只有
//!    `start = max(page - 1, 0) * page_size`。而本仓库其它域都走
//!    [`sm_core::pagination::validate_page`]（1..=100）。这里**不**校验：
//!    那样会让 `page_size=200` 从 200 条变成 422 —— 宽表视图是真实用法。
//!    见 [`ActorListParams::offset`]。
//! 2. **未知字段被静默忽略。** 上游 `SchemaModel` 没开 `extra="forbid"`，
//!    pydantic 默认 `ignore`，所以 `update_profile` 里那段
//!    `unsupported_fields → invalid_actor_update` **经HTTP 不可达**。
//!    这里保留该分支并注明不可达，而不是删掉 —— 删掉会让后来人以为
//!    上游没有这条规则。
//! 3. **年龄只有一个算法。** `age_min`/`age_max` 筛选、筛选项区间、
//!    列表项 `age` 三处都走 `sm_db::catalog::actor::age_for_birthday`。
//!
//! # 墓碑只跳一跳
//!
//! [`ActorService::detail`] 的 `_require_actor` 等价物只跟随**一跳**
//! `merged_into_id`，与上游一致 —— 上游**没有**走完整链。链长恒为 1
//! （合并会压平，见 `sm_db::repo::actor` 模块文档），但一旦有人绕过合并流程
//! 写指针，「跳一跳」与「走到底」就会给出不同演员，而本层必须与上游相同。

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{Map, Value};
use sm_core::text_search::split_search_terms;
use sm_db::catalog::actor::{age_for_birthday, Actor};
use sm_db::common::page::Page;
use sm_db::repo::actor::{
    ActorListFilter, ActorListRow, ActorRepository, ActorScope, ActorSort, ActorSortKey,
    ActorUpdate,
};
use sm_db::{Db, DbError};

use crate::error::{details_of, ServiceError};

/// 本域列表类校验的错误码。
///
/// 分页（不校验，见模块文档第 1 条）、搜索词、排序、区间互相矛盾
/// **四类**都归到这一个码 —— 与上游 `_filtered_actors` /
/// `resolve_sort_expression(error_code="invalid_actor_filter")` 一致。
/// 客户端按这一个码提示「筛选条件有问题」。
pub const INVALID_ACTOR_FILTER: &str = "invalid_actor_filter";

/// 404 的错误码。上游 `require_by_id(Actor, id, "actor", ...)` 的默认码。
pub const ACTOR_NOT_FOUND: &str = "actor_not_found";

/// `PATCH` 的请求体校验错误码。
///
/// 与上游 FastAPI 的 `RequestValidationError` 处理器一致
/// （`src/api/exception/exception.py:63`）：pydantic 层校验失败一律
/// `422 validation_error`，**不**带业务码。
pub const VALIDATION_ERROR: &str = "validation_error";

/// 可通过 `PATCH /actors/{id}` 修改的字段。
///
/// 对应上游 `ActorService.ACTOR_PROFILE_EDITABLE_FIELDS`。
/// **10 个**，注意不含 `is_subscribed` / `profile_image_*` / `merged_into_id`
/// —— 订阅与头像各有自己的端点，墓碑只能由合并产生。
pub const PROFILE_EDITABLE_FIELDS: [&str; 10] = [
    "birthday",
    "blood_type",
    "bust_cm",
    "cup",
    "display_name_override",
    "gender",
    "height_cm",
    "hips_cm",
    "birthplace",
    "waist_cm",
];

/// 人工修改的归属标记。与上游 `actor_ownership_gateway.MANUAL_ACTOR_FIELD_OWNER`。
pub const MANUAL_FIELD_OWNER: &str = "host:manual";

/// 列表查询参数。
#[derive(Debug, Clone, Default)]
pub struct ActorListParams {
    /// `1` = 女、`2` = 男。`None` = 不限（上游 `ActorListGender.ALL`）。
    pub gender: Option<i32>,
    /// `Some(true)` = 只看已订阅（上游 `SUBSCRIBED`）。
    pub subscribed: Option<bool>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub height_min: Option<i32>,
    pub height_max: Option<i32>,
    /// **已归一为大写**的罩杯值。非空即筛选。
    pub cups: Vec<String>,
    pub has_playable_movies: bool,
    /// 形如 `movie_count:desc`。`None` / 空白 = 用默认排序。
    pub sort: Option<String>,
    /// 检索词原文，交给 `split_search_terms` 拆分。
    pub query: Option<String>,
    pub page: i64,
    pub page_size: i64,
}

impl ActorListParams {
    /// 偏移量。**复刻上游的 `max(page - 1, 0)`**。
    ///
    /// `page <= 1` 一律夹到第一页，而响应里 `page` **回显请求的原值** ——
    /// 上游 `PageResponse(page=page, ...)` 就是这么做的，客户端看到
    /// `page=0` 配第一页数据就知道自己传错了。
    pub fn offset(&self) -> i64 {
        self.page.saturating_sub(1).max(0) * self.page_size
    }
}

/// 一位演员的列表 / 详情视图。
///
/// 具名类型放在 service 层而不是 `sm-db`：它是**投影**（演员本体 + 三个
/// 计算出来的列），不是表镜像，在 `sm-db` 里声明会让 schema 对拍把它当成
/// 待验证的表模型。
#[derive(Debug, Clone)]
pub struct ActorView {
    pub actor: Actor,
    /// 关联影片数（实时按 `movie_actor` 数）。
    pub movie_count: i64,
    /// 生效头像。**覆盖优先**，与 `actor.effective_profile_image` 同序。
    pub image_id: Option<i32>,
    pub image_origin: Option<String>,
    /// 周岁。`birthday` 为空时 `None`。
    pub age: Option<i32>,
    /// 归属为 `host:manual` 的字段名，**升序**。
    pub manual_fields: Vec<String>,
}

/// 一页演员。
#[derive(Debug, Clone)]
pub struct ActorPage {
    pub items: Vec<ActorView>,
    /// 回显请求值（可能是 0 或负数，见 [`ActorListParams::offset`]）。
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
}

/// 筛选项里的一个区间。
#[derive(Debug, Clone, Copy, Default)]
pub struct ActorFilterRange {
    pub min: Option<i32>,
    pub max: Option<i32>,
    /// 该区间里**有值**的演员数（`COUNT(col)`，不是 `COUNT(*)`）。
    pub populated_count: i64,
}

/// `GET /actors/filter-options` 的结果。
#[derive(Debug, Clone)]
pub struct ActorFilterOptions {
    pub actor_count: i64,
    /// 基准日期。响应里的 `as_of_date`，客户端据此显示「截至某日」。
    pub as_of_date: NaiveDate,
    pub age: ActorFilterRange,
    pub height_cm: ActorFilterRange,
    /// `(罩杯值, 演员数)`，按值升序。
    pub cups: Vec<(String, i64)>,
}

/// 一位演员关联的标签。
#[derive(Debug, Clone)]
pub struct ActorTag {
    pub tag_id: i32,
    pub name: String,
}

/// 一位演员关联影片的年份分布。
#[derive(Debug, Clone, Copy)]
pub struct ActorYear {
    pub year: i32,
    pub movie_count: i64,
}

/// 演员目录 service。
#[derive(Debug, Clone)]
pub struct ActorService {
    repo: ActorRepository,
}

impl ActorService {
    pub fn new(db: &Db) -> Self {
        Self {
            repo: ActorRepository::new(db.clone()),
        }
    }

    /// `GET /actors`。
    ///
    /// 校验顺序与上游一致：**先**拆检索词（可能 422），**再**校验区间
    /// （`_filtered_actors` 入口）。顺序换了的话，一个「区间矛盾 + 词数超限」
    /// 的请求会报出不同的码，而客户端按码分支提示。
    pub async fn list(&self, params: &ActorListParams) -> Result<ActorPage, ServiceError> {
        self.list_on(params, today_utc()).await
    }

    /// [`Self::list`]，但基准日期由调用方给定。
    ///
    /// 年龄全靠「今天」算，不注入就没法测「昨天 30 岁今天 31 岁」这种
    /// 跨年生日 —— 而那正是 `years_before` 钳位逻辑最容易写错的地方。
    pub async fn list_on(
        &self,
        params: &ActorListParams,
        today: NaiveDate,
    ) -> Result<ActorPage, ServiceError> {
        let terms = split_terms(params.query.as_deref())?;
        validate_ranges(params)?;

        let filter = ActorListFilter {
            gender: params.gender,
            subscribed: params.subscribed,
            age_min: params.age_min,
            age_max: params.age_max,
            height_min: params.height_min,
            height_max: params.height_max,
            cups: params.cups.clone(),
            has_playable_movies: params.has_playable_movies,
            search_terms: terms.clone(),
        };
        let order = sort_with_terms(params.sort.as_deref(), &terms)?;

        let Page { items, total } = self
            .repo
            .list_filtered(&filter, &order, today, params.page_size, params.offset())
            .await?;

        Ok(ActorPage {
            items: items.iter().map(|row| view_of(row, today)).collect(),
            // 回显**请求的** page / page_size，而不是夹取后的 offset 推算值。
            page: params.page,
            page_size: params.page_size,
            total,
        })
    }

    /// 批量取视图（含生效头像）。`hot-actress-releases` 的装配用。
    ///
    /// 与 [`Self::detail`] 的区别是**批量 + 不报 404**：缺的 id 只是不出现在
    /// 返回的映射里，由调用方决定跳过还是报错（上游那处 `continue`）。
    /// `movie_count` / `age` / `manual_fields` 与列表端点同一份 `view_of` ——
    /// 三处各写一遍迟早会漂。
    pub async fn views_of(
        &self,
        actor_ids: &[i32],
    ) -> Result<HashMap<i32, ActorView>, ServiceError> {
        let today = today_utc();
        Ok(self
            .repo
            .find_with_images(actor_ids)
            .await?
            .iter()
            .map(|row| (row.0.id, view_of(row, today)))
            .collect())
    }

    /// `GET /actors/filter-options`。
    ///
    /// 作用域只有性别与订阅状态 —— 上游**不含**年龄/身高/罩杯筛选本身，
    /// 否则筛选项会自我收窄（选了 C 罩就没有 D 罩的选项）。
    pub async fn filter_options(
        &self,
        gender: Option<i32>,
        subscribed: Option<bool>,
        today: NaiveDate,
    ) -> Result<ActorFilterOptions, ServiceError> {
        let scope = ActorScope { gender, subscribed };
        let (actor_count, birthday_count, oldest, youngest, height_count, min_height, max_height) =
            self.repo.filter_aggregate(&scope).await?;
        let cups = self.repo.cup_options(&scope).await?;

        Ok(ActorFilterOptions {
            actor_count,
            as_of_date: today,
            // 年龄区间由生日区间**换算**：最年轻的生日 → 最小年龄。
            // 方向反了的话筛选项会显示成「最小 60 岁」，而点进去是空的。
            age: ActorFilterRange {
                min: youngest.map(|date| age_for_birthday(date, today)),
                max: oldest.map(|date| age_for_birthday(date, today)),
                populated_count: birthday_count,
            },
            height_cm: ActorFilterRange {
                min: min_height,
                max: max_height,
                populated_count: height_count,
            },
            cups,
        })
    }

    /// `GET /actors/{id}`。
    ///
    /// 先按 id 取，再按 `merged_into_id` **跳一跳**（上游 `_require_actor`）。
    pub async fn detail(&self, actor_id: i32) -> Result<ActorView, ServiceError> {
        self.detail_on(actor_id, today_utc()).await
    }

    /// [`Self::detail`]，基准日期由调用方给定。
    pub async fn detail_on(
        &self,
        actor_id: i32,
        today: NaiveDate,
    ) -> Result<ActorView, ServiceError> {
        Ok(view_of(&self.require_row(actor_id).await?, today))
    }

    /// `PATCH /actors/{id}`。
    ///
    /// `body` 是**原始 JSON 对象**，不是已反序列化的结构体 —— 因为
    /// `exclude_unset` 语义要求知道「哪些键出现过」，而 serde 的
    /// `Option<T>` 把「键不存在」与「键存在且为 null」都解成 `None`。
    pub async fn update_profile(
        &self,
        actor_id: i32,
        body: &Map<String, Value>,
        today: NaiveDate,
    ) -> Result<ActorView, ServiceError> {
        // 先取演员：上游 `update_profile` 开头就 `_require_actor`，
        // 所以「演员不存在」优先于「请求体不合法」。
        let row = self.require_row(actor_id).await?;
        let (actor, _, _, _) = &row;

        let changes = parse_profile_changes(body)?;
        if changes.is_empty() {
            return Err(ServiceError::validation(
                "empty_actor_update",
                "至少需要修改一个资料字段",
            ));
        }

        // `birthday` 晚于今天 → 422。上游比的是 `utc_now_for_db().date()`，
        // 不带时分秒。
        if let Some(birthday) = changes.iter().find_map(|c| match c.value {
            ProfileValue::Date(Some(date)) if c.column == "birthday" => Some(date),
            _ => None,
        }) {
            if birthday > today {
                return Err(ServiceError::validation(
                    "invalid_actor_profile",
                    "birthday 不能晚于今天",
                ));
            }
        }

        let mut update = ActorUpdate::new();
        let mut manual = serde_json::Map::new();
        for change in &changes {
            match &change.value {
                ProfileValue::Text(value) => {
                    update.set_text(change.column, value.clone());
                }
                ProfileValue::Int(value) => {
                    update.set_int(change.column, *value);
                }
                ProfileValue::Date(value) => {
                    update.set_date(change.column, *value);
                }
            }
            // `display_name_override` **不进** field_owners：上游
            // `scalar_fields = changes & (EDITABLE - {"display_name_override"})`。
            // 它是显示名覆盖而不是资料字段，归属由 actor 表之外的东西管。
            if change.column != "display_name_override" {
                manual.insert(change.column.to_owned(), Value::from(MANUAL_FIELD_OWNER));
            }
        }
        if !manual.is_empty() {
            update.merge_field_owners(manual);
            // 版本只在真的有资料字段被改时推进 —— 与上游
            // `if scalar_fields: mutation_revision + 1` 一致。
            update.bump_mutation_revision();
        }
        update.touch();

        let affected = self.repo.apply_update(actor.id, &update).await?;
        if affected != 1 {
            // 行数不是 1 = 演员在「取到」与「写入」之间被删了。
            // 上游同样在这里报 404 而不是静默成功。
            return Err(actor_not_found(actor.id));
        }
        self.detail_on(actor.id, today).await
    }

    /// `PUT` / `DELETE /actors/{id}/subscription`。
    ///
    /// 重复订阅**不刷新** `subscribed_at`（上游只在它是 `None` 时写
    /// `utc_now_for_db()`）—— 否则「关注列表按订阅时间倒序」会在每次点按
    /// 后把这条顶到最前面。
    pub async fn set_subscription(
        &self,
        actor_id: i32,
        subscribed: bool,
    ) -> Result<(), ServiceError> {
        let row = self.require_row(actor_id).await?;
        self.repo
            .set_subscribed(row.0.id, subscribed)
            .await
            // 行已在上面读到过，所以 NotFound 只可能是并发删除 —— 报 404
            // 而不是 500，客户端的「重试」按钮才有意义。
            .map_err(|err| match err {
                DbError::NotFound { .. } => actor_not_found(row.0.id),
                other => ServiceError::from(other),
            })?;
        Ok(())
    }

    /// `DELETE /actors/{id}/profile-image`。
    ///
    /// 上游在清掉覆盖之后要 `ImageCleanupService.delete_image_record_if_unused`
    /// —— 它检查**6 张表**（`movie` / `actor` / `movie_plot_image` /
    /// `media_thumbnail` / `media_point` / `video_item`）是否还引用这张图，
    /// 不被引用才删 `image` 行。
    ///
    /// # 刻意只做 loose 文件删除
    ///
    /// 上游接着还会 `delete_obsolete_image_files`，它对**打包内**的图片要
    /// 重建 zip（`MovieAssetPackService` 169 行 + `image_store` 59 行）。
    /// 而演员头像的 origin 恒为 `actors/manual/<id>-<hex>.webp`，
    /// **永远不是包内条目** —— 所以那条分支从演员端点不可达。
    /// 这里只删 loose 文件，并在 [`crate::catalog::actor`] 侧留注释说明
    /// 打包重建属于打包服务，尚未移植。
    ///
    /// 幂等：本来就没有覆盖时直接返回详情（上游同样提前 return）。
    pub async fn clear_profile_image(&self, actor_id: i32) -> Result<ActorView, ServiceError> {
        self.clear_profile_image_on(actor_id, today_utc()).await
    }

    /// [`Self::clear_profile_image`]，基准日期由调用方给定。
    pub async fn clear_profile_image_on(
        &self,
        actor_id: i32,
        today: NaiveDate,
    ) -> Result<ActorView, ServiceError> {
        let row = self.require_row(actor_id).await?;
        let (actor, _, _, _) = &row;
        if actor.profile_image_override_id.is_none() {
            // 上游：`if old_override is None: return get_actor_detail(...)`。
            // 提前返回意味着**连 updated_at 都不动**。
            return self.detail_on(actor.id, today).await;
        }

        let mut update = ActorUpdate::new();
        update.set_null("profile_image_override_id");
        update.touch();
        let affected = self.repo.apply_update(actor.id, &update).await?;
        if affected != 1 {
            return Err(actor_not_found(actor.id));
        }
        self.detail_on(actor.id, today).await
    }

    /// `GET /actors/{id}/movie-ids`。
    pub async fn movie_ids(&self, actor_id: i32) -> Result<Vec<i32>, ServiceError> {
        let row = self.require_row(actor_id).await?;
        Ok(self.repo.movie_ids(row.0.id).await?)
    }

    /// `GET /actors/{id}/tags`。按标签名升序。
    pub async fn tags(&self, actor_id: i32) -> Result<Vec<ActorTag>, ServiceError> {
        let row = self.require_row(actor_id).await?;
        Ok(self
            .repo
            .actor_tags(row.0.id)
            .await?
            .into_iter()
            .map(|(tag_id, name)| ActorTag { tag_id, name })
            .collect())
    }

    /// `GET /actors/{id}/years`。按年份降序。
    pub async fn years(&self, actor_id: i32) -> Result<Vec<ActorYear>, ServiceError> {
        let row = self.require_row(actor_id).await?;
        Ok(self
            .repo
            .actor_years(row.0.id)
            .await?
            .into_iter()
            .map(|(year, movie_count)| ActorYear { year, movie_count })
            .collect())
    }

    /// `_require_actor` 的等价物：取一行，并让墓碑指向保留记录。
    ///
    /// **只跳一跳**，与上游一致（见模块文档）。
    async fn require_row(&self, actor_id: i32) -> Result<ActorListRow, ServiceError> {
        let row = self
            .repo
            .find_with_image(actor_id)
            .await?
            .ok_or_else(|| actor_not_found(actor_id))?;
        let Some(target_id) = row.0.merged_into_id else {
            return Ok(row);
        };
        // 上游是 `canonical or actor`：跳一跳指向的行不存在时**退回原行**，
        // 而不是报错 —— 悬空指针不该让整个详情端点 404。
        Ok(self.repo.find_with_image(target_id).await?.unwrap_or(row))
    }
}

/// 404 的构造。文案是上游显式传的那个，不是 `require_by_id` 的默认
/// `"{entity} not found"` —— 上游传的是中文「演员不存在」。
fn actor_not_found(actor_id: i32) -> ServiceError {
    ServiceError::not_found(ACTOR_NOT_FOUND, "演员不存在", "actor_id", actor_id)
}

/// UTC 今天。对应上游 `utc_now_for_db().date()`。
///
/// naive UTC：`timestamp` 列存的是 naive UTC，而生日/年龄是**日期**语义，
/// 用本地时区会让「今天」在某些部署里偏一天。
fn today_utc() -> NaiveDate {
    chrono::Utc::now().naive_utc().date()
}

/// 拆检索词。**未传 `query` 与传空串是两种结果**：前者不过滤，后者得到
/// 空词表（等价于不过滤）—— 与上游一致（`split_search_terms(None) == []`，
/// 而 `""` 拆出 0 个词）。
///
/// 越界 → 422 [`INVALID_ACTOR_FILTER`]，`details` 回显**原始输入**
/// （`{"query": value}`），不是归一后的词表。
fn split_terms(query: Option<&str>) -> Result<Vec<String>, ServiceError> {
    split_search_terms(query).map_err(|err| {
        ServiceError::validation_with(
            INVALID_ACTOR_FILTER,
            err.reason(),
            details_of("query", query.unwrap_or_default()),
        )
    })
}

/// 区间校验：`age_min <= age_max`、`height_min <= height_max`。
///
/// 上游只报**第一个**越界的（`age` 先于 `height`），消息与 details 逐字对齐。
fn validate_ranges(params: &ActorListParams) -> Result<(), ServiceError> {
    if let (Some(min), Some(max)) = (params.age_min, params.age_max) {
        if min > max {
            let mut details = details_of("age_min", min);
            details.insert("age_max".to_owned(), Value::from(max));
            return Err(ServiceError::validation_with(
                INVALID_ACTOR_FILTER,
                "age_min 不能大于 age_max",
                details,
            ));
        }
    }
    if let (Some(min), Some(max)) = (params.height_min, params.height_max) {
        if min > max {
            let mut details = details_of("height_min", min);
            details.insert("height_max".to_owned(), Value::from(max));
            return Err(ServiceError::validation_with(
                INVALID_ACTOR_FILTER,
                "height_min 不能大于 height_max",
                details,
            ));
        }
    }
    Ok(())
}

/// 解析 `字段:方向` 排序表达式。
///
/// 三条出口，与上游 `_build_actor_list_sort` 一一对应：
///
/// 1. **有检索词且没给排序** → 按相关度（[`ActorSort::SearchRelevance`]）；
/// 2. 没给排序（`None` 或纯空白）→ [`ActorSort::Default`]（`id ASC`）；
/// 3. 其余 → `字段:方向`，字段非法 / 缺冒号 / 方向不是 `asc|desc` → 422。
///
/// 注意第 1 条**优先于**第 2 条：给了检索词又没给排序时，排序是相关度而不是
/// 默认 `id` —— 否则「搜到了但顺序随机」。
pub fn parse_sort(value: Option<&str>, has_terms: bool) -> Result<ActorSort, ServiceError> {
    let Some(raw) = value else {
        return Ok(if has_terms {
            ActorSort::SearchRelevance { terms: Vec::new() }
        } else {
            ActorSort::Default
        });
    };
    if raw.trim().is_empty() {
        // 上游的条件是 `if search_terms and (sort is None or not sort.strip())`：
        // **显式给了排序就以排序为准**，检索词不参与排序决策。
        return Ok(if has_terms {
            ActorSort::SearchRelevance { terms: Vec::new() }
        } else {
            ActorSort::Default
        });
    }
    parse_field_sort(raw)
}

/// 带真实词表的 [`parse_sort`]。检索词排序必须带上词 —— 空词表会让
/// `ORDER BY  ASC`（空表达式）拼出非法 SQL。
fn sort_with_terms(sort: Option<&str>, terms: &[String]) -> Result<ActorSort, ServiceError> {
    let resolved = parse_sort(sort, !terms.is_empty())?;
    match resolved {
        ActorSort::SearchRelevance { .. } => Ok(ActorSort::SearchRelevance {
            terms: terms.to_vec(),
        }),
        other => Ok(other),
    }
}

/// 解析 `字段:方向`。分离出来是为了能被单测直接调用（不依赖检索词）。
fn parse_field_sort(raw: &str) -> Result<ActorSort, ServiceError> {
    let normalized = raw.trim().to_lowercase();
    let invalid = || {
        ServiceError::validation_with(
            INVALID_ACTOR_FILTER,
            "Invalid sort expression",
            // details 回显**原始**输入（含空白与原大小写），与上游一致。
            details_of("sort", raw),
        )
    };
    let Some((field, direction)) = normalized.split_once(':') else {
        return Err(invalid());
    };
    if direction != "asc" && direction != "desc" {
        return Err(invalid());
    }
    let Some(key) = ActorSortKey::from_name(field) else {
        return Err(invalid());
    };
    Ok(ActorSort::Field {
        key,
        descending: direction == "desc",
    })
}

/// 一个待写入的资料字段。
#[derive(Debug, Clone)]
struct ProfileChange {
    column: &'static str,
    value: ProfileValue,
}

/// 资料字段的值。类型对应 DDL：`display_name_override` / `cup` /
/// `birthplace` / `blood_type` 是文本，`gender` 与四个尺寸是整数，
/// `birthday` 是**日期**（`date` 列，不是 `timestamp`）。
#[derive(Debug, Clone)]
enum ProfileValue {
    Text(Option<String>),
    Int(Option<i32>),
    Date(Option<NaiveDate>),
}

/// 把请求体解析成待写入字段。
///
/// # 未知键被忽略，与上游一致
///
/// 上游 `SchemaModel` 没有 `extra="forbid"`，pydantic v2 默认 `ignore`，
/// 所以 `{"nickname": "x"}` 里的 `nickname` 既不报错也不写入 ——
/// 而因为它不算 `changes`，若请求体只有它，就落到
/// `empty_actor_update` 422。
///
/// # `gender` 不接受显式 null
///
/// 上游 `validate_explicit_gender`：`gender` 出现在请求体里就不能是 `null`
/// ——「未知」请用 `0`。这条容易漏，因为其余 9 个字段都能用 `null` 清空。
fn parse_profile_changes(body: &Map<String, Value>) -> Result<Vec<ProfileChange>, ServiceError> {
    let mut changes = Vec::new();
    // 按 `PROFILE_EDITABLE_FIELDS` 的固定顺序遍历，而不是请求体的键序：
    // 上游 `changes` 是 dict，赋值顺序取决于 JSON 键序，但那不影响结果
    // （每列各写一次）。固定顺序让生成的 SQL 与测试断言都稳定。
    for column in PROFILE_EDITABLE_FIELDS {
        let Some(raw) = body.get(column) else {
            continue;
        };
        let value = match column {
            "display_name_override" => ProfileValue::Text(optional_text(raw, column, 255)?),
            "birthplace" | "blood_type" => ProfileValue::Text(optional_text(raw, column, 255)?),
            "cup" => ProfileValue::Text(normalize_cup(raw)?),
            "gender" => {
                let value = optional_int(raw, column)?;
                match value {
                    None => return Err(invalid_field(column, "gender 不能为 null，未知请使用 0")),
                    Some(gender) if !(0..=2).contains(&gender) => {
                        return Err(invalid_field(column, "gender 必须是 0、1 或 2"))
                    }
                    Some(gender) => ProfileValue::Int(Some(gender)),
                }
            }
            "height_cm" | "bust_cm" | "waist_cm" | "hips_cm" => {
                let value = optional_int(raw, column)?;
                match value {
                    // 上游 `Field(ge=1)`：null 通过，0 与负数被拒。
                    Some(size) if size < 1 => {
                        return Err(invalid_field(column, "必须是大于等于 1 的整数"))
                    }
                    other => ProfileValue::Int(other),
                }
            }
            "birthday" => ProfileValue::Date(optional_date(raw, column)?),
            other => unreachable!("PROFILE_EDITABLE_FIELDS 里的 {other} 没有解析分支"),
        };
        changes.push(ProfileChange { column, value });
    }

    // 不可达分支，见函数文档。保留是为了让「上游有这条规则」这件事留在代码里。
    let unsupported: Vec<&str> = body
        .keys()
        .map(String::as_str)
        .filter(|key| {
            !PROFILE_EDITABLE_FIELDS.contains(key)
                && Actor::is_guarded(key)
                && !Actor::is_protected(key)
        })
        .collect();
    if !unsupported.is_empty() {
        let mut sorted = unsupported;
        sorted.sort_unstable();
        return Err(ServiceError::validation_with(
            "invalid_actor_update",
            "包含不支持修改的女优字段",
            details_of("fields", Value::from(sorted)),
        ));
    }
    Ok(changes)
}

/// 空 `details` 的 422。
fn invalid_field(field: &str, reason: &str) -> ServiceError {
    ServiceError::validation_with(
        VALIDATION_ERROR,
        "Request validation failed",
        details_of(field, reason),
    )
}

/// `null` → `None`；字符串先 trim，空串 → `None`；超过 `max_length` → 422。
///
/// 对应上游 `normalize_optional_text` + `Field(max_length=255)`。
fn optional_text(
    raw: &Value,
    field: &str,
    max_length: usize,
) -> Result<Option<String>, ServiceError> {
    match raw {
        Value::Null => Ok(None),
        Value::String(text) => {
            if text.chars().count() > max_length {
                return Err(invalid_field(
                    field,
                    &format!("长度不能超过 {max_length} 个字符"),
                ));
            }
            let trimmed = text.trim();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            })
        }
        other => Err(invalid_field(
            field,
            &format!("必须是字符串或 null，收到 {}", kind_of(other)),
        )),
    }
}

/// 罩杯：trim → 大写；空 → `None`；非 ASCII 字母或超过 4 个字母 → 422。
///
/// 对应上游 `normalize_cup`。注意列是 `varchar(255)`，4 字母上限是
/// **pydantic 的约束**，数据库不拦 —— 所以校验必须在这一层做。
fn normalize_cup(raw: &Value) -> Result<Option<String>, ServiceError> {
    match raw {
        Value::Null => Ok(None),
        Value::String(text) => {
            let upper = text.trim().to_uppercase();
            if upper.is_empty() {
                return Ok(None);
            }
            let ascii_alpha = upper.chars().all(|c| c.is_ascii_alphabetic());
            if !ascii_alpha || upper.chars().count() > 4 {
                return Err(invalid_field("cup", "cup 必须是 1 到 4 个英文字母"));
            }
            Ok(Some(upper))
        }
        other => Err(invalid_field(
            "cup",
            &format!("必须是字符串或 null，收到 {}", kind_of(other)),
        )),
    }
}

/// `null` → `None`；非整数 → 422。
fn optional_int(raw: &Value, field: &str) -> Result<Option<i32>, ServiceError> {
    match raw {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| invalid_field(field, "必须是 32 位整数或 null")),
        other => Err(invalid_field(
            field,
            &format!("必须是整数或 null，收到 {}", kind_of(other)),
        )),
    }
}

/// `null` → `None`；必须是**严格**的 `YYYY-MM-DD`。
///
/// 对应上游 pydantic 的 `date` 解析：它接受 `1990-1-1` 之外的补零形式？
/// 不接受 —— pydantic v2 的 date 解析要求 `YYYY-MM-DD`。Rust 侧用
/// `NaiveDate::parse_from_str` 同义。
fn optional_date(raw: &Value, field: &str) -> Result<Option<NaiveDate>, ServiceError> {
    match raw {
        Value::Null => Ok(None),
        Value::String(text) => {
            // **先查形状再解析**：chrono 的 `%m` / `%d` 接受 1 位数字
            // （`1990-1-1` 能解析成功），而上游 pydantic 走 speedate，
            // 它的 full-date 要求定宽 `YYYY-MM-DD`。不补这道形状检查的话
            // 我们会接受一个上游拒绝的输入 —— 而生日会进 `age` 计算，
            // 两种解析结果可能差一天。
            let shaped = text.len() == 10
                && text.as_bytes()[4] == b'-'
                && text.as_bytes()[7] == b'-'
                && text
                    .as_bytes()
                    .iter()
                    .enumerate()
                    .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
            if !shaped {
                return Err(invalid_field(field, "birthday 必须是 YYYY-MM-DD 日期"));
            }
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(Some)
                .map_err(|_| invalid_field(field, "birthday 必须是 YYYY-MM-DD 日期"))
        }
        other => Err(invalid_field(
            field,
            &format!("必须是 YYYY-MM-DD 字符串或 null，收到 {}", kind_of(other)),
        )),
    }
}

/// JSON 值类型的名字，进错误消息。
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

/// 仓储行 → 视图。
fn view_of(row: &ActorListRow, today: NaiveDate) -> ActorView {
    let (actor, movie_count, image_id, image_origin) = row;
    ActorView {
        actor: actor.clone(),
        movie_count: *movie_count,
        image_id: *image_id,
        image_origin: image_origin.clone(),
        age: actor.age_on(today),
        manual_fields: manual_fields(actor),
    }
}

/// 归属为 `host:manual` 的字段名，升序。
///
/// 对应上游 `_actor_resource_payload` 里
/// `sorted(name for name, owner in field_owners.items() if owner == "host:manual")`。
/// 客户端用它高亮「这些是人工改过的，自动同步不会覆盖」。
pub fn manual_fields(actor: &Actor) -> Vec<String> {
    let Some(owners) = actor.field_owners.as_object() else {
        return Vec::new();
    };
    let mut names: Vec<String> = owners
        .iter()
        .filter(|(_, owner)| owner.as_str() == Some(MANUAL_FIELD_OWNER))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params() -> ActorListParams {
        ActorListParams {
            page: 1,
            page_size: 20,
            ..ActorListParams::default()
        }
    }

    #[test]
    fn offset_clamps_non_positive_pages_to_the_first_page() {
        // 上游：`start = max(page - 1, 0) * page_size`
        assert_eq!(params().offset(), 0);
        for page in [0, -1, -100] {
            let mut p = params();
            p.page = page;
            assert_eq!(p.offset(), 0, "page={page} 应夹到第一页");
        }
        let mut p = params();
        p.page = 3;
        p.page_size = 25;
        assert_eq!(p.offset(), 50);
    }

    #[test]
    fn page_and_page_size_are_echoed_verbatim() {
        // 上游 `PageResponse(page=page, page_size=page_size)` 不夹取，
        // 客户端靠「page=0 却拿到第一页」发现自己传错了参数。
        let mut p = params();
        p.page = 0;
        assert_eq!(p.page, 0);
        assert_eq!(p.offset(), 0);
    }

    #[test]
    fn page_size_above_one_hundred_is_not_rejected() {
        // 刻意不校验：上游允许 page_size=500，而 validate_page 会给 422。
        let mut p = params();
        p.page_size = 500;
        assert_eq!(p.page_size, 500);
    }

    #[test]
    fn sort_without_colon_is_rejected() {
        let err = parse_field_sort("movie_count").unwrap_err();
        assert_eq!(err.status, 422);
        assert_eq!(err.code(), INVALID_ACTOR_FILTER);
        assert_eq!(
            err.api.details.as_ref().unwrap().get("sort"),
            Some(&json!("movie_count"))
        );
    }

    #[test]
    fn sort_rejects_unknown_fields_and_directions() {
        for raw in [
            "nope:asc",
            "movie_count:up",
            "movie_count:",
            ":asc",
            "a:b:c",
        ] {
            assert!(parse_field_sort(raw).is_err(), "{raw:?} 应被拒绝");
        }
    }

    #[test]
    fn sort_accepts_every_documented_field() {
        for key in ActorSortKey::ALL {
            let raw = format!("{}:desc", key.name());
            let parsed = parse_field_sort(&raw).unwrap_or_else(|_| panic!("{raw} 应被接受"));
            assert_eq!(
                parsed,
                ActorSort::Field {
                    key,
                    descending: true
                }
            );
        }
    }

    #[test]
    fn sort_normalizes_case_and_whitespace_like_upstream() {
        // 上游 `value.strip().lower()`
        let parsed = parse_field_sort("  Movie_Count:ASC  ").unwrap();
        assert_eq!(
            parsed,
            ActorSort::Field {
                key: ActorSortKey::MovieCount,
                descending: false
            }
        );
    }

    #[test]
    fn blank_sort_falls_back_to_default_id_order() {
        assert_eq!(parse_sort(None, false).unwrap(), ActorSort::Default);
        assert_eq!(parse_sort(Some(""), false).unwrap(), ActorSort::Default);
        assert_eq!(parse_sort(Some("   "), false).unwrap(), ActorSort::Default);
    }

    #[test]
    fn search_terms_win_over_the_default_order() {
        // 有检索词又没给排序 -> 相关度，而不是 id ASC。
        let terms = vec!["aoi".to_owned()];
        assert!(matches!(
            sort_with_terms(None, &terms).unwrap(),
            ActorSort::SearchRelevance { .. }
        ));
        // 但显式给了排序就以排序为准。
        assert_eq!(
            sort_with_terms(Some("name:asc"), &terms).unwrap(),
            ActorSort::Field {
                key: ActorSortKey::Name,
                descending: false
            }
        );
    }

    #[test]
    fn age_range_inversion_is_rejected_with_both_bounds() {
        let mut p = params();
        p.age_min = Some(40);
        p.age_max = Some(20);
        let err = validate_ranges(&p).unwrap_err();
        assert_eq!(err.code(), INVALID_ACTOR_FILTER);
        let details = err.api.details.unwrap();
        assert_eq!(details.get("age_min"), Some(&json!(40)));
        assert_eq!(details.get("age_max"), Some(&json!(20)));
    }

    #[test]
    fn height_range_inversion_is_rejected_too() {
        let mut p = params();
        p.height_min = Some(200);
        p.height_max = Some(150);
        let err = validate_ranges(&p).unwrap_err();
        assert_eq!(err.code(), INVALID_ACTOR_FILTER);
        assert_eq!(
            err.api.details.unwrap().get("height_min"),
            Some(&json!(200))
        );
    }

    #[test]
    fn a_single_sided_range_is_always_valid() {
        let mut p = params();
        p.age_min = Some(40);
        assert!(validate_ranges(&p).is_ok());
        p.age_min = None;
        p.age_max = Some(20);
        assert!(validate_ranges(&p).is_ok());
    }

    #[test]
    fn an_absent_query_is_not_a_filter_but_an_empty_one_is_still_empty() {
        assert_eq!(split_terms(None).unwrap(), Vec::<String>::new());
        assert_eq!(split_terms(Some("")).unwrap(), Vec::<String>::new());
        assert_eq!(split_terms(Some("  ")).unwrap(), Vec::<String>::new());
        assert_eq!(
            split_terms(Some("aoi  空")).unwrap(),
            vec!["aoi".to_owned(), "空".to_owned()]
        );
    }

    #[test]
    fn too_many_terms_is_422_echoing_the_raw_input() {
        let raw = "a b c d e f g";
        let err = split_terms(Some(raw)).unwrap_err();
        assert_eq!(err.code(), INVALID_ACTOR_FILTER);
        assert_eq!(err.api.details.unwrap().get("query"), Some(&json!(raw)));
    }

    #[test]
    fn an_empty_update_is_rejected_before_touching_the_database() {
        let body = Map::new();
        assert!(parse_profile_changes(&body).unwrap().is_empty());
    }

    #[test]
    fn only_the_keys_present_in_the_body_are_changes() {
        // 这是 `exclude_unset` 的等价语义：`height_cm` 没出现就不动它。
        let body: Map<String, Value> = json!({ "height_cm": 165 }).as_object().unwrap().clone();
        let changes = parse_profile_changes(&body).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].column, "height_cm");
    }

    #[test]
    fn explicit_null_clears_a_size_but_not_gender() {
        let body: Map<String, Value> = json!({ "height_cm": null, "gender": null })
            .as_object()
            .unwrap()
            .clone();
        let err = parse_profile_changes(&body).unwrap_err();
        assert_eq!(
            err.api.details.unwrap().get("gender"),
            Some(&json!("gender 不能为 null，未知请使用 0"))
        );

        let ok_body: Map<String, Value> = json!({ "height_cm": null }).as_object().unwrap().clone();
        let changes = parse_profile_changes(&ok_body).unwrap();
        assert!(matches!(changes[0].value, ProfileValue::Int(None)));
    }

    #[test]
    fn gender_accepts_only_zero_one_or_two() {
        for good in [0, 1, 2] {
            let body: Map<String, Value> = json!({ "gender": good }).as_object().unwrap().clone();
            assert!(
                parse_profile_changes(&body).is_ok(),
                "gender={good} 应被接受"
            );
        }
        for bad in [3, -1] {
            let body: Map<String, Value> = json!({ "gender": bad }).as_object().unwrap().clone();
            assert!(
                parse_profile_changes(&body).is_err(),
                "gender={bad} 应被拒绝"
            );
        }
    }

    #[test]
    fn sizes_must_be_positive() {
        for bad in [0, -1] {
            let body: Map<String, Value> = json!({ "bust_cm": bad }).as_object().unwrap().clone();
            assert!(parse_profile_changes(&body).is_err(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn text_fields_are_trimmed_and_blanks_become_null() {
        let body: Map<String, Value> = json!({ "birthplace": "  上海  ", "blood_type": "   " })
            .as_object()
            .unwrap()
            .clone();
        let changes = parse_profile_changes(&body).unwrap();
        let by = |column: &str| {
            changes
                .iter()
                .find(|c| c.column == column)
                .map(|c| c.value.clone())
                .unwrap()
        };
        assert!(matches!(by("birthplace"), ProfileValue::Text(Some(v)) if v == "上海"));
        assert!(matches!(by("blood_type"), ProfileValue::Text(None)));
    }

    #[test]
    fn overlong_text_is_rejected() {
        let long = "x".repeat(256);
        let body: Map<String, Value> = json!({ "birthplace": long }).as_object().unwrap().clone();
        assert!(parse_profile_changes(&body).is_err());
        let ok: Map<String, Value> = json!({ "birthplace": "x".repeat(255) })
            .as_object()
            .unwrap()
            .clone();
        assert!(parse_profile_changes(&ok).is_ok());
    }

    #[test]
    fn cup_is_upper_cased_and_limited_to_four_ascii_letters() {
        let body: Map<String, Value> = json!({ "cup": " a " }).as_object().unwrap().clone();
        let changes = parse_profile_changes(&body).unwrap();
        assert!(matches!(&changes[0].value, ProfileValue::Text(Some(v)) if v == "A"));

        for bad in ["ABCD1", "中文", "ABCDE"] {
            let body: Map<String, Value> = json!({ "cup": bad }).as_object().unwrap().clone();
            assert!(parse_profile_changes(&body).is_err(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn birthday_must_be_iso_formatted() {
        let body: Map<String, Value> = json!({ "birthday": "1990-1-1" })
            .as_object()
            .unwrap()
            .clone();
        assert!(parse_profile_changes(&body).is_err());
        let ok: Map<String, Value> = json!({ "birthday": "1990-01-01" })
            .as_object()
            .unwrap()
            .clone();
        assert!(parse_profile_changes(&ok).is_ok());
    }

    #[test]
    fn unknown_keys_are_ignored_just_like_pydantic() {
        // 上游 SchemaModel 没开 extra="forbid"，pydantic 默认 ignore。
        // 所以 nickname 既不报错也不写入，最终落到 empty_actor_update。
        let body: Map<String, Value> = json!({ "nickname": "x" }).as_object().unwrap().clone();
        assert!(parse_profile_changes(&body).unwrap().is_empty());
    }

    #[test]
    fn guarded_but_not_protected_keys_would_be_rejected() {
        // `merged_into_id` 之类：pydantic 会 ignore，所以这条规则经 HTTP 不可达。
        // 直接构造 body 验证规则本身还在。
        let body: Map<String, Value> = json!({ "merged_into_id": 3 }).as_object().unwrap().clone();
        let err = parse_profile_changes(&body).unwrap_err();
        assert_eq!(err.code(), "invalid_actor_update");
        assert_eq!(
            err.api.details.unwrap().get("fields"),
            Some(&json!(["merged_into_id"]))
        );
    }

    #[test]
    fn manual_fields_are_sorted_and_filtered_by_owner() {
        let mut actor = sample_actor();
        actor.field_owners = json!({
            "cup": "host:manual",
            "birthday": "host:javdb",
            "bust_cm": "host:manual",
        });
        assert_eq!(manual_fields(&actor), vec!["bust_cm", "cup"]);
        actor.field_owners = json!({});
        assert!(manual_fields(&actor).is_empty());
    }

    fn sample_actor() -> Actor {
        Actor {
            id: 1,
            javdb_id: "n".to_owned(),
            name: "n".to_owned(),
            alias_name: String::new(),
            merged_into_id: None,
            profile_image_id: None,
            profile_image_override_id: None,
            display_name_override: None,
            javdb_type: 0,
            gender: 0,
            is_subscribed: false,
            subscribed_at: None,
            subscribed_movies_synced_at: None,
            subscribed_movies_full_synced_at: None,
            birthday: None,
            height_cm: None,
            bust_cm: None,
            waist_cm: None,
            hips_cm: None,
            cup: None,
            birthplace: None,
            blood_type: None,
            field_owners: json!({}),
            mutation_revision: 0,
            created_at: None,
            updated_at: None,
        }
    }
}
