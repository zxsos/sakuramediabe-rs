//! Media 族仓储的集成测试。
//!
//! 覆盖的是**唯一索引如何决定写入方式**——这是本文件的核心。
//! 三张表的唯一索引形状不同，upsert 语义也就不同：
//!
//! | 表 | 索引 | 测试验证的行为 |
//! |---|---|---|
//! | `media_thumbnail` | `(media_id, offset)` | 同时刻点重试 = 覆盖，不新增行，且 `created_at` 不变 |
//! | `media_progress` | `(media_id)` | 反复保存 = 一行，进度可覆盖 |
//! | `media_clip` | `(media_id, start, end)`，可空 | 同区间重复插入撞约束；`media_id=NULL` 的多个可共存 |

use sm_db::common::time::now_utc;
use sm_db::error::DbError;
use sm_db::playback::media::image_search_index_status;
use sm_db::repo::playback::{
    MediaClipRepository, MediaPointRepository, MediaProgressRepository, MediaThumbnailRepository,
    NewMediaClip,
};
use sm_db::repo::{MediaRepository, NewMedia};
use sm_db::testing::TestDb;

mod fixtures {
    use super::*;

    /// 挂在一个 movie_number 上的 Media。`library_id` 会被单独指定。
    pub fn media(movie_number: &str, library_id: i32) -> NewMedia {
        NewMedia {
            library_id,
            file_name: format!("{movie_number}.mp4"),
            file_size_bytes: 1024,
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some("1080p".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: Some(120),
            video_info: None,
        }
    }

    pub fn clip(media_id: Option<i32>, start: i32, end: i32) -> NewMediaClip {
        NewMediaClip {
            media_id,
            movie_number: Some("ABC-001".to_owned()),
            start_offset_seconds: start,
            end_offset_seconds: end,
            title: None,
            file_path: format!("clip-{start}-{end}.mp4"),
            file_size_bytes: 2048,
            duration_seconds: end - start,
        }
    }
}

/// 建一条 media，返回它的 id。
async fn seed_media(repo: &MediaRepository, movie_number: &str) -> i32 {
    repo.insert(&fixtures::media(movie_number, 1))
        .await
        .unwrap()
        .id
}

/// 建一条 media_library 行（`media.library_id` 指向它）。
///
/// `media_library` 还没有仓储，所以这里直接写 SQL —— 这正是缺口的
/// 样子：为了测试下游，得手工准备上游的父行。
async fn seed_library(pool: &sqlx::PgPool, name: &str) -> i32 {
    let row = sqlx::query_as::<_, (i32,)>(
        "INSERT INTO media_library (name, created_at, updated_at) VALUES ($1, $2, $2) RETURNING id",
    )
    .bind(name)
    .bind(now_utc())
    .fetch_one(pool)
    .await
    .expect("插入 media_library 失败");
    row.0
}

/// 建一条 `image` 行（`media_thumbnail.image_id` 指向它）。
async fn seed_image(pool: &sqlx::PgPool, key: &str) -> i32 {
    let row = sqlx::query_as::<_, (i32,)>(
        "INSERT INTO image (image_key, created_at, updated_at) VALUES ($1, $2, $2) RETURNING id",
    )
    .bind(key)
    .bind(now_utc())
    .fetch_one(pool)
    .await
    .expect("插入 image 失败");
    row.0
}

// ================================================================ 缩略图：接上断裂的闭环

#[tokio::test]
async fn thumbnail_upsert_lands_the_artifact_the_state_machine_claimed() {
    // 这是本文件存在的理由：record_thumbnail_success 说「生成成功」，
    // 而产物必须能落到 media_thumbnail。闭环在这里接上。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let image_id = seed_image(db.pool(), "thumb-0").await;

    // 状态机标记成功
    let media_repo2 = MediaRepository::new(db.pool().clone());
    let claimed = media_repo2.list_pending_thumbnails(10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let finished = media_repo2
        .record_thumbnail_success(media_id)
        .await
        .unwrap();
    assert_eq!(finished.thumbnail_generation_state, "succeeded");

    // 产物落库
    let thumb = thumbs
        .upsert(media_id, 0, image_id, image_search_index_status::PENDING)
        .await
        .unwrap();
    assert_eq!(thumb.media_id, media_id);
    assert_eq!(thumb.image_id, image_id);
    assert_eq!(thumb.offset, 0);
    assert_eq!(
        thumb.image_search_index_status,
        image_search_index_status::PENDING
    );

    // 库里确实有这一行
    let listed = thumbs.list_by_media(media_id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, thumb.id);
}

#[tokio::test]
async fn thumbnail_upsert_at_the_same_offset_overwrites_rather_than_duplicating() {
    // 唯一索引 (media_id, offset) + 重试是常态 -> 必须覆盖不新增。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let first_image = seed_image(db.pool(), "thumb-v1").await;
    let better_image = seed_image(db.pool(), "thumb-v2").await;

    let first = thumbs
        .upsert(
            media_id,
            120,
            first_image,
            image_search_index_status::PENDING,
        )
        .await
        .unwrap();
    let second = thumbs
        .upsert(
            media_id,
            120,
            better_image,
            image_search_index_status::PENDING,
        )
        .await
        .unwrap();

    // 同一行被覆盖
    assert_eq!(first.id, second.id, "同一时刻点应是同一行");
    assert_eq!(second.image_id, better_image, "图应被换成重试后的版本");
    assert_eq!(thumbs.list_by_media(media_id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn thumbnail_upsert_keeps_the_original_created_at_across_retries() {
    // created_at 记录的是「这个时刻点被首次识别出来」，重试不该改写它。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let first_image = seed_image(db.pool(), "thumb-v1").await;
    let second_image = seed_image(db.pool(), "thumb-v2").await;

    let first = thumbs
        .upsert(media_id, 0, first_image, image_search_index_status::PENDING)
        .await
        .unwrap();
    let first_created = first.created_at;

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let second = thumbs
        .upsert(
            media_id,
            0,
            second_image,
            image_search_index_status::PENDING,
        )
        .await
        .unwrap();
    assert_eq!(
        second.created_at, first_created,
        "重试不应改写首次产出的时刻"
    );
    assert!(second.updated_at > first.created_at, "但 updated_at 应推进");
}

#[tokio::test]
async fn thumbnail_upsert_rejects_an_unknown_index_status() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let image_id = seed_image(db.pool(), "thumb-0").await;

    let err = thumbs
        .upsert(media_id, 0, image_id, 99)
        .await
        .expect_err("未知状态应被拒");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");
    // 未写入
    assert!(thumbs.list_by_media(media_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn index_status_moves_through_its_own_state_machine() {
    // 这台状态机与 media 上的那台**刻意分开**：
    // 「有没有生成出来」与「有没有进检索索引」是两件事。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let image_id = seed_image(db.pool(), "thumb-0").await;
    let thumb = thumbs
        .upsert(media_id, 0, image_id, image_search_index_status::PENDING)
        .await
        .unwrap();

    // PENDING -> FAILED -> SUCCESS（重试后成功）
    let failed = thumbs.mark_index_failed(thumb.id).await.unwrap();
    assert_eq!(
        failed.image_search_index_status,
        image_search_index_status::FAILED
    );
    let succeeded = thumbs.mark_indexed(thumb.id).await.unwrap();
    assert_eq!(
        succeeded.image_search_index_status,
        image_search_index_status::SUCCESS
    );
    assert!(image_search_index_status::is_terminal(
        succeeded.image_search_index_status
    ));
}

#[tokio::test]
async fn skipped_is_a_valid_terminal_state_for_non_jav_media() {
    // 非 JAV 媒体的缩略图不进检索索引，但必须落明确终态，
    // 否则永久滞留 PENDING。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    // 挂 video_item（非 JAV）
    let mut m = fixtures::media("ignored", 1);
    m.movie_number = None;
    m.video_item_id = Some(1);
    // video_item_id 指向 video_item 表，先建它
    sqlx::query("INSERT INTO video_item (title, created_at, updated_at) VALUES ('x', $1, $1)")
        .bind(now_utc())
        .execute(db.pool())
        .await
        .unwrap();
    let media_id = media_repo.insert(&m).await.unwrap().id;

    let image_id = seed_image(db.pool(), "thumb-nonjav").await;
    let thumb = thumbs
        .upsert(media_id, 0, image_id, image_search_index_status::PENDING)
        .await
        .unwrap();
    let skipped = thumbs.mark_skipped(thumb.id).await.unwrap();
    assert_eq!(
        skipped.image_search_index_status,
        image_search_index_status::SKIPPED
    );
    assert!(image_search_index_status::is_terminal(
        skipped.image_search_index_status
    ));
}

#[tokio::test]
async fn pending_index_queue_returns_only_pending_rows() {
    // 索引是 (image_search_index_status, id) —— 只按状态取，不 join。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let img1 = seed_image(db.pool(), "a").await;
    let img2 = seed_image(db.pool(), "b").await;
    let img3 = seed_image(db.pool(), "c").await;

    let t1 = thumbs
        .upsert(media_id, 0, img1, image_search_index_status::PENDING)
        .await
        .unwrap();
    thumbs
        .upsert(media_id, 10, img2, image_search_index_status::PENDING)
        .await
        .unwrap();
    let t3 = thumbs
        .upsert(media_id, 20, img3, image_search_index_status::PENDING)
        .await
        .unwrap();
    thumbs.mark_indexed(t1.id).await.unwrap();

    let pending = thumbs
        .list_by_index_status(image_search_index_status::PENDING, 50)
        .await
        .unwrap();
    assert_eq!(pending.len(), 2, "只有两个 PENDING");
    let ids: Vec<i32> = pending.iter().map(|t| t.id).collect();
    assert!(ids.contains(&t3.id));

    // 终态的行不再出现
    let done = thumbs
        .list_by_index_status(image_search_index_status::SUCCESS, 50)
        .await
        .unwrap();
    assert_eq!(done.len(), 1);
}

#[tokio::test]
async fn mark_indexed_of_a_missing_row_reports_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());
    let err = thumbs.mark_indexed(999_999).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }), "实际 {err:?}");
}

// ================================================================ 进度

#[tokio::test]
async fn progress_save_is_upsert_not_insert() {
    // media_id 上有单列唯一索引 -> 反复保存是一行。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let progress = MediaProgressRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let first = progress.save(media_id, 30).await.unwrap();
    let second = progress.save(media_id, 90).await.unwrap();

    assert_eq!(first.id, second.id, "同一条 Media 只有一行进度");
    assert_eq!(second.position_seconds, 90, "位置应被覆盖");
    assert!(second.last_watched_at.is_some());
}

#[tokio::test]
async fn progress_may_move_backwards_on_request() {
    // 真实场景：用户拖回去重看。「只许前进」会让这些操作
    // 看起来成功却没生效 —— 比倒退本身更糟。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let progress = MediaProgressRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    progress.save(media_id, 300).await.unwrap();
    let back = progress.save(media_id, 30).await.unwrap();
    assert_eq!(back.position_seconds, 30, "回看重看是合法操作");
}

#[tokio::test]
async fn progress_rejects_a_negative_position() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let progress = MediaProgressRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let err = progress.save(media_id, -1).await.expect_err("负数应被拒");
    assert!(err.to_string().contains("不能为负"), "{err}");
}

#[tokio::test]
async fn progress_clear_deletes_rather_than_zeroing() {
    // 归零会让「看到片尾」与「刚开始看」变成同一个值。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let progress = MediaProgressRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    progress.save(media_id, 300).await.unwrap();

    assert!(progress.clear(media_id).await.unwrap(), "应删掉一行");
    assert!(progress.find_by_media(media_id).await.unwrap().is_none());
    // 再删一次返回 false（幂等）
    assert!(!progress.clear(media_id).await.unwrap());
}

#[tokio::test]
async fn progress_load_many_avoids_n_plus_one() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let progress = MediaProgressRepository::new(db.pool().clone());

    let watched = seed_media(&media_repo, "ABC-001").await;
    let unwatched = seed_media(&media_repo, "ABC-002").await;
    progress.save(watched, 60).await.unwrap();

    let loaded = progress.load_many(&[watched, unwatched]).await.unwrap();
    assert_eq!(loaded.len(), 1, "只有看过的那条有进度");
    assert_eq!(loaded[&watched].position_seconds, 60);
    assert!(!loaded.contains_key(&unwatched), "未看过的不出现在结果里");

    // 空输入不查库
    assert!(progress.load_many(&[]).await.unwrap().is_empty());
}

// ================================================================ 时刻点

#[tokio::test]
async fn point_insert_and_list_by_media() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let points = MediaPointRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let img1 = seed_image(db.pool(), "p1").await;
    let img2 = seed_image(db.pool(), "p2").await;

    points
        .insert(img2, 200, Some(media_id), Some("ABC-001"), None)
        .await
        .unwrap();
    points
        .insert(img1, 100, Some(media_id), Some("ABC-001"), None)
        .await
        .unwrap();

    let listed = points.list_by_media(media_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].offset_seconds, 100, "应按时刻升序");
    assert_eq!(listed[1].offset_seconds, 200);
}

#[tokio::test]
async fn point_survives_its_source_media_being_deleted() {
    // on_delete = SET NULL：来源删后时刻点仍在，快照列仍能归属与展示。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let points = MediaPointRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let image_id = seed_image(db.pool(), "p1").await;
    let point = points
        .insert(image_id, 100, Some(media_id), Some("ABC-001"), None)
        .await
        .unwrap();

    // 删来源 Media
    sqlx::query("DELETE FROM media WHERE id = $1")
        .bind(media_id)
        .execute(db.pool())
        .await
        .unwrap();

    // 时刻点还在，但 media_id 变 NULL
    let orphans = points.list_orphaned(10).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, point.id);
    assert!(orphans[0].media_id.is_none(), "media_id 应被置空");
    assert_eq!(
        orphans[0].movie_number.as_deref(),
        Some("ABC-001"),
        "快照列让归属与展示仍可用"
    );
}

#[tokio::test]
async fn point_delete_removes_the_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let points = MediaPointRepository::new(db.pool().clone());
    let image_id = seed_image(db.pool(), "p1").await;

    let point = points
        .insert(image_id, 100, None, Some("ABC-001"), None)
        .await
        .unwrap();
    assert!(points.delete(point.id).await.unwrap());
    assert!(!points.delete(point.id).await.unwrap(), "重复删返回 false");
}

// ================================================================ 片段

#[tokio::test]
async fn clip_insert_and_list() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let clips = MediaClipRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let c1 = clips
        .insert(&fixtures::clip(Some(media_id), 0, 30))
        .await
        .unwrap();
    let c2 = clips
        .insert(&fixtures::clip(Some(media_id), 60, 90))
        .await
        .unwrap();

    assert_eq!(c1.length_seconds(), 30);
    assert_eq!(c2.length_seconds(), 30);

    let listed = clips.list_by_media(media_id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].start_offset_seconds, 0);
}

#[tokio::test]
async fn clip_duplicate_range_on_the_same_source_hits_the_unique_constraint() {
    // (media_id, start, end) 唯一 —— 同一来源的同一区间不重复登记。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let clips = MediaClipRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    clips
        .insert(&fixtures::clip(Some(media_id), 0, 30))
        .await
        .unwrap();
    let err = clips
        .insert(&fixtures::clip(Some(media_id), 0, 30))
        .await
        .expect_err("同区间应撞唯一约束");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应归为 409，实际 {err:?}"
    );
}

#[tokio::test]
async fn clip_range_is_rejected_when_inverted_or_path_is_blank() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clips = MediaClipRepository::new(db.pool().clone());

    let err = clips
        .insert(&fixtures::clip(None, 90, 30))
        .await
        .expect_err("倒置区间应被拒");
    assert!(err.to_string().contains("不能小于"), "{err}");

    let mut blank = fixtures::clip(None, 0, 30);
    blank.file_path = "  ".to_owned();
    let err = clips.insert(&blank).await.expect_err("空路径应被拒");
    assert!(err.to_string().contains("file_path"), "{err}");
}

#[tokio::test]
async fn detached_clips_coexist_because_null_does_not_join_unique_constraints() {
    // 唯一索引含可空的 media_id，NULL 不参与约束 —— 这正是期望行为：
    // 多个「来源已删除」的片段可以共存。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clips = MediaClipRepository::new(db.pool().clone());

    // 三个都 media_id=NULL，且区间也相同 —— 若 media_id 参与约束就会撞
    for i in 0..3 {
        let mut c = fixtures::clip(None, 0, 30);
        c.file_path = format!("orphan-{i}.mp4");
        clips.insert(&c).await.unwrap();
    }
    let detached = clips.list_detached(10).await.unwrap();
    assert_eq!(detached.len(), 3, "NULL 不参与唯一约束，三个应共存");
    assert!(clips.list_attached(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn clip_snapshot_column_survives_source_deletion() {
    // 来源删后片段仍在，快照列让归属与展示仍可用。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let clips = MediaClipRepository::new(db.pool().clone());

    let media_id = seed_media(&media_repo, "ABC-001").await;
    let clip = clips
        .insert(&fixtures::clip(Some(media_id), 0, 30))
        .await
        .unwrap();

    sqlx::query("DELETE FROM media WHERE id = $1")
        .bind(media_id)
        .execute(db.pool())
        .await
        .unwrap();

    let orphans = clips.list_detached(10).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].id, clip.id);
    assert!(orphans[0].media_id.is_none(), "media_id 应被置空");
    // 快照仍能归属到影片
    let by_movie = clips.list_by_movie_number("ABC-001", 10).await.unwrap();
    assert_eq!(by_movie.len(), 1);
}

// ================================================================ 业务键查询

#[tokio::test]
async fn file_hash_lookup_finds_the_same_file_across_libraries() {
    // file_hash 的模型注释：「跨存储识别重复文件的依据」。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let lib_a = seed_library(db.pool(), "库A").await;
    let lib_b = seed_library(db.pool(), "库B").await;

    let hash = "media-file-hash-v1:0123456789abcdef0123456789abcdef01234567";
    let mut m1 = fixtures::media("ABC-001", lib_a);
    m1.file_hash = Some(hash.to_owned());
    let mut m2 = fixtures::media("ABC-001", lib_b);
    m2.file_hash = Some(hash.to_owned());
    media_repo.insert(&m1).await.unwrap();
    media_repo.insert(&m2).await.unwrap();

    let found = media_repo.find_by_file_hash(hash).await.unwrap();
    assert_eq!(found.len(), 2, "同一文件在两个库各有一份，应都被找到");
    assert_eq!(found[0].library_id, lib_a);
    assert_eq!(found[1].library_id, lib_b);
}

#[tokio::test]
async fn file_hash_lookup_returns_empty_for_unknown_hash() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let found = media_repo
        .find_by_file_hash("media-file-hash-v1:doesnotexist")
        .await
        .unwrap();
    assert!(found.is_empty());
}

#[tokio::test]
async fn list_by_library_and_movie_number() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let media_repo = MediaRepository::new(db.pool().clone());
    let lib = seed_library(db.pool(), "库A").await;

    for number in ["ABC-001", "ABC-001", "ABC-002"] {
        media_repo
            .insert(&fixtures::media(number, lib))
            .await
            .unwrap();
    }

    let all = media_repo.list_by_library(lib, 50).await.unwrap();
    assert_eq!(all.len(), 3);
    let jav = media_repo
        .list_by_movie_number("ABC-001", 50)
        .await
        .unwrap();
    assert_eq!(jav.len(), 2, "同一部影片的两份媒体");
    // 空白应被 trim
    assert_eq!(
        media_repo
            .list_by_movie_number("  ABC-002  ", 50)
            .await
            .unwrap()
            .len(),
        1
    );
}
