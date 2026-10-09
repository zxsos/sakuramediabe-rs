//! `daily_recommendation_item` 与 `moment_recommendation` 的集成测试。
//!
//! # 这两张表与 `ranking_item` 语义相反，本文件的全部重点
//!
//! | 表 | `rank` 的唯一性 | 后果 |
//! |---|---|---|
//! | `ranking_item` | `UNIQUE (source_key, board_key, period, rank)` | 榜单内唯一 → 累积历史，按 rank upsert |
//! | `daily_recommendation_item` | `rank integer NOT NULL UNIQUE` | **全表**唯一 → 必须清空重写 |
//! | `moment_recommendation` | `rank integer NOT NULL UNIQUE` | 同上 |
//!
//! 下面第一个测试用**真实的唯一约束**证明「第二天写 rank=1 会撞第一天
//! 的」—— 而不是靠注释声称。那是选择 `replace_all` 而不是 upsert 的理由。

use chrono::{NaiveDate, NaiveDateTime};
use sm_db::common::page::PageRequest;
use sm_db::discovery::rankings::MomentSeedKind;
use sm_db::error::DbError;
use sm_db::repo::{
    DailyRecommendationItemRepository, MomentRecommendationRepository, MovieRepository,
    NewDailyRecommendation, NewMedia, NewMediaLibrary, NewMomentRecommendation, NewMovie,
};
use sm_db::testing::TestDb;

fn page() -> PageRequest {
    PageRequest::new(1, 50).unwrap()
}

fn now() -> NaiveDateTime {
    sm_db::common::time::now_utc()
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

async fn seed_movie(db: &TestDb) -> i32 {
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("REC-{:06}", n()),
            title: "推荐影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 0,
            score: 0.0,
            score_number: 0,
            series_id: None,
            cover_image_id: None,
            thin_cover_image_id: None,
            metadata_source: None,
        })
        .await
        .expect("insert movie")
        .id
}

/// 建一个 `media` 行 —— `moment_recommendation.media_id` 指向它。
async fn seed_media(db: &TestDb, movie_id: i32) -> i32 {
    let movie_number: String = sqlx::query_scalar("SELECT movie_number FROM movie WHERE id = $1")
        .bind(movie_id)
        .fetch_one(db.pool())
        .await
        .expect("read movie_number");
    let library = sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id;
    sm_db::repo::MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id: library,
            file_name: "rec.mp4".to_owned(),
            file_size_bytes: 1,
            movie_number: Some(movie_number),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id
}

/// 建一个 `image` 行，返回 id。
async fn seed_image(db: &TestDb) -> i32 {
    sm_db::repo::ImageRepository::new(db.pool().clone())
        .upsert(&sm_db::repo::NewImage {
            origin: format!("rec/{}.jpg", n()),
        })
        .await
        .expect("upsert image")
        .0
}

/// 建一个 `media_thumbnail` 行 —— `moment_recommendation.thumbnail_id` 指向它。
///
/// **必须传不同的 `offset`**：`media_thumbnail` 的唯一索引是
/// `(media_id, offset)`，同一个 offset 上的 upsert 会返回**同一行** ——
/// 于是「两个不同的推荐时刻」会撞 `thumbnail_id` 的唯一约束。第一版夹具
/// 恒传 0，正是这么错的。
///
/// 签名是 `upsert(media_id, offset_seconds, image_id, index_status)` ——
/// 重跑缩略图生成是常态（第一次质量不达标），所以是 upsert 而非 insert。
async fn seed_thumbnail(db: &TestDb, media_id: i32, offset_seconds: i32, image_id: i32) -> i32 {
    sm_db::repo::playback::MediaThumbnailRepository::new(db.pool().clone())
        .upsert(media_id, offset_seconds, image_id, 0)
        .await
        .expect("upsert media_thumbnail")
        .id
}

// ================================================================ daily_recommendation_item

#[tokio::test]
async fn rank_is_globally_unique_so_a_second_batch_cannot_be_written_alongside() {
    // **本文件最重要的一处，且它证明了为什么 `replace_all` 是唯一正确写法。**
    //
    // `daily_recommendation_item.rank` 是 `UNIQUE`，**不**按 `snapshot_date`
    // 分组。所以昨天那批占用了 rank 1..N，今天再写 rank=1 会**直接撞昨天
    // 那一行**。
    //
    // 这里不用注释声称，而是让数据库自己说：先写一批，再尝试写第二天的
    // 一批，断言它撞约束。
    let db = TestDb::require().await;
    let repo = DailyRecommendationItemRepository::new(db.pool().clone());

    let m1 = seed_movie(&db).await;
    let m2 = seed_movie(&db).await;
    let day1 = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
    let day2 = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();

    repo.replace_all(&[
        NewDailyRecommendation {
            snapshot_date: day1,
            movie_id: m1,
            rank: 1,
            score: 0.9,
            reason_codes: None,
            reason_texts: None,
            signal_scores: None,
            generated_at: now(),
        },
        NewDailyRecommendation {
            snapshot_date: day1,
            movie_id: m2,
            rank: 2,
            score: 0.8,
            reason_codes: None,
            reason_texts: None,
            signal_scores: None,
            generated_at: now(),
        },
    ])
    .await
    .unwrap();
    assert_eq!(repo.list_by_rank().await.unwrap().len(), 2);

    // 第二天的一批：rank=1 会与昨天撞。
    //
    // 这里用 `replace_all` 之外的路径写入 —— 那正是「逐条 upsert」的真实
    // 形态。没有为此给生产代码加测试专用入口：`insert_in` 已经是公开的事务
    // 内变体，配一个一次性 `Ctx` 就能表达「只插这一条，不清空」。
    let m3 = seed_movie(&db).await;
    let mut tx = db.pool().begin().await.unwrap();
    let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, db.pool());
    let err = repo
        .insert_in(
            &mut ctx,
            &NewDailyRecommendation {
                snapshot_date: day2,
                movie_id: m3,
                rank: 1,
                score: 0.95,
                reason_codes: None,
                reason_texts: None,
                signal_scores: None,
                generated_at: now(),
            },
        )
        .await
        .expect_err("rank 全表唯一，第二天写 rank=1 必撞第一天");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );
    tx.rollback().await.unwrap();

    // 正确的做法：整批替换。
    repo.replace_all(&[NewDailyRecommendation {
        snapshot_date: day2,
        movie_id: m3,
        rank: 1,
        score: 0.95,
        reason_codes: None,
        reason_texts: None,
        signal_scores: None,
        generated_at: now(),
    }])
    .await
    .unwrap();

    let listed = repo.list_by_rank().await.unwrap();
    assert_eq!(listed.len(), 1, "旧的整批被替换掉了");
    assert_eq!(listed[0].movie_id, m3);
    assert_eq!(listed[0].snapshot_date, day2);
}
#[tokio::test]
async fn json_columns_get_their_ddl_defaults_when_the_caller_omits_them() {
    // `reason_codes` / `reason_texts` / `signal_scores` 是
    // `text NOT NULL DEFAULT '[]'`（与 `'{}'`）—— **有 DEFAULT 但不可空**。
    //
    // 调用方传 `None` 时不能绑 NULL（会违反 NOT NULL），必须补上 DDL 里那
    // 个默认值。`Option` 在这里表达「调用方没提供」，不是「允许存 NULL」。
    let db = TestDb::require().await;
    let repo = DailyRecommendationItemRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let day = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();

    repo.replace_all(&[NewDailyRecommendation {
        snapshot_date: day,
        movie_id: movie,
        rank: 1,
        score: 0.5,
        reason_codes: None,
        reason_texts: None,
        signal_scores: None,
        generated_at: now(),
    }])
    .await
    .unwrap();

    let listed = repo.list_by_rank().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].reason_codes.as_deref(), Some("[]"));
    assert_eq!(listed[0].reason_texts.as_deref(), Some("[]"));
    assert_eq!(listed[0].signal_scores.as_deref(), Some("{}"));
    assert!(!listed[0].reason_arity_mismatch(), "两个空数组长度一致");

    // 显式给出时按原样写，且理由代码与文案数量应当一致。
    let m2 = seed_movie(&db).await;
    repo.replace_all(&[NewDailyRecommendation {
        snapshot_date: day,
        movie_id: m2,
        rank: 1,
        score: 0.5,
        reason_codes: Some(r#"["hot","new"]"#.to_owned()),
        reason_texts: Some(r#"["热门","新片"]"#.to_owned()),
        signal_scores: Some(r#"{"hot":0.9}"#.to_owned()),
        generated_at: now(),
    }])
    .await
    .unwrap();
    let listed = repo.list_by_rank().await.unwrap();
    assert_eq!(
        listed[0].parsed_reason_codes(),
        Some(vec!["hot".to_owned(), "new".to_owned()])
    );
    assert!(!listed[0].reason_arity_mismatch());
    assert!(listed[0].parsed_signal_scores().is_some());
}

#[tokio::test]
async fn rank_zero_is_rejected_and_a_movie_appears_at_most_once() {
    let db = TestDb::require().await;
    let repo = DailyRecommendationItemRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let day = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();

    let mut tx = db.pool().begin().await.unwrap();
    let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, db.pool());
    let err = repo
        .insert_in(
            &mut ctx,
            &NewDailyRecommendation {
                snapshot_date: day,
                movie_id: movie,
                rank: 0,
                score: 0.5,
                reason_codes: None,
                reason_texts: None,
                signal_scores: None,
                generated_at: now(),
            },
        )
        .await
        .expect_err("rank 从 1 开始");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    tx.rollback().await.unwrap();

    // `movie_id` 也全表唯一 —— 一部影片至多一条。
    repo.replace_all(&[
        NewDailyRecommendation {
            snapshot_date: day,
            movie_id: movie,
            rank: 1,
            score: 0.5,
            reason_codes: None,
            reason_texts: None,
            signal_scores: None,
            generated_at: now(),
        },
        NewDailyRecommendation {
            snapshot_date: day,
            movie_id: movie,
            rank: 2,
            score: 0.4,
            reason_codes: None,
            reason_texts: None,
            signal_scores: None,
            generated_at: now(),
        },
    ])
    .await
    .expect_err("同一部影片不能占两个推荐位");
}

#[tokio::test]
async fn a_recommendation_cannot_outlive_its_movie() {
    // `daily_recommendation_item_movie_id_fk` 是 `ON DELETE CASCADE`。
    let db = TestDb::require().await;
    let repo = DailyRecommendationItemRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let day = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();

    repo.replace_all(&[NewDailyRecommendation {
        snapshot_date: day,
        movie_id: movie,
        rank: 1,
        score: 0.5,
        reason_codes: None,
        reason_texts: None,
        signal_scores: None,
        generated_at: now(),
    }])
    .await
    .unwrap();
    assert!(repo.find_by_movie(movie).await.unwrap().is_some());

    sqlx::query("DELETE FROM movie WHERE id = $1")
        .bind(movie)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        repo.find_by_movie(movie).await.unwrap().is_none(),
        "影片被删，推荐随之消失（CASCADE）"
    );
    assert!(repo.list_by_rank().await.unwrap().is_empty());
}
// ================================================================ moment_recommendation

/// 造一条 `moment_recommendation` 的入参。
#[allow(clippy::too_many_arguments)]
fn moment(
    rank: i32,
    movie_id: i32,
    media_id: i32,
    thumbnail_id: i32,
    strategy: &str,
) -> NewMomentRecommendation {
    NewMomentRecommendation {
        rank,
        score: 0.5,
        strategy: strategy.to_owned(),
        reason: "推荐理由".to_owned(),
        movie_id,
        media_id,
        thumbnail_id,
        offset_seconds: 12,
        seed_point_id: None,
        seed_thumbnail_id: None,
        source_movie_id: None,
        visual_score: None,
        movie_similarity_score: None,
        generated_at: now(),
    }
}

#[tokio::test]
async fn seed_fields_and_their_scores_must_be_given_together() {
    // 有视觉种子就该有视觉分数，反之亦然。数据库不校验这个（两列是独立的
    // 可空列），漏掉它不会崩，只会让客户端显示一个没有分数的理由 ——
    // 所以放在入口拦。
    let db = TestDb::require().await;
    let repo = MomentRecommendationRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let media = seed_media(&db, movie).await;
    let thumb = seed_thumbnail(&db, media, 0, seed_image(&db).await).await;

    let mut tx = db.pool().begin().await.unwrap();
    let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, db.pool());

    let mut bad = moment(1, movie, media, thumb, "visual");
    bad.seed_thumbnail_id = Some(thumb);
    bad.visual_score = None;
    let err = repo
        .insert_in(&mut ctx, &bad)
        .await
        .expect_err("有视觉种子却没有视觉分数");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");

    let mut bad2 = moment(1, movie, media, thumb, "movie");
    bad2.source_movie_id = Some(movie);
    bad2.movie_similarity_score = None;
    let err = repo
        .insert_in(&mut ctx, &bad2)
        .await
        .expect_err("有影片种子却没有影片相似度分数");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    tx.rollback().await.unwrap();

    // 同时给出或同时留空都可以。
    let mut good = moment(1, movie, media, thumb, "visual");
    good.seed_thumbnail_id = Some(thumb);
    good.visual_score = Some(0.8);
    repo.replace_all(&[good]).await.unwrap();

    let listed = repo.list_by_rank().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].seed_kind(), MomentSeedKind::Thumbnail);
    assert!(listed[0].has_seed());
    assert!(!listed[0].has_both_scores(), "只有一种种子");
}
#[tokio::test]
async fn a_thumbnail_is_recommended_at_most_once_but_a_movie_may_have_many_moments() {
    // 唯一性落在 `thumbnail_id` 上，不是 `movie_id` —— 同一部影片的
    // **不同时刻**是不同的推荐。
    let db = TestDb::require().await;
    let repo = MomentRecommendationRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let media = seed_media(&db, movie).await;
    let thumb1 = seed_thumbnail(&db, media, 0, seed_image(&db).await).await;
    let thumb2 = seed_thumbnail(&db, media, 30, seed_image(&db).await).await;

    repo.replace_all(&[
        moment(1, movie, media, thumb1, "visual"),
        moment(2, movie, media, thumb2, "visual"),
    ])
    .await
    .unwrap();
    let by_movie = repo.list_by_movie(movie, page()).await.unwrap();
    assert_eq!(by_movie.total, 2, "同一影片可以有多个推荐时刻");
    assert_eq!(by_movie.total as usize, by_movie.items.len());

    // 同一个缩略图出现两次 —— 撞唯一约束。
    let err = repo
        .replace_all(&[
            moment(1, movie, media, thumb1, "visual"),
            moment(2, movie, media, thumb1, "visual"),
        ])
        .await
        .expect_err("一个缩略图至多被推荐一次");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn unexplainable_recommendations_can_be_found_on_their_own() {
    // 三个 seed 全空意味着无法回答「为什么推荐这个」。推荐本身仍有效不该
    // 丢弃，但值得能被单独查出来 —— 那通常意味着检索路径出了问题。
    //
    // 判定与 `MomentRecommendation::has_seed` 一致，这里同时断言两者。
    let db = TestDb::require().await;
    let repo = MomentRecommendationRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let media = seed_media(&db, movie).await;
    let t1 = seed_thumbnail(&db, media, 0, seed_image(&db).await).await;
    let t2 = seed_thumbnail(&db, media, 30, seed_image(&db).await).await;

    // 可解释：有影片种子 + 相似度分数。
    let mut explained = moment(1, movie, media, t1, "movie");
    explained.source_movie_id = Some(movie);
    explained.movie_similarity_score = Some(0.7);
    // 不可解释：三个 seed 全空。
    let plain = moment(2, movie, media, t2, "fallback");

    repo.replace_all(&[explained, plain]).await.unwrap();

    let unexplainable = repo.list_unexplainable(page()).await.unwrap();
    assert_eq!(unexplainable.total, 1, "只有一条说不出理由");
    assert_eq!(unexplainable.total as usize, unexplainable.items.len());
    let row = &unexplainable.items[0];
    assert!(!row.has_seed(), "与 has_seed 的判定必须一致");
    assert_eq!(row.seed_kind(), MomentSeedKind::None);

    let all = repo.list_by_rank().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all.iter().filter(|r| r.has_seed()).count(), 1);
}

#[tokio::test]
async fn seed_rows_are_nulled_not_deleted_when_their_source_goes_away() {
    // `seed_point_id` / `seed_thumbnail_id` / `source_movie_id` 三个外键都是
    // `ON DELETE SET NULL`，而 `movie_id` / `media_id` / `thumbnail_id` 是
    // CASCADE。依据没了推荐仍然成立，只是失去可解释性。
    let db = TestDb::require().await;
    let repo = MomentRecommendationRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let media = seed_media(&db, movie).await;
    let thumb = seed_thumbnail(&db, media, 0, seed_image(&db).await).await;
    let seed_source = seed_movie(&db).await;

    let mut row = moment(1, movie, media, thumb, "movie");
    row.source_movie_id = Some(seed_source);
    row.movie_similarity_score = Some(0.6);
    repo.replace_all(&[row]).await.unwrap();
    assert!(repo
        .find_by_thumbnail(thumb)
        .await
        .unwrap()
        .unwrap()
        .has_seed());

    // 删掉种子影片：推荐还在，但依据变 NULL。
    sqlx::query("DELETE FROM movie WHERE id = $1")
        .bind(seed_source)
        .execute(db.pool())
        .await
        .unwrap();
    let after = repo.find_by_thumbnail(thumb).await.unwrap().unwrap();
    assert!(after.source_movie_id.is_none(), "依据应被 SET NULL");
    assert!(!after.has_seed(), "失去可解释性");
    assert_eq!(
        after.movie_similarity_score,
        Some(0.6),
        "分数本身保留 —— 那是历史得分，不是依据"
    );

    // 但目标缩略图被删会级联删掉整条推荐。
    sqlx::query("DELETE FROM media_thumbnail WHERE id = $1")
        .bind(thumb)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        repo.find_by_thumbnail(thumb).await.unwrap().is_none(),
        "目标没了（CASCADE），推荐不再成立"
    );
}
