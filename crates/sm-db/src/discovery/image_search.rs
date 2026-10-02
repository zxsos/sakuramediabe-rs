//! 图搜索模型（2 张表）。
//!
//! 对应 `src/model/discovery/image_search.py`。
//!
//! # `image_search_index_state` 是单例表，且没有时间戳
//!
//! ```python
//! class ImageSearchIndexState(BaseModel):     # 不是 TimestampedMixin
//!     id = peewee.IntegerField(primary_key=True, default=1)
//!     indexed_space_id = peewee.CharField(max_length=255)
//! ```
//!
//! 两个非常规之处：
//!
//! 1. **单行表** —— `id` 默认 1，全表只应存在一行，承载「两套图搜索向量
//!    集合共享的嵌入空间身份」。写入方必须用 upsert 而非 insert。
//! 2. **无 `created_at` / `updated_at`** —— 全库第二张不继承
//!    `TimestampedMixin` 的表（第一张是 `schema_migration`）。
//!    嵌入空间变更的时刻由外部记录，本表只持有当前值。
//!
//! 这个 `indexed_space_id` 是**兼容性关键**：SigLIP2 模型一换，嵌入维度就变，
//! 旧向量无法与新查询向量比较。客户端持有的会话里存着查询向量，
//! 若空间已切换而会话未失效，检索结果会**静默出错** —— 不报错，只是变差。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// 图搜索会话状态。
pub mod image_search_status {
    /// 默认值：已就绪，可直接检索。
    pub const READY: &str = "ready";
}

/// `image_search_session` 表：一次图搜会话。
///
/// 会话持有查询向量与结果游标，靠 `expires_at` 定期清理。
#[derive(Debug, Clone, FromRow)]
pub struct ImageSearchSession {
    pub id: i32,
    /// 对外暴露的会话 id，唯一且带索引。
    pub session_id: String,
    /// 状态，默认 `ready`。
    pub status: String,
    /// 每页条数，默认 20。
    pub page_size: i32,
    /// 结果游标（不透明文本）。首轮为空。
    pub next_cursor: Option<String>,
    /// SigLIP2 查询向量，以 JSON 文本存储浮点数组。
    pub query_vector: Option<String>,
    /// 命中的影片 id 列表，JSON 数组。
    pub movie_ids: Option<String>,
    /// 排除的影片 id 列表，JSON 数组。
    pub exclude_movie_ids: Option<String>,
    /// 相似度阈值。为空表示不设下限。
    pub score_threshold: Option<f32>,
    /// 过期时刻，有索引 —— 清理任务按它扫。
    pub expires_at: NaiveDateTime,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl ImageSearchSession {
    /// 是否已过期。
    pub fn is_expired(&self, now: NaiveDateTime) -> bool {
        self.expires_at <= now
    }

    /// 是否还有下一页。
    pub fn has_next_page(&self) -> bool {
        self.next_cursor
            .as_deref()
            .is_some_and(|c| !c.trim().is_empty())
    }

    /// 解析查询向量维度。
    ///
    /// 用于与嵌入空间声明的维度交叉校验 —— 维度对不上基本等价于
    /// 「空间已切换但会话仍在有效期内」。
    pub fn query_vector_dim(&self) -> Option<usize> {
        let raw = self.query_vector.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let values: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;
        if values.is_empty() {
            None
        } else {
            Some(values.len())
        }
    }

    /// 解析命中的影片 id。
    pub fn parsed_movie_ids(&self) -> Option<Vec<i32>> {
        parse_ids(self.movie_ids.as_deref())
    }

    /// 解析排除的影片 id。
    pub fn parsed_exclude_movie_ids(&self) -> Option<Vec<i32>> {
        parse_ids(self.exclude_movie_ids.as_deref())
    }
}

fn parse_ids(raw: Option<&str>) -> Option<Vec<i32>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str(raw).ok()
}

/// `image_search_index_state` 表：嵌入空间身份，**单例**。
///
/// 固定 `id = 1`，全表一行。无 `created_at` / `updated_at`。
#[derive(Debug, Clone, FromRow)]
pub struct ImageSearchIndexState {
    /// 恒为 1。
    pub id: i32,
    /// 已索引向量集合的嵌入空间标识（Qdrant collection / space id）。
    pub indexed_space_id: String,
}

/// 单例行的约定 id。
pub const IMAGE_SEARCH_STATE_ID: i32 = 1;

impl ImageSearchIndexState {
    /// 该状态行是否为约定的单例行。
    pub fn is_singleton_row(&self) -> bool {
        self.id == IMAGE_SEARCH_STATE_ID
    }

    /// 校验会话的查询向量是否与当前嵌入空间兼容。
    ///
    /// 维度不匹配时返回 false，调用方应让会话失效而不是返回错误结果。
    /// 缺维度信息时不阻断，交给上层判断。
    pub fn accepts_session(&self, session_dim: Option<usize>, expected_dim: Option<usize>) -> bool {
        match (session_dim, expected_dim) {
            (Some(actual), Some(expected)) => actual == expected,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 2).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    fn session(vector: Option<&str>, expires: NaiveDateTime) -> ImageSearchSession {
        ImageSearchSession {
            id: 1,
            session_id: "s1".to_owned(),
            status: image_search_status::READY.to_owned(),
            page_size: 20,
            next_cursor: None,
            query_vector: vector.map(str::to_owned),
            movie_ids: Some("[1,2,3]".to_owned()),
            exclude_movie_ids: Some("[]".to_owned()),
            score_threshold: None,
            expires_at: expires,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        assert!(session(None, at(10)).is_expired(at(10)));
        assert!(session(None, at(10)).is_expired(at(11)));
        assert!(!session(None, at(10)).is_expired(at(9)));
    }

    #[test]
    fn cursor_drives_pagination() {
        let mut s = session(None, at(10));
        assert!(!s.has_next_page(), "首轮无游标");
        s.next_cursor = Some("  ".to_owned());
        assert!(!s.has_next_page(), "空白游标等同没有");
        s.next_cursor = Some("cursor-abc".to_owned());
        assert!(s.has_next_page());
    }

    #[test]
    fn query_vector_dim_detects_space_switch() {
        let s = session(Some("[0.1,0.2,0.3]"), at(10));
        assert_eq!(s.query_vector_dim(), Some(3));
        assert!(!session(Some("[]"), at(10)).query_vector_dim().is_some());
        assert!(session(None, at(10)).query_vector_dim().is_none());
    }

    #[test]
    fn parses_movie_id_lists() {
        let s = session(None, at(10));
        assert_eq!(s.parsed_movie_ids().unwrap(), vec![1, 2, 3]);
        assert_eq!(s.parsed_exclude_movie_ids().unwrap(), Vec::<i32>::new());
    }

    #[test]
    fn index_state_is_a_singleton_row() {
        let state = ImageSearchIndexState {
            id: IMAGE_SEARCH_STATE_ID,
            indexed_space_id: "siglip2-768".to_owned(),
        };
        assert!(state.is_singleton_row());
        let stray = ImageSearchIndexState { id: 2, ..state.clone() };
        assert!(!stray.is_singleton_row(), "id 恒为 1，出现第二行说明写入方用了 insert");
    }

    #[test]
    fn rejects_sessions_from_a_different_embedding_space() {
        let state = ImageSearchIndexState {
            id: IMAGE_SEARCH_STATE_ID,
            indexed_space_id: "siglip2-768".to_owned(),
        };
        // 维度一致 -> 接受
        assert!(state.accepts_session(Some(768), Some(768)));
        // 维度不同 -> 会话来自旧嵌入空间，应失效而非返回错误结果
        assert!(!state.accepts_session(Some(512), Some(768)));
        // 信息缺失时不阻断，交给上层判断
        assert!(state.accepts_session(None, Some(768)));
        assert!(state.accepts_session(Some(768), None));
    }
}
