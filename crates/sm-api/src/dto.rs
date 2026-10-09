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
//! # 一处已知偏差（不要当成已对齐）
//!
//! **`created_at` / `updated_at` 的时间戳格式。** 上游 `SchemaModel` 有
//!    全局 `@field_serializer("*")` 按**运行时本地时区**序列化；这里按
//!    naive UTC 输出 `YYYY-MM-DDTHH:MM:SS`。上游容器 `TZ=UTC`、
//!    PG `timezone=UTC`，实际值应当一致，但**没有对拍过**。
//!    另外 DB 两列可空而上游 DTO 非可空，这里 None 时输出空串。
//!
//! `movie_count` 曾恒为 0，现已由
//! [`PlaylistResource::with_movie_count`] 从 service 的聚合计数填充。

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

impl PlaylistResource {
    /// 带成员计数构造。
    ///
    /// `From<Playlist>` 委托到这里并传 0 —— 那条路径只用于「刚写完、
    /// 成员数必为 0」的场景（`POST /playlists`）。凡是**读**已有列表的
    /// 地方都必须走这里并传真实计数，否则客户端读到的 `movie_count: 0`
    /// 与「列表是空的」不可区分。
    pub fn with_movie_count(value: Playlist, movie_count: i32) -> Self {
        let is_system = value.is_system();
        Self {
            id: value.id,
            name: value.name,
            kind: value.kind,
            description: value.description,
            is_system,
            is_mutable: !is_system,
            is_deletable: !is_system,
            movie_count,
            created_at: format_timestamp(value.created_at),
            updated_at: format_timestamp(value.updated_at),
        }
    }
}

impl From<Playlist> for PlaylistResource {
    /// **计数恒为 0。** 见 [`PlaylistResource::with_movie_count`] 的说明 ——
    /// 新建列表的成员数确实是 0，但这个 `From` 也会被误用到读路径上。
    fn from(value: Playlist) -> Self {
        Self::with_movie_count(value, 0)
    }
}

impl From<&sm_service::collections::playlist::PlaylistWithCount> for PlaylistResource {
    fn from(value: &sm_service::collections::playlist::PlaylistWithCount) -> Self {
        Self::with_movie_count(value.playlist.clone(), value.movie_count)
    }
}

/// `GET /playlists/{id}/resolutions` 的响应项（上游 `PlaylistResolutionOption`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistResolutionOption {
    /// 档位标签（`8K` / `4K` / …）。已归一为大写形态。
    pub resolution: String,
    /// 列表内最高分辨率落在该档位的**影片**数。
    pub count: i32,
}

/// `GET /status/capabilities` 的响应体。
///
/// 上游那个端点**没有 `response_model`**，直接返回 `capabilities()` 的
/// `dict[str, bool]`。所以这里的字段集合就是全部 —— 多一个键客户端不会
/// 报错，但少一个会。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesResource {
    /// 相似影片能力（依赖 `qdrant.enabled`）。
    pub movie_similarity: bool,
    /// 图搜能力（依赖 `qdrant.enabled` **且** `image_search.enabled`）。
    pub image_search: bool,
}

impl From<sm_service::system::optional_services::Capabilities> for CapabilitiesResource {
    fn from(value: sm_service::system::optional_services::Capabilities) -> Self {
        Self {
            movie_similarity: value.movie_similarity,
            image_search: value.image_search,
        }
    }
}

impl From<&sm_service::collections::playlist::ResolutionOption> for PlaylistResolutionOption {
    fn from(value: &sm_service::collections::playlist::ResolutionOption) -> Self {
        Self {
            resolution: value.resolution.clone(),
            count: value.count,
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

/// `GET /config` 的响应体（上游 `ConfigResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigResource {
    /// 全部**可改**配置节的明文快照。只读键（`auth` / `enable_docs` /
    /// `plugins`）已被剔除 —— 见 `routes/config.rs` 的模块文档。
    pub values: serde_json::Value,
}

/// `PATCH /config` 的响应体（上游 `ConfigUpdateResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigUpdateResource {
    /// 写盘**之后**的公开快照。注意它与运行中进程实际在用的值可能不同 ——
    /// 那要等重启，见 `restart_required`。
    pub values: serde_json::Value,
    /// 必须重启的进程。**恒为** `["api", "aps"]`，永不为空。
    ///
    /// 上游的类型是 `list[Literal["api", "aps"]]`，字面量集合只有这两个，
    /// 所以这个字段表达的不是「哪些进程受影响」，而是「本项目由这两个进程
    /// 读配置」这一事实。客户端据此提示用户重启，而不需要判断非空。
    pub restart_required: Vec<String>,
}

impl ConfigUpdateResource {
    /// 用 service 给出的公开快照构造，`restart_required` 取常量。
    pub fn new(values: serde_json::Value) -> Self {
        Self {
            values,
            restart_required: sm_service::system::config::RESTART_REQUIRED
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
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
