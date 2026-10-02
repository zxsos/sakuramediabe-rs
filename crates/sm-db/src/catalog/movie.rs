use chrono::NaiveDateTime;
use serde_json::Value as Json;
use sqlx::FromRow;

/// 受保护字段白名单。插件可写，宿主刷新与批量 UPDATE 均被运行时护栏拒绝。
pub const PROTECTED_MOVIE_FIELDS: [&str; 6] = [
    "title",
    "summary",
    "maker_name",
    "director_name",
    "is_collection",
    "is_blacklisted",
];

/// 字段归属标记。缺键代表宿主自动管理。
pub mod field_owner {
    /// 人工修改。
    pub const HOST_MANUAL: &str = "host:manual";
    /// 某插件持有。
    pub fn plugin(plugin_id: &str) -> String {
        format!("plugin:{plugin_id}")
    }
}

/// `movie_series` 表。对应 Peewee 的 `MovieSeries`。
#[derive(Debug, Clone, FromRow)]
pub struct MovieSeries {
    pub id: i64,
    /// Peewee 在 save 前统一 strip，避免同一系列产生重复实体。
    pub name: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `movie` 表。字段顺序与 Peewee 定义一致，便于逐列核对。
///
/// 分组：身份 / 元数据 / 图片 / 描述与归属 / 状态位 / 订阅搜索 / 字段主权。
/// 外键在 PostgreSQL 中是 `<field>_id` 整数列，故此处为 `*_id: Option<i64>`。
#[derive(Debug, Clone, FromRow)]
pub struct Movie {
    pub id: i64,

    /// JavDB ID。空串在 save 时归一为 NULL。
    pub javdb_id: Option<String>,
    /// 元数据来源记录（JSONB）。
    pub metadata_source: Option<Json>,
    pub javdb_next_check_at: Option<NaiveDateTime>,
    /// 番号。存 provider 规范原样，只去首尾空白、不做归一化改写：
    /// 分隔符与大小写都是有效信息。
    pub movie_number: String,

    pub title: String,
    pub release_date: Option<NaiveDateTime>,
    pub duration_minutes: i32,
    pub score: f64,
    pub score_number: i32,
    pub watched_count: i32,
    pub cover_image_id: Option<i64>,
    pub thin_cover_image_id: Option<i64>,

    pub summary: String,
    pub series_id: Option<i64>,
    pub maker_name: Option<String>,
    pub director_name: Option<String>,
    pub want_watch_count: i32,
    pub comment_count: i32,
    pub interaction_synced_at: Option<NaiveDateTime>,

    pub heat: i32,
    pub is_collection: bool,
    pub is_subscribed: bool,
    pub is_blacklisted: bool,
    /// 数据库 CHECK 约束保证不同时为真。
    pub subscribed_at: Option<NaiveDateTime>,

    pub subscription_search_state: String,
    pub subscription_search_attempt_count: i32,
    pub subscription_search_retry_round: i32,
    pub subscription_search_last_attempted_at: Option<NaiveDateTime>,
    pub subscription_search_last_succeeded_at: Option<NaiveDateTime>,
    pub subscription_search_next_retry_at: Option<NaiveDateTime>,
    pub subscription_search_error_code: Option<String>,
    pub subscription_search_last_error: Option<String>,
    pub subscription_search_last_error_at: Option<NaiveDateTime>,

    /// 受保护字段 owner 映射，JSONB，服务端默认 `{}`。
    pub field_owners: Json,
    /// 受保护字段版本号，服务端默认 0。
    /// 注意它不是整行的全局版本。
    pub mutation_revision: i64,

    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl Movie {
    /// 某字段是否受插件或人工主权保护。
    pub fn is_protected(field: &str) -> bool {
        PROTECTED_MOVIE_FIELDS.contains(&field)
    }

    /// 是否满足 CHECK 约束 `NOT (is_subscribed AND is_blacklisted)`。
    ///
    /// CHECK 是最后一道防线；service 层应先拦，否则会把 500 当成业务错误返回。
    pub fn satisfies_blacklist_constraint(&self) -> bool {
        !(self.is_subscribed && self.is_blacklisted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_fields_match_backend_whitelist() {
        let mut expected = vec![
            "title",
            "summary",
            "maker_name",
            "director_name",
            "is_collection",
            "is_blacklisted",
        ];
        expected.sort_unstable();
        let mut actual = PROTECTED_MOVIE_FIELDS.to_vec();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn plugin_owner_tag_matches_documented_format() {
        assert_eq!(
            field_owner::plugin("sakuramedia_local_provider"),
            "plugin:sakuramedia_local_provider"
        );
    }

    #[test]
    fn blacklist_constraint_is_mirrored() {
        let base = Movie {
            id: 1,
            javdb_id: None,
            metadata_source: None,
            javdb_next_check_at: None,
            movie_number: "ABC-001".to_owned(),
            title: "t".to_owned(),
            release_date: None,
            duration_minutes: 0,
            score: 0.0,
            score_number: 0,
            watched_count: 0,
            cover_image_id: None,
            thin_cover_image_id: None,
            summary: String::new(),
            series_id: None,
            maker_name: None,
            director_name: None,
            want_watch_count: 0,
            comment_count: 0,
            interaction_synced_at: None,
            heat: 0,
            is_collection: false,
            is_subscribed: false,
            is_blacklisted: false,
            subscribed_at: None,
            subscription_search_state: "pending".to_owned(),
            subscription_search_attempt_count: 0,
            subscription_search_retry_round: 0,
            subscription_search_last_attempted_at: None,
            subscription_search_last_succeeded_at: None,
            subscription_search_next_retry_at: None,
            subscription_search_error_code: None,
            subscription_search_last_error: None,
            subscription_search_last_error_at: None,
            field_owners: serde_json::json!({}),
            mutation_revision: 0,
            created_at: None,
            updated_at: None,
        };
        assert!(base.satisfies_blacklist_constraint());

        let mut subscribed = base.clone();
        subscribed.is_subscribed = true;
        assert!(subscribed.satisfies_blacklist_constraint());

        let mut conflict = subscribed.clone();
        conflict.is_blacklisted = true;
        assert!(
            !conflict.satisfies_blacklist_constraint(),
            "同时订阅与屏蔽会被数据库 CHECK 拒绝，必须提前拦截"
        );
    }
}
