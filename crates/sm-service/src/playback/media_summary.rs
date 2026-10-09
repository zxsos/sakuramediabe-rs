//! 影片媒体摘要，对应上游
//! `src/service/playback/media_summary_service.py`（40 行）+ 它的调用方
//! `src/service/catalog/movie_list_media_service.py`（14 行）。
//!
//! # 它解决的是 N+1
//!
//! 上游把两个文件拆开，但职责是一件事：
//!
//! ```python
//! summaries = list_movie_media_summaries([m.movie_number for m in movies])
//! for movie in movies:
//!     movie.media_items = summaries.get(movie.movie_number, [])
//! ```
//!
//! 第一句是**一条**带 `IN` 的查询（外加一次 `LEFT JOIN` 取库名），第二句
//! 在内存里按番号分组。所以「一部影片有多少媒体」「能不能播」这些字段
//! **不会**变成每部影片一次查询。
//!
//! 上游用「显式覆盖同名反向关联」实现（给 Peewee 的 model 实例挂属性），
//! 那是 Python 动态属性的写法。Rust 侧没有那个能力，所以这里返回
//! `HashMap<String, Vec<MediaSummary>>` 让调用方自己组装 —— 语义一致，
//! 而且「查了但没挂上」这种错误在类型层面就做不到。
//!
//! # `LEFT JOIN` 而不是 `JOIN` —— 跟着上游，不是为了孤儿媒体
//!
//! 上游写的是 `JOIN.LEFT_OUTER`，这里照抄。**理由不是**「孤儿媒体可能存在」：
//! `media_library_id_fk` 是 `ON DELETE CASCADE`（`docker/schema.sql:512`），
//! 删库会连带删掉媒体，所以孤儿媒体在当前 DDL 下**不可能出现**。
//!
//! 保留左连接的实际理由：这三个 `library_*` 字段在 DTO 里声明为**可空**，
//! 左连接是那个声明成立的前提。改成内连接后 `Option` 永远不会为 `None`，
//! 而类型仍在说「可能没有」—— 某天有人把外键放宽成 `SET NULL` 就会突然
//! 解码报错。
//!
//! # `can_play` = 至少一条**有效**媒体
//!
//! 不是「有媒体」，也不是「全部有效」。一条有效 + 五条判死的影片是**能播**的。
//! 这条语义同时被 `list_playlist_movies` 与影片卡片端点依赖。

use std::collections::HashMap;

use sm_db::Db;

use crate::error::ServiceError;

/// 一条媒体的展示摘要。字段集合照抄上游 `MediaSummaryResource`。
///
/// # 不含 `media_id` 以外的 `media` 列
///
/// 上游的 SELECT 显式列了 10 个字段（`Media.select(...)`），不是 `SELECT *`。
/// 跟着走：摘要用于列表渲染，而 `storage_ref`（可能含凭据）与
/// `video_info`（可能很大）都不该因为「顺手」被带出来。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSummary {
    /// `media.id`。
    pub media_id: i32,
    /// 所属库。`None` 只在**左连接未命中**时出现 —— 而
    /// `ON DELETE CASCADE` 下孤儿媒体不可能存在，所以实践里恒为 `Some`。
    /// 保留 `Option` 是为了与上游 DTO 的可空声明一致。
    pub library_id: Option<i32>,
    /// 库名。`None` 同上。
    pub library_name: Option<String>,
    /// provider 键。`None` 同上。
    ///
    /// 客户端用它决定用哪个 provider 的播放/下载能力。
    pub provider_key: Option<String>,
    pub file_name: String,
    pub resolution: Option<String>,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
    /// `JsonTextField`：JSON 存 TEXT，**可能是脏文本**。
    ///
    /// 刻意保留为 `Option<String>` 而不是解析成 `serde_json::Value`：
    /// 解析失败时上游会 500，而摘要的用途是渲染列表 —— 一个坏 `video_info`
    /// 不该让整个列表挂掉。调用方要读结构时自己 `serde_json::from_str`。
    pub video_info: Option<String>,
    pub valid: bool,
}

impl MediaSummary {
    /// 这条媒体是否「可播放」的**候选**：它自己是有效的。
    ///
    /// 影片级的 `can_play` 是 `any(候选)`，所以这个方法只回答单条。
    pub fn is_playable(&self) -> bool {
        self.valid
    }
}

/// 一部影片的媒体摘要 + 派生字段。
///
/// 这三个字段是上游挂在 `Movie` 实例上的三个属性，语义都属于「派生」——
/// 它们完全由 `media_items` 决定，所以放在这里而不在 `Movie` 上。
/// `Default` 是**「没有媒体」**这个事实的取值：空列表、计数 0、不能播。
/// 卡片组装时用它兜底（`attach_movie_list_media` 对每个问到的番号都会给一项，
/// 兜底路径实际不可达 —— 但 `unwrap_or_default` 比 `expect` 好，因为 release
/// 是 `panic = "abort"`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MovieMediaAttachment {
    /// 按 `media.id` 升序（与上游 `ORDER BY Media.movie, Media.id` 一致）。
    pub media_items: Vec<MediaSummary>,
    /// = `media_items.len()`。
    pub media_count: i64,
    /// **至少一条**有效媒体。
    ///
    /// 注意不是「全部有效」也不是「有媒体」：一条有效 + 五条判死 = 能播。
    pub can_play: bool,
}

/// 批量取回若干影片的媒体摘要，按番号分组。
///
/// # 空输入直接返回空 map，不发查询
///
/// 上游是 `if not movie_numbers: return {}`。列表页在筛选后可能一页都没有
/// 影片，此时发一条 `IN ()` 查询既没意义又会让 sqlx 拼出非法 SQL。
///
/// # 结果里**可能**有请求没问到的番号
///
/// 不会 —— `WHERE movie_number = ANY($1)` 保证了范围。但**某个番号可能不在
/// 结果里**（那部影片没有媒体），所以调用方要用
/// `get(&number).unwrap_or_default()` 而不是 `expect`。
///
/// # 不按影片分组去重同番号
///
/// 键是 `movie_number`（字符串业务主键），一部影片的多条媒体各占一项。
/// 调用方要的是「这部影片的媒体列表」，不是「每个番号一行」。
pub async fn list_movie_media_summaries(
    db: &Db,
    movie_numbers: &[String],
) -> Result<HashMap<String, Vec<MediaSummary>>, ServiceError> {
    if movie_numbers.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sm_db::repo::MediaRepository::new(db.clone())
        .summaries_for_movies(movie_numbers)
        .await?;

    let mut grouped: HashMap<String, Vec<MediaSummary>> = HashMap::new();
    for row in rows {
        // 元组解包顺序与 `MediaRepository::MediaSummaryRow` 的定义逐条对应。
        // 那个顺序是**契约** —— 两处必须一起改，所以下面按位置解构而不是
        // 逐字段赋值（后者在加列时会被静默忽略）。
        let (
            movie_number,
            media_id,
            library_id,
            library_name,
            provider_key,
            file_name,
            resolution,
            file_size_bytes,
            duration_seconds,
            video_info,
            valid,
        ) = row;
        grouped.entry(movie_number).or_default().push(MediaSummary {
            media_id,
            library_id,
            library_name,
            provider_key,
            file_name,
            resolution,
            file_size_bytes,
            duration_seconds,
            video_info,
            valid,
        });
    }
    Ok(grouped)
}

/// 上游 `attach_movie_list_media` 的等价物：给每个番号算出派生字段。
///
/// 一次查询 + 内存分组，与上游一致。返回 `HashMap` 而不是原地修改 ——
/// 见类型文档里「显式覆盖同名反向关联」的说明。
pub async fn attach_movie_list_media(
    db: &Db,
    movie_numbers: &[String],
) -> Result<HashMap<String, MovieMediaAttachment>, ServiceError> {
    let summaries = list_movie_media_summaries(db, movie_numbers).await?;
    Ok(movie_numbers
        .iter()
        .map(|number| {
            // 没有媒体 = 三个字段全 0/false。**不是**「查不到这部影片」——
            // 那个概念在这里不存在：我们只被问了番号，没有「影片是否存在」这回事。
            let media_items = summaries.get(number).cloned().unwrap_or_default();
            let media_count = media_items.len() as i64;
            let can_play = media_items.iter().any(MediaSummary::is_playable);
            (
                number.clone(),
                MovieMediaAttachment {
                    media_items,
                    media_count,
                    can_play,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条最小摘要。
    fn media(id: i32, valid: bool) -> MediaSummary {
        MediaSummary {
            media_id: id,
            library_id: Some(1),
            library_name: Some("lib".to_owned()),
            provider_key: Some("local".to_owned()),
            file_name: format!("{id}.mp4"),
            resolution: Some("1920x1080".to_owned()),
            file_size_bytes: 1024,
            duration_seconds: 60,
            video_info: None,
            valid,
        }
    }

    /// `can_play` 是 **any**，不是 all —— 一条有效就够。
    #[test]
    fn can_play_is_any_valid_media_not_all() {
        // 一条有效 + 五条判死 -> 能播
        let mixed = [media(1, false), media(2, true), media(3, false)];
        assert!(
            mixed.iter().any(MediaSummary::is_playable),
            "只要有一条有效就该能播"
        );
        // 全部判死 -> 不能播
        let all_dead = [media(1, false), media(2, false)];
        assert!(!all_dead.iter().any(MediaSummary::is_playable));
        // 一条都没有 -> 不能播
        assert!(!Vec::<MediaSummary>::new()
            .iter()
            .any(MediaSummary::is_playable));
    }

    #[test]
    fn an_orphan_media_still_counts_even_without_library_fields() {
        // `LEFT JOIN` 的意义：库被删了，媒体仍在，计数必须是 1
        let orphan = MediaSummary {
            library_id: None,
            library_name: None,
            provider_key: None,
            ..media(9, true)
        };
        assert!(orphan.is_playable());
        assert_eq!(orphan.media_id, 9);
    }

    #[test]
    fn a_single_media_is_playable_when_valid() {
        assert!(media(1, true).is_playable());
        assert!(!media(1, false).is_playable());
    }
}
