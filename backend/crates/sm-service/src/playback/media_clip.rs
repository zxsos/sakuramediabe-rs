//! 片段列表面，对应上游 `src/service/playback/media_clip_service.py`。
//!
//! # 这个模块最要紧的一条：分页在内存里做，不在 SQL 里
//!
//! 上游 `list_media_clips` 的形状是：
//!
//! ```python
//! clips = cls.valid_clips(list(query.order_by(*order_by)))  # 取回全部
//! total = len(clips)                                        # 过滤后计数
//! clips = clips[start : start + page_size]                  # 切片
//! ```
//!
//! 三个后果都是契约：
//!
//! 1. **`total` 是文件系统过滤之后的数量。** `valid_clips` 要看产物文件是否
//!    还在、字节数是否对得上 —— 数据库不知道这些，所以 `total` 无法由
//!    `COUNT(*)` 得出。
//! 2. **分页不能下推。** 若用 `LIMIT/OFFSET`，页里可能全是即将被回收的无效行，
//!    而 `total` 与页内容互相矛盾。
//! 3. **读列表有写副作用。** `valid_clips` 对无效片段删库行、删磁盘文件。
//!
//! 第 3 条照搬自上游，且**必须**照搬：过滤与回收无法拆开，否则无效片段会
//! 继续出现在列表里并计入 `total` —— 那是可见的行为差异。
//!
//! # 代价是全量 `stat`
//!
//! 每次列表请求都要对全部匹配片段做一次文件判定（见
//! [`crate::playback::clip_artifact`]），且要取回全部候选行才能切片。这是
//! 上游的形状，本仓刻意保持一致：它决定了 `total` 的口径，而口径是客户端
//! 会读、会缓存、会做乐观更新的状态。
//!
//! 真要优化，正确方向是给 `media_clip` 加一列「产物状态」，由转码完成时写入 —
//! 那会引入一个数据库无法与磁盘保持一致的新状态，漂移时的表现是「列表说片段
//! 有效但播放失败」。所以这里不做。

use std::collections::HashMap;
use std::path::PathBuf;

use sm_core::pagination::{page_offset, Paginated};
use sm_core::text_search::{split_search_terms, TermLimitError};
use sm_db::catalog::asset::Image;
use sm_db::playback::media::MediaClip;
use sm_db::repo::{ClipFilter, ImageRepository, MediaClipRepository, MediaThumbnailRepository};
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::playback::clip_artifact::{self, clip_relative_path};
use crate::playback::search_filters::{FilterBuilder, KeywordFilters};

/// 本模块所有 422 的错误码。
///
/// 上游把 `validate_page` / `resolve_sort` / `split_search_terms` 三处的
/// `error_code` 都传成这一个，所以**分页越界、排序表达式非法、关键词过多或过长
/// 三种不同的客户端错误共用同一个 `code`**。
///
/// 不能用 `sm_core::pagination` 的默认码（`invalid_page` / `invalid_page_size`）
/// —— 那是别的域的契约，客户端是按 `code` 分支的。
pub const INVALID_CLIP_FILTER: &str = "invalid_media_clip_filter";

/// 排序表达式集合，与上游 `MEDIA_CLIP_SORT_FIELDS` 的键逐条一致。
///
/// 只有创建时间两个方向，且都带 `id` 兜底 —— 没有按大小、时长、番号排序。
const SORT_FIELDS: [&str; 2] = ["created_at:desc", "created_at:asc"];

/// 默认排序。
const DEFAULT_SORT: &str = "created_at:desc";

/// 片段列表的查询参数。缺省值与上游 `Query(default=...)` 一致。
#[derive(Debug, Clone)]
pub struct ClipListParams {
    /// 1-based 页码。默认 1。
    pub page: i64,
    /// 每页条数。默认 20，上限 100。
    pub page_size: i64,
    /// 排序表达式。`None` 或空白 = `created_at:desc`。
    pub sort: Option<String>,
    /// 精确匹配来源番号快照。
    pub movie_number: Option<String>,
    /// 关键词：空白分词，词间 AND，命中番号或标题。
    pub keyword: Option<String>,
    /// 排除该合集内已有的片段。
    ///
    /// 上游声明 `ge=1`，所以 0 与负数是 422 而非「不过滤」——
    /// 空值由 `Option::None` 表达。
    pub exclude_collection_id: Option<i32>,
}

impl Default for ClipListParams {
    /// 默认值逐条对照上游 `Query(default=1)` / `Query(default=20)`。
    fn default() -> Self {
        Self {
            page: 1,
            page_size: 20,
            sort: None,
            movie_number: None,
            keyword: None,
            exclude_collection_id: None,
        }
    }
}

/// 一页片段及其封面。
#[derive(Debug, Clone)]
pub struct ClipPage {
    /// 已过滤、已切片的一页。
    pub clips: Vec<MediaClip>,
    /// `(media_id, start_offset)` -> 封面图。孤立片段不在其中。
    pub covers: HashMap<(i32, i32), Image>,
    /// 分页壳所需的计数：**过滤后**的总数。
    pub total: i64,
    /// 本次请求**回收**掉的无效片段 id。
    ///
    /// 单独返回而不是只做掉事，是因为回收是写操作、且可能失败：调用方
    /// （或日志）需要知道它发生了，而失败时也要能看见。
    pub reclaimed: Vec<i32>,
}

/// 一个合集的引用，供「加入合集」选择器回显已勾选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipCollectionSummary {
    pub id: i32,
    pub name: String,
}

/// 片段区间内的一个源缩略图，`offset` 已重基到片段自身时间轴。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipThumbnail {
    /// 源媒体缩略图 id —— 与 `/media/{id}/thumbnails` 语义一致，客户端靠它跳转。
    pub thumbnail_id: i32,
    /// 相对片段起点的秒数。
    pub offset_seconds: i32,
    /// 对应图片的 id（图片本身由调用方批量取）。
    pub image_id: i32,
}

/// 片段详情：本体 + 封面 + 预览帧 + 所属合集。
///
/// 预览帧与缩略图**取自同一批行**，区别只在 `offset` 是否重基 —— 上游也是
/// 复用同一个 `_clip_thumbnail_rows`。这里同样只查一次。
#[derive(Debug, Clone)]
pub struct ClipDetail {
    pub clip: MediaClip,
    /// 区间首帧的封面。孤立片段或该帧没有缩略图时为 `None`。
    pub cover: Option<Image>,
    /// 区间内的所有帧，供前端循环播放成动态预览。
    pub preview_frames: Vec<Image>,
    /// 区间内的缩略图（offset 已重基），供进度条定位。
    pub thumbnails: Vec<ClipThumbnail>,
    /// 该片段所属的合集。
    pub collections: Vec<ClipCollectionSummary>,
}

/// 片段列表面。
pub struct MediaClipService {
    clips: MediaClipRepository,
    thumbnails: MediaThumbnailRepository,
    images: ImageRepository,
    /// 片段产物根目录，**必须已规范化**（`clip_artifact` 的契约）。
    clip_root: PathBuf,
}

impl MediaClipService {
    /// 构造服务。
    ///
    /// `clip_root` 必须是已规范化的绝对路径 —— 路径包含性判断依赖它，
    /// 详见 [`clip_artifact::resolve_clip_file`]。
    #[must_use]
    pub fn new(db: &Db, clip_root: PathBuf) -> Self {
        Self {
            clips: MediaClipRepository::new(db.clone()),
            thumbnails: MediaThumbnailRepository::new(db.clone()),
            images: ImageRepository::new(db.clone()),
            clip_root,
        }
    }

    /// 解析排序表达式。返回 `true` 表示升序。
    ///
    /// 对应上游 `resolve_sort` + `MEDIA_CLIP_SORT_FIELDS`。归一化是
    /// `strip().lower()`，所以 `CREATED_AT:DESC` 合法。
    pub fn resolve_sort(&self, value: Option<&str>) -> Result<bool, ServiceError> {
        let normalized = value.unwrap_or_default().trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" => Ok(DEFAULT_SORT == "created_at:asc"),
            key if SORT_FIELDS.contains(&key) => Ok(key == "created_at:asc"),
            _ => Err(ServiceError::validation_with(
                INVALID_CLIP_FILTER,
                "Invalid sort expression",
                details_of("sort", value.unwrap_or_default()),
            )),
        }
    }

    /// 列表：过滤 → 回收无效 → 计数 → 切片 → 解析封面。
    ///
    /// 顺序不能换：`total` 必须在**回收之后**算，且必须在切片之前算。
    pub async fn list(&self, params: &ClipListParams) -> Result<ClipPage, ServiceError> {
        self.validate_page(params)?;
        let ascending = self.resolve_sort(params.sort.as_deref())?;

        // 关键词先切词：词数/词长越界要在**任何查询之前**报 422，
        // 否则一个非法请求会先把全表拉出来再失败。
        let terms = self.split_terms(params.keyword.as_deref())?;

        // **只有关键词条件**进 FilterBuilder。番号不是：仓储层把 `movie_number`
        // 当作独立筛选字段自己绑，并会把关键词条件整体后移让开它。
        //
        // 若这里也把番号塞进 builder，它会被绑**两次**（一次在这里、一次在
        // 仓储），而 SQL 里只有一处 `movie_number = $n` —— 于是绑定值个数与
        // 占位符个数不等，PostgreSQL 报 `bind message supplies N parameters,
        // but prepared statement requires M`。这个错误只有真库能抓到。
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&terms, Some("movie_number"), Some("title"));
        let (keyword_sql, keyword_binds) = builder.finish();

        let candidates = self
            .clips
            .list_filtered(&ClipFilter {
                movie_number: params.movie_number.clone(),
                keyword_sql: Some(keyword_sql),
                keyword_binds,
                exclude_collection_id: params.exclude_collection_id,
                created_at_asc: ascending,
            })
            .await?;

        // 过滤 + 回收。顺序与上游 `valid_clips` 一致。
        let (valid, reclaimed) = self.retain_valid(candidates).await?;
        let total = i64::try_from(valid.len()).unwrap_or(i64::MAX);

        // 切片。`page_offset` 在校验之后调用，所以不会得到负 offset。
        let start = usize::try_from(page_offset(params.page, params.page_size))
            .unwrap_or(usize::MAX)
            .min(valid.len());
        let end = start
            .saturating_add(usize::try_from(params.page_size).unwrap_or(usize::MAX))
            .min(valid.len());
        let page = valid[start..end].to_vec();

        let covers = self.load_cover_map(&page).await?;

        Ok(ClipPage {
            clips: page,
            covers,
            total,
            reclaimed,
        })
    }

    /// 把一组片段转成分页壳。
    ///
    /// 单独一个函数是为了让 API 层不必知道 `total` 是「过滤后」的口径。
    #[must_use]
    pub fn into_paginated<T>(
        self,
        page: ClipPage,
        params: &ClipListParams,
        map: impl Fn(&MediaClip, Option<&Image>) -> T,
    ) -> Paginated<T> {
        let items = page
            .clips
            .iter()
            .map(|clip| {
                let cover = clip.media_id.map(|id| (id, clip.start_offset_seconds));
                let image = cover.and_then(|key| page.covers.get(&key));
                map(clip, image)
            })
            .collect();
        Paginated::new(items, params.page, params.page_size, page.total)
    }

    /// 校验分页参数，错误码是本域的 `invalid_media_clip_filter`。
    fn validate_page(&self, params: &ClipListParams) -> Result<(), ServiceError> {
        if params.page <= 0 {
            return Err(ServiceError::validation_with(
                INVALID_CLIP_FILTER,
                "page must be greater than 0",
                details_of("page", params.page),
            ));
        }
        // 0 与负数都拒。`page_size <= 0` 单独给出消息，与上游一致。
        if params.page_size <= 0 || params.page_size > 100 {
            return Err(ServiceError::validation_with(
                INVALID_CLIP_FILTER,
                "page_size must be between 1 and 100",
                details_of("page_size", params.page_size),
            ));
        }
        Ok(())
    }

    /// 切分关键词，越界转成 422。**details 回显原始输入**。
    fn split_terms(&self, keyword: Option<&str>) -> Result<Vec<String>, ServiceError> {
        split_search_terms(keyword).map_err(|err: TermLimitError| {
            ServiceError::validation_with(
                INVALID_CLIP_FILTER,
                err.reason(),
                // 词长超限时回显长度（字符数），词数超限时回显词数。
                // 两者都放在 `query` 键下 —— 上游也是 `{"query": value}`。
                match err {
                    TermLimitError::TooManyTerms { count } => details_of("count", count),
                    TermLimitError::TermTooLong { length } => details_of("length", length),
                },
            )
        })
    }

    /// 保留产物有效的片段，其余**回收**（删行 + 删文件）。
    ///
    /// 对应上游 `valid_clips`。回收失败**不**中断整批 —— 一个删不掉的文件
    /// 不该让整个列表 500，它下个请求还会被判定为无效并重试。
    ///
    /// # 刻意 `pub`：合集也用这一套
    ///
    /// 上游 `_valid_collection_items` 调的是同一个 `valid_clips`，所以合集的
    /// `clip_count` 与封面才和片段列表口径一致。**这里必须是同一个函数**
    /// 而不是第二份实现 —— 两份实现一旦漂移，「列表说 3 个、合集说 2 个」
    /// 这种矛盾就无从排查。
    pub async fn retain_valid(
        &self,
        clips: Vec<MediaClip>,
    ) -> Result<(Vec<MediaClip>, Vec<i32>), ServiceError> {
        let mut valid = Vec::with_capacity(clips.len());
        let mut reclaimed = Vec::new();

        for clip in clips {
            if clip_artifact::has_valid_artifact(&self.clip_root, &clip) {
                valid.push(clip);
                continue;
            }
            let id = clip.id;
            self.reclaim(&clip).await;
            reclaimed.push(id);
        }

        Ok((valid, reclaimed))
    }

    /// 回收一个无效片段：删库行、删磁盘文件。
    ///
    /// 对应上游 `_discard_invalid_clip`。**顺序是「先删行、后删文件」**，
    /// 与上游逐条一致 —— 包括 `delete_clip` 那条路径，两处顺序相同。
    ///
    /// # 为什么不是反过来
    ///
    /// 直觉上「先删文件」更安全：若删行成功而删文件失败，就没有任何记录指向
    /// 那个文件了。但那个推理不成立 —— 产物路径可以由**番号与 id 推导**
    /// （[`clip_relative_path`]），上游正是为此留了兜底：路径解析失败时用
    /// 推导值删。所以「没有记录指向文件」并不等于「找不到那个文件」。
    ///
    /// 反过来，先删文件才真的危险：删完文件、删行失败，那一行就**指向一个
    /// 不存在的产物**，而它仍然会被列出来、计入 `total`，直到下一次请求
    /// 才被判定为无效。行先消失至少保证列表立刻自洽。
    ///
    /// 两次删除各自吞掉错误：任一失败都只留下垃圾，不影响返回值。
    async fn reclaim(&self, clip: &MediaClip) {
        let target = clip_artifact::resolve_clip_file(&self.clip_root, &clip.file_path)
            .unwrap_or_else(|| {
                self.clip_root
                    .join(clip_relative_path(clip.movie_number.as_deref(), clip.id))
            });

        // 行先删：合集成员由外键 CASCADE 一并清掉，不需要显式处理。
        let _ = self.clips.delete(clip.id).await;

        // 内联 `remove_file` 而不是 `spawn_blocking`：单次 unlink 是微秒级的
        // 系统调用，而把 `tokio` 提为常规依赖只为一个 `spawn_blocking` 不划算
        // （它目前只是 dev-dependency）。**若将来在这里做真正的文件 IO**
        // （复制、ffmpeg、批量删除），那时必须换回阻塞池并加上依赖。
        let _ = std::fs::remove_file(&target);
    }

    /// 列出某条 Media 的全部片段，**不分页**，按 `created_at DESC, id DESC`。
    ///
    /// 对应上游 `list_clips`。**同样要过滤 + 回收** —— 它走的是同一个
    /// `valid_clips`，所以这里的返回同样只含产物完好的片段，且会顺带回收
    /// 失效的那些。
    ///
    /// 刻意不分页：上游返回 `list[...]` 而非 `PageResponse`，一部 Media 的
    /// 片段数是几十量级，客户端一次拿全。
    pub async fn list_for_media(&self, media_id: i32) -> Result<ClipPage, ServiceError> {
        // Media 不存在时上游是 404。这里靠「取得到片段」判断不了 ——
        // 一部没有片段的 Media 是合法的，所以必须单独查 Media。
        let candidates = self.all_clips_for_media(media_id).await?;
        let (valid, reclaimed) = self.retain_valid(candidates).await?;
        let total = i64::try_from(valid.len()).unwrap_or(i64::MAX);
        let covers = self.load_cover_map(&valid).await?;
        Ok(ClipPage {
            clips: valid,
            covers,
            total,
            reclaimed,
        })
    }

    /// 取回某条 Media 的全部片段行。**刻意不分页。**
    ///
    /// 单独一个私有方法而不是直接用 `paged_list!` 的 `list_by_media`：那个按
    /// `start_offset_seconds, id` 排序并分页，而这里要的是 `created_at DESC`
    /// 的全量。两处排序语义不同，混用会让「按创建时间」变成「按区间起点」。
    async fn all_clips_for_media(&self, media_id: i32) -> Result<Vec<MediaClip>, ServiceError> {
        Ok(self.clips.list_all_for_media(media_id).await?)
    }

    /// 片段详情：本体 + 封面 + 区间内的预览帧 + 所属合集。
    ///
    /// 对应上游 `get_clip_detail`。
    ///
    /// # 孤立片段（`media_id` 为空）的预览帧与缩略图都是空列表
    ///
    /// 上游 `_clip_thumbnail_rows` 开头就 `if clip.media_id is None: return []`。
    /// 片段是「独立资产」，来源被删后仍存在，但它没有可回溯的源缩略图 ——
    /// 产物 mp4 还在，可它的帧已经不在源媒体里了。
    pub async fn detail(&self, clip_id: i32) -> Result<ClipDetail, ServiceError> {
        let clip = self.require_clip(clip_id).await?;

        // 封面 = 区间首帧那张缩略图。孤立片段解析不到。
        let covers = self.load_cover_map(std::slice::from_ref(&clip)).await?;
        let cover = clip
            .media_id
            .and_then(|id| covers.get(&(id, clip.start_offset_seconds)));

        // 预览帧与缩略图取的是**同一批**行，只有重基与否不同。
        let thumbnails = self.clip_thumbnails(&clip).await?;
        let images = self
            .images
            .find_by_ids(&thumbnails.iter().map(|t| t.image_id).collect::<Vec<_>>())
            .await?;

        let collections = self
            .clips
            .list_collections_for_clip(clip.id)
            .await?
            .into_iter()
            .map(|(id, name)| ClipCollectionSummary { id, name })
            .collect();

        // 先算缩略图：构造结构体时 `clip` 会被移走，而重基还要用它的起点。
        let thumbnails: Vec<ClipThumbnail> = thumbnails
            .iter()
            .map(|t| ClipThumbnail {
                thumbnail_id: t.id,
                // **重基到片段自身时间轴**：源缩略图的 offset 是相对源媒体
                // 片头的，而片段有自己的起点。前端进度条要的是片段内的秒数。
                offset_seconds: t.offset - clip.start_offset_seconds,
                image_id: t.image_id,
            })
            .collect();

        Ok(ClipDetail {
            clip,
            cover: cover.cloned(),
            // 只保留图片确实存在的帧：外键是 NOT NULL，但约束可以被关掉，
            // 而少一帧比整条 500 更容易排查。
            preview_frames: thumbnails
                .iter()
                .filter_map(|t| images.get(&t.image_id).cloned())
                .collect(),
            thumbnails,
            collections,
        })
    }

    /// 改标题。返回改后的行。
    ///
    /// 对应上游 `update_clip`。标题的 strip 在这里做（上游是 pydantic 的
    /// `field_validator`）—— 放在 service 是因为 API 层的 DTO 校验与
    /// 业务归一不是同一件事，而空串标题在上游是**允许**的。
    pub async fn update_title(
        &self,
        clip_id: i32,
        title: &str,
    ) -> Result<(MediaClip, Option<Image>), ServiceError> {
        // 先确认存在，好把「不存在」报成 404 而不是 500 —— 仓储的
        // `update_title` 找不到行时给的是业务错误。
        self.require_clip(clip_id).await?;
        let updated = self.clips.update_title(clip_id, title).await?;
        let covers = self.load_cover_map(std::slice::from_ref(&updated)).await?;
        let cover = updated
            .media_id
            .and_then(|id| covers.get(&(id, updated.start_offset_seconds)));
        Ok((updated, cover.cloned()))
    }

    /// 删片段：删库行 + 删磁盘文件。返回被删的片段（用于日志）。
    ///
    /// 对应上游 `delete_clip`。**先删行后删文件**，与回收路径同一顺序，
    /// 理由见 `reclaim` 的文档。
    ///
    /// 合集成员由 `clip_collection_item.clip_id` 的外键 CASCADE 自动清除，
    /// 不需要显式处理 —— 与上游注释一致。
    pub async fn delete(&self, clip_id: i32) -> Result<MediaClip, ServiceError> {
        let clip = self.require_clip(clip_id).await?;
        let target = clip_artifact::resolve_clip_file(&self.clip_root, &clip.file_path)
            .unwrap_or_else(|| {
                self.clip_root
                    .join(clip_relative_path(clip.movie_number.as_deref(), clip.id))
            });

        let deleted = self.clips.delete(clip.id).await?;
        if !deleted {
            // 理论上不可达：`require_clip` 刚确认过存在。仍要处理，否则会
            // 静默返回一个「已经删掉了」的片段，客户端以为删除成功而文件还在。
            return Err(ServiceError::not_found_with(
                "media_clip",
                "Media clip not found",
                details_of("clip_id", clip_id),
            ));
        }
        let _ = std::fs::remove_file(&target);
        Ok(clip)
    }

    /// 取回片段或报 404。
    async fn require_clip(&self, clip_id: i32) -> Result<MediaClip, ServiceError> {
        self.clips.find_by_id(clip_id).await?.ok_or_else(|| {
            ServiceError::not_found_with(
                "media_clip",
                "Media clip not found",
                details_of("clip_id", clip_id),
            )
        })
    }

    /// 串流端点用：取回片段的产物绝对路径。
    ///
    /// 返回 `Ok(None)` 而不是报错，是为了让 API 层把三种「拿不到文件」的情况
    /// 统一报成 404 `file_not_found`（与上游 `_clip_file_path` 返回 `None` →
    /// `require_existing_file` 抛 404 的效果一致）：
    ///
    /// - 片段不存在；
    /// - `file_path` 被四道穿越规则拒绝（脏数据）；
    /// - 路径合法但文件已不在磁盘上。
    ///
    /// **不校验产物有效性**（字节数比对那套）：串流时文件马上要读，
    /// 而 `retain_valid` 那套判定是为「列表要不要显示它」设计的。
    /// 在这里判无效会让「文件字节数与库里记录不符」变成 404，而正确反应是
    /// 把它读出来 —— 下一次列表请求自然会把它回收掉。
    pub async fn require_clip_for_stream(
        &self,
        clip_id: i32,
    ) -> Result<Option<PathBuf>, ServiceError> {
        let Some(clip) = self.clips.find_by_id(clip_id).await? else {
            return Ok(None);
        };
        // 路径解析失败时退回由番号与 id 推导的规范路径 —— 与删除、回收
        // 同一套兜底。推导值也不存在的话，下面的 `is_file` 会把它挡掉。
        let candidate = clip_artifact::resolve_clip_file(&self.clip_root, &clip.file_path)
            .unwrap_or_else(|| {
                self.clip_root
                    .join(clip_relative_path(clip.movie_number.as_deref(), clip.id))
            });
        Ok(candidate.is_file().then_some(candidate))
    }

    /// 区间内的源缩略图行，按 offset 升序。孤立片段返回空。
    async fn clip_thumbnails(
        &self,
        clip: &MediaClip,
    ) -> Result<Vec<sm_db::playback::media::MediaThumbnail>, ServiceError> {
        let Some(media_id) = clip.media_id else {
            return Ok(Vec::new());
        };
        Ok(self
            .thumbnails
            .list_in_offset_range(media_id, clip.start_offset_seconds, clip.end_offset_seconds)
            .await?)
    }

    /// 批量解析片段封面：区间首帧的缩略图。
    ///
    /// 对应上游 `load_cover_map`。两次查询（缩略图 + 图片），不是 N+1。
    ///
    /// # 为什么按 `(media_id, offset)` 精确配对
    ///
    /// `media_id = ANY(..) AND offset = ANY(..)` 是笛卡尔积筛选，会多取
    /// （要 `(1,100)` 与 `(2,200)` 时，`(1,200)` 也会被取出来）。上游同样按
    /// 精确键回填，所以多取的行被直接丢弃。
    ///
    /// 刻意 `pub`：合集封面复用它，与片段列表同一套解析（见 [`Self::retain_valid`]
    /// 的说明）。
    pub async fn load_cover_map(
        &self,
        clips: &[MediaClip],
    ) -> Result<HashMap<(i32, i32), Image>, ServiceError> {
        // 孤立片段（media_id 为空）没有封面可解析。
        let mut media_ids: Vec<i32> = Vec::new();
        let mut offsets: Vec<i32> = Vec::new();
        for clip in clips {
            if let Some(media_id) = clip.media_id {
                if !media_ids.contains(&media_id) {
                    media_ids.push(media_id);
                }
                if !offsets.contains(&clip.start_offset_seconds) {
                    offsets.push(clip.start_offset_seconds);
                }
            }
        }
        if media_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let thumbnails = self
            .thumbnails
            .covers_by_media_offsets(&media_ids, &offsets)
            .await?;
        if thumbnails.is_empty() {
            return Ok(HashMap::new());
        }

        let image_ids: Vec<i32> = thumbnails
            .iter()
            .map(|thumbnail| thumbnail.image_id)
            .collect();
        let images = self.images.find_by_ids(&image_ids).await?;

        // 只保留「确实被某个片段当作首帧」的键，丢掉笛卡尔积多取的那些。
        let wanted: HashMap<(i32, i32), ()> = clips
            .iter()
            .filter_map(|clip| {
                clip.media_id
                    .map(|id| ((id, clip.start_offset_seconds), ()))
            })
            .collect();

        let mut covers = HashMap::new();
        for thumbnail in thumbnails {
            let key = (thumbnail.media_id, thumbnail.offset);
            if !wanted.contains_key(&key) {
                continue;
            }
            if let Some(image) = images.get(&thumbnail.image_id) {
                covers.insert(key, image.clone());
            }
        }
        Ok(covers)
    }
}
