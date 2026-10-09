//! 非 JAV 视频（`VideoItem` / `VideoCollection`）的 service。
//!
//! 对应上游 `src/service/videos/`（4 个文件，927 行）：
//!
//! | 上游文件 | 行数 | 本模块 | 落在哪 |
//! |---|---|---|---|
//! | `video_item_service.py` | 436 | [`item::VideoItemService`] | [`item`] |
//! | `video_collection_service.py` | 389 | [`collection::VideoCollectionService`] | [`collection`] |
//! | `video_cover_service.py` | 95 | **不落地**，见下 | — |
//! | `__init__.py` | 7 | 无逻辑 | — |
//!
//! # 这一批只落「规则」，不落「查询编排」
//!
//! `VideoItemService.list_videos` 与 `VideoCollectionService.list_collection_items`
//! 的主体是 Peewee 表达式树：每条目 `MIN(Media.id)` 分组子查询 + 三次
//! `LEFT JOIN` + `COALESCE` 取第一条媒体的时长/大小，再分两次批量回填
//! 媒体统计与合集引用（避免 N+1）。按 [`crate`] 的分层约定，那部分应当
//! 直接写 SQL 放进仓储，而不是照搬表达式树的形状 —— 所以它不在本批。
//!
//! # 刻意不复刻的三处
//!
//! **① 播放地址与 `can_play`。** `_media_items` 与
//! `_query_item_resources` 都要 `MEDIA_PROVIDER_REGISTRY.require(provider_key)`
//! 拿 `playback_deliveries[0]` 才能签出播放地址。插件 ABI（gRPC）还没接，
//! 这里**不**用假 provider 顶替 —— 契约由错误码与字段位置决定，值等
//! 插件落地后自然接上。
//!
//! **② 首帧封面生成。** `video_cover_service` 依赖 PyAV 解码第 0 帧、写
//! `videos/<id>/cover/0.webp`。它是 `svc-image`（有损 WebP 目前走进程外
//! `cwebp`，见 ADR §3.4）与 `svc-probe` 的职责，不属于 service 层。
//!
//! **③ 换封面时旧图片的回收。** `update` 换掉 `video_item.cover_image_id`
//! 之后，那张旧图可能已经没人引用，该调 `ImageCleanupService` 删掉它的行与
//! 磁盘文件 —— 这一步**还没做**（本批只改 id，旧图行会留下）。
//!
//! ⚠️ 别把这一条读成「删条目也不回收封面」：删条目那条链路**已经**会回收
//! （[`VideoItemService::delete`](item::VideoItemService::delete) 删完条目行
//! 之后调 `MediaService::reap_images`）。两者的区别是「引用还在不在」——
//! 换封面时那张图可能还被**别的**媒体或影片用着，而删条目之后它一定没人用了。
//!
//! # 三条容易搞反的规则
//!
//! **`remove_item` 里的 `item_id` 是关联行 id，不是视频 id。** 上游
//! `VideoCollectionItem.id == item_id`，而 `remove_items_by_video_ids`
//! 才按 `video_item_id` 删。两者取值空间相同、外观相同，混淆不会报错，
//! 只会删掉错的行。见 [`collection`] 与 `sm_db::repo::VideoCollectionItemRepository`。
//!
//! **`add_item` 重复加入是幂等返回，不是 409。** 上游先查后插，撞唯一
//! 约束时也只 `return`。UI 上连点两下「加入」是常态，拿约束违例当业务
//! 结果没有意义。
//!
//! **`update_video` 里三个字段对「显式 null」的处理各不相同。** 见
//! [`Field`]：title/summary 的 null 被忽略，`release_date` 的 null 是**清空**。
//! 而 `{"title": null}` 这种只带 null 的更新**不算空更新** —— 它会通过
//! 空更新检查然后只推进 `updated_at`。

pub mod collection;
pub mod item;

use serde_json::{Map, Value};

use crate::error::{details_of, ServiceError};

pub use collection::{Added, VideoCollectionService, VideoCollectionUpdate};
pub use item::{
    VideoCollectionRef, VideoItemCreate, VideoItemService, VideoItemUpdate, VideoListItem,
};

/// 分页与筛选类校验的错误码。
///
/// 与 `playlist` 域的 `validation_error` 不同：videos 域的列表端点把
/// 分页、搜索词、排序**三类**校验都归到同一个码（上游两处
/// `validate_page(..., error_code="invalid_video_filter")` 与
/// `resolve_sort_expression(..., error_code="invalid_video_filter")`），
/// 客户端按这一个码分支提示「筛选条件有问题」。
pub const INVALID_VIDEO_FILTER: &str = "invalid_video_filter";

/// 分页校验。违规 → 422 [`INVALID_VIDEO_FILTER`]。
///
/// 消息与 `details` 逐字对齐上游 `validate_page`（`sm_core::pagination`
/// 已经把两条消息与 details 形状编码成 [`sm_core::pagination::PageError`]，
/// 这里只负责换错误码）。
pub fn validate_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
    sm_core::pagination::validate_page(page, page_size).map_err(|err| {
        let details = match err.details() {
            Value::Object(map) => map,
            // `PageError::details` 只会返回对象；不是对象时不编造形状。
            other => {
                let mut map = Map::new();
                map.insert("page".to_owned(), other);
                map
            }
        };
        ServiceError::validation_with(INVALID_VIDEO_FILTER, err.message(), details)
    })
}

/// 搜索词归一。**给出但归一后为空** → 422 [`INVALID_VIDEO_FILTER`]。
///
/// 上游 `_filtered_query` 只在 `query is not None` 时才校验，所以
/// 「没传 query」与「传了空 query」是两种结果：前者不过滤，后者 422。
/// 这个区别是刻意的 —— `?query=` 出现在 URL 里几乎总是前端拼串出错。
pub fn normalize_query(query: Option<&str>) -> Result<Option<String>, ServiceError> {
    match query {
        None => Ok(None),
        Some(raw) => {
            let normalized = raw.trim();
            if normalized.is_empty() {
                return Err(ServiceError::validation_with(
                    INVALID_VIDEO_FILTER,
                    "Invalid video filter",
                    details_of("query", raw),
                ));
            }
            Ok(Some(normalized.to_owned()))
        }
    }
}

/// 排序字段。
///
/// `Duration` 与 `FileSize` 排的不是本表列，而是**第一条有效媒体**的
/// `duration_seconds` / `file_size_bytes`（`Media.id` 最小且 `valid`），
/// 无媒体时按 0 参与排序 —— 上游用 `COALESCE(..., 0)` 保证空媒体的条目
/// 位置稳定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSort {
    /// `video_item.created_at`
    CreatedAt,
    /// `video_item.title`
    Title,
    /// 首条有效媒体的时长
    Duration,
    /// 首条有效媒体的文件大小
    FileSize,
    /// `video_collection_item.position`。**仅合集成员列表可用。**
    Position,
}

impl VideoSort {
    /// `field:direction` 里的 `field`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::Title => "title",
            Self::Duration => "duration",
            Self::FileSize => "file_size",
            Self::Position => "position",
        }
    }

    /// 由 `field` 反查。**调用方负责校验它在白名单内** ——
    /// 所以这里只用于错误消息与文档，不承担放行责任。
    pub fn from_field(field: &str) -> Option<Self> {
        match field {
            "created_at" => Some(Self::CreatedAt),
            "title" => Some(Self::Title),
            "duration" => Some(Self::Duration),
            "file_size" => Some(Self::FileSize),
            "position" => Some(Self::Position),
            _ => None,
        }
    }
}

/// 排序方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// 一次通过校验的排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub key: VideoSort,
    pub direction: SortDirection,
}

impl SortSpec {
    pub const fn new(key: VideoSort, direction: SortDirection) -> Self {
        Self { key, direction }
    }
}

/// 条目列表允许的排序键。上游 `_build_video_order` 的 `columns` 字典。
pub const ITEM_SORT_KEYS: &[VideoSort] = &[
    VideoSort::CreatedAt,
    VideoSort::Title,
    VideoSort::Duration,
    VideoSort::FileSize,
];

/// 合集成员列表允许的排序键。= 条目列表 + `position`。
pub const COLLECTION_ITEM_SORT_KEYS: &[VideoSort] = &[
    VideoSort::Position,
    VideoSort::CreatedAt,
    VideoSort::Title,
    VideoSort::Duration,
    VideoSort::FileSize,
];

/// 条目列表的默认排序：最新在前。
pub const DEFAULT_ITEM_SORT: SortSpec = SortSpec::new(VideoSort::CreatedAt, SortDirection::Desc);

/// 合集成员列表的默认排序：手动顺序（前端据此顺序播放）。
pub const DEFAULT_COLLECTION_ITEM_SORT: SortSpec =
    SortSpec::new(VideoSort::Position, SortDirection::Asc);

/// 解析 `field:direction`。非法 → 422 [`INVALID_VIDEO_FILTER`]。
///
/// 逐条对齐上游 `resolve_sort_expression`：
///
/// | 输入 | 结果 |
/// |---|---|
/// | `None` / 空白 | `default` |
/// | `"  Title : ASC "` | `title:asc`（整体 `strip()` + `lower()`） |
/// | `"title"`（缺 `:`） | 422 |
/// | `"title:up"` | 422（方向只认 `asc` / `desc`） |
/// | `"file_size:asc"` | 条目列表 422（键不在白名单内） |
///
/// 大小写不敏感是有意义的：前端把用户输入原样透传，而
/// `"Title:ASC"` 与 `"title:asc"` 表达同一个意图。
pub fn parse_sort(
    value: Option<&str>,
    allowed: &[VideoSort],
    default: SortSpec,
) -> Result<SortSpec, ServiceError> {
    let Some(raw) = value else {
        return Ok(default);
    };
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(default);
    }
    let invalid = || {
        ServiceError::validation_with(
            INVALID_VIDEO_FILTER,
            "Invalid sort expression",
            details_of("sort", raw),
        )
    };
    let Some((field, direction)) = normalized.split_once(':') else {
        return Err(invalid());
    };
    // 上游 `split(":", 1)`：`"a:b:c"` 的方向部分是 `"b:c"`，不在白名单里。
    let direction = match direction {
        "asc" => SortDirection::Asc,
        "desc" => SortDirection::Desc,
        _ => return Err(invalid()),
    };
    let key = VideoSort::from_field(field).filter(|k| allowed.contains(k));
    match key {
        Some(key) => Ok(SortSpec::new(key, direction)),
        None => Err(invalid()),
    }
}

/// 一个字段的「给出方式」。
///
/// # 为什么不是 `Option<T>`
///
/// 上游的更新走 `payload.model_dump(exclude_unset=True)`，所以要区分
/// **三**种状态，而 `Option<T>` 只有两种。三者在 `title` 上就能看到差别：
///
/// ```python
/// update_data = payload.model_dump(exclude_unset=True)
/// if not update_data:            # 空更新 → 422
///     raise ApiError(422, "validation_error", ...)
/// if "title" in update_data and update_data["title"] is not None:  # null 被忽略
///     video.title = update_data["title"]
/// ```
///
/// - `{"title": "新"}` → 改标题
/// - `{"title": null}` → **非空更新**（过了空更新检查），但标题不变，
///   只推进 `updated_at`
/// - `{}` → 422 `validation_error`
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Field<T> {
    /// 没给这个字段。
    #[default]
    Absent,
    /// 给了，且是 `null`。
    Null,
    /// 给了具体值。
    Value(T),
}

impl<T> Field<T> {
    /// 没给。
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// 给了（不论是不是 `null`）。
    ///
    /// 空更新判定用的就是这个：上游判的是「有没有 key」，
    /// 不是「有没有非 null 的值」。
    pub fn is_given(&self) -> bool {
        !self.is_absent()
    }

    /// 取值。`Absent` 与 `Null` 都给 `None` —— 两者在上游那些
    /// `is not None` 的判断里**行为相同**。
    pub fn as_value(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sort_err(value: &str) -> ServiceError {
        parse_sort(Some(value), ITEM_SORT_KEYS, DEFAULT_ITEM_SORT).unwrap_err()
    }

    #[test]
    fn blank_sort_falls_back_to_the_default() {
        // 上游 `(sort or "").strip() or default_sort`：空白等同没给。
        for blank in [None, Some(""), Some("   "), Some("\t\n")] {
            assert_eq!(
                parse_sort(blank, ITEM_SORT_KEYS, DEFAULT_ITEM_SORT).unwrap(),
                DEFAULT_ITEM_SORT
            );
        }
    }

    #[test]
    fn sort_is_case_insensitive_and_trimmed() {
        assert_eq!(
            parse_sort(Some("  Title:ASC  "), ITEM_SORT_KEYS, DEFAULT_ITEM_SORT).unwrap(),
            SortSpec::new(VideoSort::Title, SortDirection::Asc)
        );
        assert_eq!(
            parse_sort(Some("file_size:desc"), ITEM_SORT_KEYS, DEFAULT_ITEM_SORT).unwrap(),
            SortSpec::new(VideoSort::FileSize, SortDirection::Desc)
        );
    }

    #[test]
    fn spaces_around_the_colon_are_rejected_like_upstream() {
        // 上游只对**整体**做 strip()，然后 `split(":", 1)`。所以冒号两侧的
        // 空格会留在字段名/方向里，既不在白名单也不在 {asc, desc} 里 → 422。
        // 这看着像可以宽容一点的地方，但放宽会让「上游 422 的输入」在 Rust
        // 侧变成 200，客户端的分支就与上游不一致了。
        let err = sort_err("Title : ASC");
        assert_eq!((err.status, err.code()), (422, INVALID_VIDEO_FILTER));
    }

    #[test]
    fn invalid_sort_is_a_422_with_the_original_value_in_details() {
        for bad in [
            "title",
            "title:up",
            "title:asc:desc",
            "nope:asc",
            "created_at:",
        ] {
            let err = sort_err(bad);
            assert_eq!(err.status, 422, "输入 {bad:?} 应是 422");
            assert_eq!(err.code(), INVALID_VIDEO_FILTER);
            // details 回显**原始**输入而不是归一后的 —— 客户端要显示用户
            // 实际填的东西。
            assert_eq!(
                err.api.details.as_ref().unwrap().get("sort"),
                Some(&serde_json::json!(bad))
            );
        }
    }

    #[test]
    fn position_is_only_valid_for_collection_items() {
        assert_eq!(
            parse_sort(
                Some("position:asc"),
                COLLECTION_ITEM_SORT_KEYS,
                DEFAULT_COLLECTION_ITEM_SORT
            )
            .unwrap(),
            SortSpec::new(VideoSort::Position, SortDirection::Asc)
        );
        // 条目列表没有 `position` 列 —— 同一个值在那里必须 422。
        assert_eq!(sort_err("position:asc").status, 422);
    }

    #[test]
    fn blank_query_is_rejected_but_absent_query_is_not() {
        assert_eq!(normalize_query(None).unwrap(), None);
        assert_eq!(
            normalize_query(Some("  素颜  ")).unwrap().as_deref(),
            Some("素颜")
        );
        for blank in ["", "   "] {
            let err = normalize_query(Some(blank)).unwrap_err();
            assert_eq!((err.status, err.code()), (422, INVALID_VIDEO_FILTER));
        }
    }

    #[test]
    fn page_validation_matches_upstream_messages_and_details() {
        for (page, size) in [(0, 20), (-1, 20)] {
            let err = validate_page(page, size).unwrap_err();
            assert_eq!(err.api.message, "page must be greater than 0");
            assert_eq!(
                err.api.details.as_ref().unwrap().get("page"),
                Some(&serde_json::json!(page))
            );
        }
        for size in [0, -1, 101] {
            let err = validate_page(1, size).unwrap_err();
            assert_eq!(err.api.message, "page_size must be between 1 and 100");
            assert_eq!(
                err.api.details.as_ref().unwrap().get("page_size"),
                Some(&serde_json::json!(size))
            );
        }
        // 边界值合法：page 从 1 开始，page_size 上限 100。
        assert!(validate_page(1, 1).is_ok());
        assert!(validate_page(1, 100).is_ok());
    }

    #[test]
    fn field_distinguishes_absent_from_null() {
        let absent = Field::<String>::Absent;
        let null = Field::<String>::Null;
        assert!(absent.is_absent() && !absent.is_given());
        assert!(!null.is_absent() && null.is_given());
        // 两者都取不到值 —— 上游那些 `is not None` 的判断一视同仁。
        assert_eq!(absent.as_value(), None);
        assert_eq!(null.as_value(), None);
        assert_eq!(
            Field::Value("x".to_owned()).as_value().map(String::as_str),
            Some("x")
        );
    }
}
