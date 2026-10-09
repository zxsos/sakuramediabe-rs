//! 响应 / 请求 DTO。
//!
//! # 字段集合照抄上游 `src/schema/collections/playlists.py:11-64`
//!
//! ```text
//! id, name, kind, description, is_system, is_mutable, is_deletable,
//! movie_count, created_at, updated_at
//! ```
//!
//! `is_system` / `is_mutable` / `is_deletable` 是**派生字段**：上游靠
//! `from_playlist` 里的 `extra` 注入，Rust 侧在 `From<Playlist>` 里算。
//!
//! # 两处已知偏差（不要当成已对齐）
//!
//! 1. **`movie_count` 恒为 0。** 上游的计数来自 `playlist_movie` 聚合查询，
//!    本批 service 没有暴露该方法。字段必须存在（客户端读它），值待补。
//! 2. **`created_at` / `updated_at` 的时间戳格式。** 上游 `SchemaModel` 有
//!    全局 `@field_serializer("*")` 按**运行时本地时区**序列化；这里按
//!    naive UTC 输出 `YYYY-MM-DDTHH:MM:SS`。上游容器 `TZ=UTC`、
//!    PG `timezone=UTC`，实际值应当一致，但**没有对拍过**。
//!    另外 DB 两列可空而上游 DTO 非可空，这里 None 时输出空串。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sm_db::collections::Playlist;

/// 播放列表响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistResource {
    pub id: i32,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub is_system: bool,
    pub is_mutable: bool,
    pub is_deletable: bool,
    pub movie_count: i32,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Playlist> for PlaylistResource {
    fn from(value: Playlist) -> Self {
        let is_system = value.is_system();
        Self {
            id: value.id,
            name: value.name,
            kind: value.kind,
            description: value.description,
            is_system,
            is_mutable: !is_system,
            is_deletable: !is_system,
            // 见模块文档第 1 条：计数待补。
            movie_count: 0,
            created_at: format_timestamp(value.created_at),
            updated_at: format_timestamp(value.updated_at),
        }
    }
}

/// naive UTC → 上游 Pydantic 的 datetime 字面量格式。
///
/// 见模块文档第 2 条：可选值缺失时输出空串，而不是让整个响应序列化失败。
fn format_timestamp(value: Option<NaiveDateTime>) -> String {
    value.map_or_else(String::new, |dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
}

// ---------------------------------------------------------------- 鉴权

/// `POST /auth/tokens` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenCreateRequest {
    pub username: String,
    pub password: String,
}

/// `POST /auth/token-refreshes` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenRefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUserSummary {
    pub username: String,
}

/// 令牌响应体，字段与 `src/schema/system/auth.py:19-26` 一致。
///
/// `expires_in` 是**配置窗口**（`access_token_expire_minutes * 60`）而不是
/// 实际剩余秒数 —— 上游就是这么算的，别"修正"它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResource {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expires_at: String,
    pub refresh_expires_at: String,
    pub user: AuthUserSummary,
}

/// UTC 时间戳 → 与播放列表一致的 Pydantic 字面量格式。
pub(crate) fn format_utc(value: chrono::DateTime<chrono::Utc>) -> String {
    value.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `POST /playlists` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistCreateRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// `PATCH /playlists/{id}` 请求体。两个字段都可缺省。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaylistUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_db::collections::{Playlist, PLAYLIST_KIND_CUSTOM, PLAYLIST_KIND_RECENTLY_PLAYED};

    fn playlist(kind: &str) -> Playlist {
        Playlist {
            id: 3,
            name: "我的列表".to_owned(),
            description: "d".to_owned(),
            owner_plugin_id: None,
            plugin_key: None,
            kind: kind.to_owned(),
            created_at: NaiveDateTime::parse_from_str("2026-10-04 01:02:03", "%Y-%m-%d %H:%M:%S")
                .ok(),
            updated_at: None,
        }
    }

    #[test]
    fn derived_flags_follow_upstream() {
        let custom = PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM));
        assert!(!custom.is_system);
        assert!(custom.is_mutable);
        assert!(custom.is_deletable);

        let system = PlaylistResource::from(playlist(PLAYLIST_KIND_RECENTLY_PLAYED));
        assert!(system.is_system);
        assert!(!system.is_mutable);
        assert!(!system.is_deletable);
    }

    #[test]
    fn timestamp_uses_the_pydantic_shape() {
        let resource = PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM));
        assert_eq!(resource.created_at, "2026-10-04T01:02:03");
        // 缺失时输出空串，而不是让序列化失败
        assert_eq!(resource.updated_at, "");
    }

    #[test]
    fn field_set_matches_the_upstream_dto() {
        // 字段数量变化会直接改变响应体字节数 —— 用序列化结果钉住。
        let json = serde_json::to_value(PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM)))
            .expect("DTO 必须可序列化");
        let object = json.as_object().expect("DTO 是 JSON 对象");
        for key in [
            "id",
            "name",
            "kind",
            "description",
            "is_system",
            "is_mutable",
            "is_deletable",
            "movie_count",
            "created_at",
            "updated_at",
        ] {
            assert!(object.contains_key(key), "缺少字段 {key}");
        }
        assert_eq!(
            object.len(),
            10,
            "字段数必须是 10，多一个少一个都是契约变更"
        );
    }
}
