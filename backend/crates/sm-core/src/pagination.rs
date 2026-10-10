//! 分页响应壳：`{items, page, page_size, total, synced_at}`。
//!
//! 对应客户端 `lib/core/network/paginated_response_dto.dart`。
//!
//! # 契约要点
//!
//! | 字段 | 客户端默认值 | 说明 |
//! |---|---|---|
//!| `items` | `[]` | 非数组或元素非对象时逐项丢弃 |
//! | `page` | `1` | |
//! | `page_size` | `20` | |
//! | `total` | `0` | |
//! | `synced_at` | `null` | 空串与非法格式都视为 `null` |
//!
//! `synced_at` 与条目内的 `created_at` 含义不同：前者是「这批数据的抓取时间，
//! 整批共用同一个值」，该周期/榜单暂无数据时为 `null`。
//!
//! 客户端 `fetch_all_pages.dart` 依赖 `total` 先取总数再并发拉完全部页，
//! 因此 `total` 必须准确，否则客户端会漏数据或多请求。

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json;

/// 分页响应。`items` 的元素类型由调用方的 DTO 决定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paginated<T> {
    pub items: Vec<T>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    /// 本批数据的抓取时间；该周期暂无数据时为 `None`。
    pub synced_at: Option<DateTime<FixedOffset>>,
}

impl<T> Paginated<T> {
    /// 构造一页数据。`synced_at` 为 `None` 时序列化仍会输出该键（值为 `null`），
    /// 客户端能处理，且比省略键更利于排障。
    pub fn new(items: Vec<T>, page: i64, page_size: i64, total: i64) -> Self {
        Self {
            items,
            page,
            page_size,
            total,
            synced_at: None,
        }
    }

    /// 附加抓取时间。
    pub fn with_synced_at(mut self, synced_at: DateTime<FixedOffset>) -> Self {
        self.synced_at = Some(synced_at);
        self
    }

    /// 总页数；`total` 为 0 时返回 0。
    pub fn total_pages(&self) -> i64 {
        if self.page_size <= 0 {
            return 0;
        }
        self.total.div_euclid(self.page_size)
    }

    /// 是否还有后续页。
    pub fn has_more(&self) -> bool {
        self.page < self.total_pages()
    }
}

impl<T> Paginated<T> {
    /// 从响应体解析，宽容度与客户端 `PaginatedResponseDto.fromJson` 一致。
    ///
    /// `item_from_json` 由调用方提供，用于把每个元素映射成具体 DTO；
    /// 无法解析的元素按客户端语义被丢弃。
    pub fn from_body<F>(body: &Value, item_from_json: F) -> Self
    where
        F: Fn(&Value) -> Option<T>,
    {
        let items = body
            .get("items")
            .and_then(Value::as_array)
            .map(|raw| {
                raw.iter()
                    .filter(|item| item.is_object())
                    .filter_map(item_from_json)
                    .collect::<Vec<T>>()
            })
            .unwrap_or_default();

        Self {
            items,
            page: json::as_int(body.get("page").unwrap_or(&Value::Null), 1),
            page_size: json::as_int(body.get("page_size").unwrap_or(&Value::Null), 20),
            total: json::as_int(body.get("total").unwrap_or(&Value::Null), 0),
            synced_at: body.get("synced_at").and_then(json::as_datetime),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_well_formed_body() {
        let body = json!({
            "items": [{"id": 1}, {"id": 2}],
            "page": 3,
            "page_size": 2,
            "total": 7,
            "synced_at": "2026-10-02T12:00:00+08:00"
        });
        let parsed: Paginated<Value> = Paginated::from_body(&body, |item| Some(item.clone()));
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.page, 3);
        assert_eq!(parsed.page_size, 2);
        assert_eq!(parsed.total, 7);
        assert!(parsed.synced_at.is_some());
        assert_eq!(parsed.total_pages(), 3);
        // page=3 恰是最后一页（total 7 / page_size 2 = 3 页），因此没有后续页。
    }

    #[test]
    fn falls_back_to_client_defaults() {
        let parsed: Paginated<Value> = Paginated::from_body(&json!({}), |item| Some(item.clone()));
        assert!(parsed.items.is_empty());
        assert_eq!(parsed.page, 1);
        assert_eq!(parsed.page_size, 20);
        assert_eq!(parsed.total, 0);
        assert!(parsed.synced_at.is_none());
        assert_eq!(parsed.total_pages(), 0);
        assert!(!parsed.has_more());
    }

    #[test]
    fn accepts_numeric_fields_as_strings() {
        // 后端历史上以字符串下发数字，客户端能吃；重写后必须同样能吃。
        let body = json!({"page": "2", "page_size": "50", "total": "120"});
        let parsed: Paginated<Value> = Paginated::from_body(&body, |item| Some(item.clone()));
        assert_eq!(parsed.page, 2);
        assert_eq!(parsed.page_size, 50);
        assert_eq!(parsed.total, 120);
    }

    #[test]
    fn drops_unparsable_items_and_synced_at_variants() {
        let body = json!({
            "items": [{"id": 1}, "not-an-object", 5],
            "synced_at": ""
        });
        let parsed: Paginated<Value> = Paginated::from_body(&body, |item| Some(item.clone()));
        assert_eq!(parsed.items.len(), 1, "非对象元素按客户端语义丢弃");
        assert!(parsed.synced_at.is_none(), "空串视为 null");

        let bad = json!({"synced_at": "not-a-date"});
        let parsed: Paginated<Value> = Paginated::from_body(&bad, |item| Some(item.clone()));
        assert!(parsed.synced_at.is_none(), "非法时间静默忽略，不报错");
    }
}

// ══════════════════════════════════════════════════════════════════
// 分页参数校验
// ══════════════════════════════════════════════════════════════════
//
// 对应后端 `src/common/service_helpers.py` 的 `validate_page` 与 `paginate`。
//
// # 后端校验风格不统一（重写时必须留意）
//
// 同一个后端里并存三种写法，行为并不完全一致：
//
// | 风格 | 出现位置 | 行为 |
// |---|---|---|
// | `Query(default=1, ge=1)` | `discovery/hot_actress_releases.py` 等 | FastAPI 参数级校验 |
// | `validate_page()` | 多数 service | 手工校验，错误码由调用方传入 |
// | `page: int = 1`（无约束） | `playback/media.py:119` | **不校验** |
//
// 统一到 `validate_page` 语义是最安全的选择：它是覆盖面最广的一种，
// 且错误码由端点自己决定，客户端可按 code 分支。
//
// # 不变量
//
// - `page` 从 **1** 开始，`offset = (page - 1) * page_size`
// - `page_size` 上限 **硬编码 100**（不是 `config.max_page_size`，那个字段目前未被使用）
// - 响应回显**请求的** `page` / `page_size`，不是服务端截断后的值
// - `total` 是**过滤后**的总数，且在取 offset 之前统计

/// `page_size` 上限。硬编码，与后端 `validate_page` 一致。
pub const MAX_PAGE_SIZE: i64 = 100;

/// 分页参数校验失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// `page <= 0`
    InvalidPage { page: i64 },
    /// `page_size <= 0` 或 `> 100`
    InvalidPageSize { page_size: i64 },
}

impl PageError {
    /// 对应后端抛出的中文/英文提示原文。
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidPage { .. } => "page must be greater than 0",
            Self::InvalidPageSize { .. } => "page_size must be between 1 and 100",
        }
    }

    /// `details` 内容：后端把出错的字段值原样放进 details。
    pub fn details(self) -> serde_json::Value {
        match self {
            Self::InvalidPage { page } => serde_json::json!({ "page": page }),
            Self::InvalidPageSize { page_size } => serde_json::json!({ "page_size": page_size }),
        }
    }
}

/// 校验分页参数。`error_code` 由端点自行决定，对应后端的 `error_code` 形参。
pub fn validate_page(page: i64, page_size: i64) -> Result<(), PageError> {
    if page <= 0 {
        return Err(PageError::InvalidPage { page });
    }
    if page_size <= 0 || page_size > MAX_PAGE_SIZE {
        return Err(PageError::InvalidPageSize { page_size });
    }
    Ok(())
}

/// 计算 SQL OFFSET，对应后端 `paginate` 的 `(page - 1) * page_size`。
///
/// 调用前应先 `validate_page`，否则 `page <= 0` 会得到负 offset。
pub fn page_offset(page: i64, page_size: i64) -> i64 {
    (page - 1) * page_size
}

/// 计算客户端 `fetchAllPagesConcurrently` 会请求的最后一页（1-based）。
///
/// 对应 Dart 的 `(total / pageSize).ceil()`。
pub fn last_page(total: i64, page_size: i64) -> i64 {
    if page_size <= 0 {
        return 0;
    }
    total.div_euclid(page_size) + i64::from(total.rem_euclid(page_size) != 0)
}
#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn accepts_valid_range() {
        assert!(validate_page(1, 1).is_ok());
        assert!(validate_page(1, MAX_PAGE_SIZE).is_ok());
        assert!(validate_page(9999, 20).is_ok());
    }

    #[test]
    fn rejects_non_positive_page() {
        for page in [0, -1, -100] {
            assert_eq!(
                validate_page(page, 20),
                Err(PageError::InvalidPage { page }),
                "page={page}"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_page_size() {
        for size in [0, -1, MAX_PAGE_SIZE + 1, 1000] {
            assert_eq!(
                validate_page(1, size),
                Err(PageError::InvalidPageSize { page_size: size }),
                "size={size}"
            );
        }
    }

    #[test]
    fn page_is_checked_before_page_size() {
        // 后端顺序是先 page 再 page_size，两个都非法时只报 page。
        assert_eq!(validate_page(0, 0), Err(PageError::InvalidPage { page: 0 }));
    }

    #[test]
    fn error_details_carry_offending_value() {
        let error = validate_page(-1, 20).unwrap_err();
        assert_eq!(error.message(), "page must be greater than 0");
        assert_eq!(error.details(), serde_json::json!({"page": -1}));

        let error = validate_page(1, 101).unwrap_err();
        assert_eq!(error.message(), "page_size must be between 1 and 100");
        assert_eq!(error.details(), serde_json::json!({"page_size": 101}));
    }

    #[test]
    fn offset_is_one_based() {
        assert_eq!(page_offset(1, 20), 0);
        assert_eq!(page_offset(2, 20), 20);
        assert_eq!(page_offset(4, 25), 75);
    }

    #[test]
    fn last_page_matches_dart_ceil() {
        // Dart: (total / pageSize).ceil()
        assert_eq!(last_page(7, 2), 4);
        assert_eq!(last_page(8, 2), 4);
        assert_eq!(last_page(0, 20), 0);
        assert_eq!(last_page(1, 20), 1);
        assert_eq!(last_page(100, 20), 5);
        assert_eq!(last_page(101, 20), 6);
        assert_eq!(last_page(50, 0), 0, "page_size 非法时不返回页数");
    }
}
