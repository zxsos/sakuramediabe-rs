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
        Self { items, page, page_size, total, synced_at: None }
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
                    .filter_map(|item| item_from_json(item))
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
