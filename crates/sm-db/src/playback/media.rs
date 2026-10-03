//! `playback` 域模型（6 张表）。
//!
//! 对应 `src/model/playback/media.py` 与 `libraries.py`。
//!
//! # 关键不变量：Media 恰好归属其一
//!
//! ```text
//! movie_number 有值 XOR video_item_id 有值
//! ```
//!
//! 解耦后一条 Media 归属 movie（JAV）或 video_item（非 JAV）之一，
//! 由 `Media.save` 强制。**两者都空或都非空都会被拒绝。**
//!
//! 注意 `movie` 外键指向 `Movie.movie_number` 而非 `Movie.id`，
//! 因此 Rust 侧该列是 `Option<String>` 而不是 `Option<i64>`。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `media_library` 表。
///
/// `provider_config` 是 `JsonTextField` —— JSON 以**文本**存储，空串视为 `None`。
#[derive(Debug, Clone, FromRow)]
pub struct MediaLibrary {
    pub id: i32,
    /// 库名全局唯一（`unique + index`）。
    pub name: String,
    /// 决定用哪个 provider 实现来解释 `provider_config`。
    pub provider_key: String,
    /// 不透明 JSON 文本。宿主只保存与回传。
    pub provider_config: Option<String>,
    /// 多账号存储（115 等）用它区分 cookie 归属。
    pub account_key: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl MediaLibrary {
    /// 解析 `provider_config` 文本。
    ///
    /// 空串与非法 JSON 都返回 `None`，与 `JsonTextField.python_value` 一致。
    pub fn parsed_config(&self) -> Option<serde_json::Value> {
        let raw = self.provider_config.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }
}

/// 缩略图生成状态机。
///
/// 挂在 `Media` 自身而不是 `MediaThumbnail`：后者是成功产物，
/// 承担不了失败、退避与人工重试状态。
pub mod thumbnail_state {
    /// 初始态。
    pub const PENDING: &str = "pending";
    /// 失败后退避等待重试。
    pub const RETRY_WAIT: &str = "retry_wait";
    /// 终态（成功或明确放弃）。
    pub const TERMINAL: &str = "terminal";
    /// 已成功产出缩略图。
    pub const SUCCEEDED: &str = "succeeded";

    /// 全部合法取值。数据库列有默认值 `"pending"`。
    pub const ALL: [&str; 4] = [PENDING, RETRY_WAIT, TERMINAL, SUCCEEDED];

    /// 是否为合法状态字面量。
    pub fn is_valid(state: &str) -> bool {
        ALL.contains(&state)
    }

    /// 该状态下是否应被重试调度扫到。
    ///
    /// 索引是 `(thumbnail_generation_state, thumbnail_next_retry_at)`，
    /// 所以只有 `retry_wait` 会被带 `next_retry_at` 的扫描命中。
    pub fn is_retryable(state: &str) -> bool {
        state == RETRY_WAIT
    }
}

/// `media` 表。
#[derive(Debug, Clone, FromRow)]
pub struct Media {
    pub id: i32,

    /// 指向 `Movie.movie_number`（**字符串**，非 id）。
    pub movie_number: Option<String>,
    /// 指向 `VideoItem.id`。
    pub video_item_id: Option<i32>,
    /// DDL 是 integer（与 media_library.id 同宽），所以是 i32 而非 i64。
    /// 写成 i64 时对拍查不出来（它只看列名/可空性/规范化类型名的映射），
    /// 只有集成测试真解码才会报 ColumnDecode。
    pub library_id: i32,

    /// 不透明存储引用，结构由 provider 定义。`JsonTextField`。
    pub storage_ref: Option<String>,
    pub file_name: String,
    pub resolution: Option<String>,
    pub file_size_bytes: i64,
    /// `media-file-hash-v1:<40 hex>`，跨存储识别重复文件的依据。
    pub file_hash: Option<String>,
    /// 导入来源身份。相同值代表同一位置且未变化的来源，
    /// 用于跳过未变化的来源。
    pub import_source_identity: Option<String>,
    pub duration_seconds: i32,
    /// 整理后的探测结果（codec / profile / bitrate 等），`JsonTextField`。
    pub video_info: Option<String>,
    pub valid: bool,

    pub thumbnail_generation_state: String,
    pub thumbnail_attempt_count: i32,
    /// 被主动推迟（如库离线）的次数，不计入失败。
    pub thumbnail_deferred_count: i32,
    pub thumbnail_next_retry_at: Option<NaiveDateTime>,
    pub thumbnail_last_error_code: Option<String>,
    pub thumbnail_last_error: Option<String>,
    /// 进入终态的时刻，用于统计与人工排查。
    pub thumbnail_terminal_at: Option<NaiveDateTime>,

    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl Media {
    /// 归属不变量：`movie_number` 与 `video_item_id` 恰好其一非空。
    ///
    /// 对应 `Media.save` 的 `if (movie_number is None) == (video_item_id is None): raise`。
    pub fn satisfies_owner_constraint(&self) -> bool {
        self.movie_number.is_some() ^ self.video_item_id.is_some()
    }

    /// 缩略图是否处于可重试态。
    pub fn thumbnail_retryable(&self) -> bool {
        thumbnail_state::is_retryable(&self.thumbnail_generation_state)
    }

    /// 缩略图是否已进入终态。
    pub fn thumbnail_is_terminal(&self) -> bool {
        matches!(
            self.thumbnail_generation_state.as_str(),
            thumbnail_state::TERMINAL | thumbnail_state::SUCCEEDED
        )
    }
}

/// 图搜索引状态。
///
/// 与 `MoviePlotImage` 的前三个取值相同，但**多一个 SKIPPED**：
/// 非 JAV 媒体的缩略图不参与向量索引，落明确终态避免长期滞留 PENDING。
pub mod image_search_index_status {
    pub const PENDING: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const SUCCESS: i32 = 2;
    /// 非 JAV 媒体缩略图，不参与图像检索。
    pub const SKIPPED: i32 = 3;

    pub const ALL: [i32; 4] = [PENDING, FAILED, SUCCESS, SKIPPED];

    pub fn is_valid(status: i32) -> bool {
        ALL.contains(&status)
    }

    /// 是否为终态（无需再处理）。
    pub fn is_terminal(status: i32) -> bool {
        status == SUCCESS || status == SKIPPED
    }
}

/// `media_thumbnail` 表。
///
/// 唯一索引 `(media, offset)`，保证同一时刻点不重复产出。
#[derive(Debug, Clone, FromRow)]
pub struct MediaThumbnail {
    pub id: i32,
    pub media_id: i64,
    pub image_id: i64,
    /// 距片头的秒数。
    pub offset: i32,
    pub image_search_index_status: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `media_progress` 表。
///
/// `media` 上有唯一索引，一条 Media 至多一条进度。
#[derive(Debug, Clone, FromRow)]
pub struct MediaProgress {
    pub id: i32,
    pub media_id: i64,
    pub position_seconds: i32,
    pub last_watched_at: Option<NaiveDateTime>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `media_point` 表（时刻点 / 剧情点）。
///
/// 删除行为与其它表不同，重写时若动 schema 会破坏语义：
///
/// | 关系 | on_delete | 原因 |
/// |---|---|---|
/// | `media` | SET NULL | 影片删除后仍保留时刻点展示 |
/// | `thumbnail` | SET NULL | 同上 |
/// | `image` | **RESTRICT** | 有引用时禁止删图 |
///
/// `movie_number` / `video_item_id` 是**快照**，不建外键。
#[derive(Debug, Clone, FromRow)]
pub struct MediaPoint {
    pub id: i32,
    pub media_id: Option<i64>,
    pub thumbnail_id: Option<i64>,
    /// 删图会被数据库拒绝（RESTRICT）。
    pub image_id: i64,
    /// 来源快照，无外键。
    pub movie_number: Option<String>,
    /// 来源快照，无外键。
    pub video_item_id: Option<i32>,
    pub offset_seconds: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `media_clip` 表（片段）。
///
/// 片段是**独立资产**：来源 Media 被删除时只置空引用，
/// 记录与其物理文件都保留。唯一索引
/// `(media, start_offset_seconds, end_offset_seconds)` 只在来源存活期间有效，
/// 这正是期望行为。
#[derive(Debug, Clone, FromRow)]
pub struct MediaClip {
    pub id: i32,
    pub media_id: Option<i64>,
    /// 来源快照，便于来源删除后仍可归属与展示。
    pub movie_number: Option<String>,
    pub start_offset_seconds: i32,
    pub end_offset_seconds: i32,
    pub title: String,
    /// 产物 mp4 相对 `media_clip_root_path` 的路径。
    pub file_path: String,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl MediaClip {
    /// 区间长度（秒）。`end <= start` 时返回 0。
    pub fn length_seconds(&self) -> i32 {
        (self.end_offset_seconds - self.start_offset_seconds).max(0)
    }

    /// 该片段是否仍挂在来源 Media 上。
    ///
    /// 为 `false` 时是孤立片段：来源已删，但记录与文件都还在。
    pub fn has_live_source(&self) -> bool {
        self.media_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_media(movie: Option<&str>, video: Option<i32>, state: &str) -> Media {
        Media {
            id: 1,
            movie_number: movie.map(str::to_owned),
            video_item_id: video,
            library_id: 1,
            storage_ref: None,
            file_name: "a.mp4".to_owned(),
            resolution: None,
            file_size_bytes: 0,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
            valid: true,
            thumbnail_generation_state: state.to_owned(),
            thumbnail_attempt_count: 0,
            thumbnail_deferred_count: 0,
            thumbnail_next_retry_at: None,
            thumbnail_last_error_code: None,
            thumbnail_last_error: None,
            thumbnail_terminal_at: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn owner_constraint_requires_exactly_one_parent() {
        assert!(demo_media(Some("ABC-001"), None, "pending").satisfies_owner_constraint());
        assert!(demo_media(None, Some(7), "pending").satisfies_owner_constraint());

        assert!(
            !demo_media(Some("ABC-001"), Some(7), "pending").satisfies_owner_constraint(),
            "两者都非空必须被拒绝"
        );
        assert!(
            !demo_media(None, None, "pending").satisfies_owner_constraint(),
            "两者都空必须被拒绝"
        );
    }

    #[test]
    fn thumbnail_state_literals_match_backend() {
        assert!(thumbnail_state::is_valid("pending"));
        assert!(thumbnail_state::is_valid("retry_wait"));
        assert!(thumbnail_state::is_valid("terminal"));
        assert!(thumbnail_state::is_valid("succeeded"));
        assert!(!thumbnail_state::is_valid("bogus"));
    }

    #[test]
    fn only_retry_wait_is_rescheduled() {
        // 索引是 (state, next_retry_at)，只有 retry_wait 会被退避扫描命中。
        assert!(thumbnail_state::is_retryable("retry_wait"));
        for state in ["pending", "terminal", "succeeded"] {
            assert!(!thumbnail_state::is_retryable(state), "state={state}");
        }
    }

    #[test]
    fn terminal_covers_terminal_and_succeeded() {
        assert!(demo_media(None, Some(1), "terminal").thumbnail_is_terminal());
        assert!(demo_media(None, Some(1), "succeeded").thumbnail_is_terminal());
        assert!(!demo_media(None, Some(1), "pending").thumbnail_is_terminal());
        assert!(!demo_media(None, Some(1), "retry_wait").thumbnail_is_terminal());
    }

    #[test]
    fn thumbnail_retryable_matches_state_helper() {
        assert!(demo_media(None, Some(1), "retry_wait").thumbnail_retryable());
        assert!(!demo_media(None, Some(1), "pending").thumbnail_retryable());
    }

    #[test]
    fn image_index_status_has_extra_skipped_for_non_jav() {
        // MediaThumbnail 比 MoviePlotImage 多一个 SKIPPED = 3。
        assert_eq!(image_search_index_status::PENDING, 0);
        assert_eq!(image_search_index_status::FAILED, 1);
        assert_eq!(image_search_index_status::SUCCESS, 2);
        assert_eq!(image_search_index_status::SKIPPED, 3);
        assert_eq!(image_search_index_status::ALL.len(), 4);

        assert!(image_search_index_status::is_terminal(
            image_search_index_status::SUCCESS
        ));
        assert!(
            image_search_index_status::is_terminal(image_search_index_status::SKIPPED),
            "跳过也是终态，否则会长期滞留"
        );
        assert!(!image_search_index_status::is_terminal(
            image_search_index_status::PENDING
        ));
    }

    #[test]
    fn clip_length_and_orphan_detection() {
        let mut clip = MediaClip {
            id: 1,
            media_id: Some(5),
            movie_number: Some("ABC-001".to_owned()),
            start_offset_seconds: 10,
            end_offset_seconds: 40,
            title: String::new(),
            file_path: "clip.mp4".to_owned(),
            file_size_bytes: 0,
            duration_seconds: 30,
            created_at: None,
            updated_at: None,
        };
        assert_eq!(clip.length_seconds(), 30);
        assert!(clip.has_live_source());

        clip.end_offset_seconds = 5;
        assert_eq!(clip.length_seconds(), 0, "倒挂区间不返回负数");

        clip.media_id = None;
        assert!(!clip.has_live_source(), "来源删除后成为孤立片段");
    }

    #[test]
    fn library_config_treats_blank_as_absent() {
        let make = |raw: Option<&str>| MediaLibrary {
            id: 1,
            name: "n".to_owned(),
            provider_key: "p".to_owned(),
            provider_config: raw.map(str::to_owned),
            account_key: None,
            created_at: None,
            updated_at: None,
        };

        assert_eq!(make(Some("  ")).parsed_config(), None, "空白文本视为 None");
        assert_eq!(
            make(Some("not json")).parsed_config(),
            None,
            "非法 JSON 视为 None"
        );

        let parsed = make(Some(r#"{"root":"/data"}"#)).parsed_config().unwrap();
        assert_eq!(parsed["root"], "/data");
    }
}
