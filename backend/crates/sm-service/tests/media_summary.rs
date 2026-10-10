//! 影片媒体摘要的集成测试。
//!
//! # 两条必须连真库才验得了的语义
//!
//! **① `LEFT JOIN` 与孤儿媒体。** 媒体的 `library_id` 是外键，但外键是
//! `ON DELETE` 什么行为决定「库被删后媒体还在不在」。用 `JOIN` 而不是
//! `LEFT JOIN` 的后果是：那些媒体从摘要里整条消失，于是「这个影片有 3 个
//! 文件」变成「2 个」，而**用户完全不知道为什么**。这不会报错，只会让数字
//! 悄悄变小 —— 只有真的造一条孤儿媒体才看得见。
//!
//! **② `can_play` 是 any 而不是 all。** 一条有效 + 五条判死的影片是能播的。
//! 写成 all 会让「有 3 个文件、1 个能放、2 个判死」的影片显示成不能播，
//! 而用户点进去会发现能播。
//!
//! 两者都不是 `cargo test --lib` 能覆盖的：前者要真的插一行违反外键关系的
//! 数据，后者要真的走一遍聚合。

use sm_db::repo::{
    MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary, NewMovie,
};
use sm_db::testing::TestDb;
use sm_service::playback::{attach_movie_list_media, list_movie_media_summaries};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

async fn seed_movie(db: &TestDb) -> String {
    let number = format!("MS-{:06}", n());
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "影片".to_owned(),
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
        .expect("insert movie");
    number
}

async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

/// 给某部影片挂一条媒体。
async fn seed_media(db: &TestDb, movie_number: &str, library_id: i32, file: &str) -> i32 {
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("{file}-{}.mp4", n()),
            file_size_bytes: 1024,
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some("1920x1080".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 120,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id
}

// ================================================================ 基本形状

#[tokio::test]
async fn a_movie_without_media_gets_an_empty_list() {
    let db = TestDb::require().await;
    let number = seed_movie(&db).await;

    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("query");
    // 没有媒体 -> 键**不存在**（而不是空数组）。调用方用
    // `get(&n).unwrap_or_default()`，两种都安全。
    assert!(
        !summaries.contains_key(&number),
        "没有媒体的影片不该出现在结果里"
    );
    let attached = attach_movie_list_media(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("attach");
    let a = attached.get(&number).expect("每个请求的番号都要有结果");
    assert_eq!(a.media_count, 0);
    assert!(a.media_items.is_empty());
    assert!(!a.can_play, "没有媒体 -> 不能播");
}

#[tokio::test]
async fn an_empty_input_issues_no_query() {
    let db = TestDb::require().await;
    // 空数组不能变成 `IN ()` 那种非法 SQL
    let summaries = list_movie_media_summaries(db.pool(), &[]).await;
    assert!(summaries.expect("空输入").is_empty());
    let attached = attach_movie_list_media(db.pool(), &[]).await;
    assert!(attached.expect("空输入").is_empty());
}

/// 摘要带上库名与 provider 键 —— 客户端靠 `provider_key` 决定用哪个
/// provider 的播放能力。
#[tokio::test]
async fn the_summary_carries_the_library_name_and_provider_key() {
    let db = TestDb::require().await;
    let number = seed_movie(&db).await;
    let library_id = seed_library(&db).await;
    let library_name =
        sqlx::query_scalar::<_, String>("SELECT name FROM media_library WHERE id = $1")
            .bind(library_id)
            .fetch_one(db.pool())
            .await
            .expect("read library name");
    seed_media(&db, &number, library_id, "with-lib").await;

    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("query");
    let items = summaries.get(&number).expect("影片应有媒体");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].library_id, Some(library_id));
    assert_eq!(
        items[0].library_name.as_deref(),
        Some(library_name.as_str())
    );
    assert_eq!(items[0].provider_key.as_deref(), Some("local"));
    assert_eq!(items[0].file_size_bytes, 1024);
    assert_eq!(items[0].duration_seconds, 120);
    assert!(items[0].valid, "新插入的媒体默认有效");
}

/// 多部影片**一次查询**全部取回，且各自成组。
#[tokio::test]
async fn several_movies_are_fetched_in_one_call_and_grouped() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let a = seed_movie(&db).await;
    let b = seed_movie(&db).await;
    seed_media(&db, &a, library_id, "a1").await;
    seed_media(&db, &a, library_id, "a2").await;
    seed_media(&db, &b, library_id, "b1").await;

    let summaries = list_movie_media_summaries(db.pool(), &[a.clone(), b.clone()])
        .await
        .expect("query");
    assert_eq!(summaries.get(&a).map(Vec::len), Some(2), "a 有两条");
    assert_eq!(summaries.get(&b).map(Vec::len), Some(1), "b 有一条");
}

/// 同一影片的媒体按 `id` 升序 —— 顺序稳定，客户端的乐观更新才不闪。
#[tokio::test]
async fn a_movies_media_come_back_in_insertion_order() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let number = seed_movie(&db).await;
    for file in ["first", "second", "third"] {
        seed_media(&db, &number, library_id, file).await;
    }

    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("query");
    let ids: Vec<i32> = summaries
        .get(&number)
        .expect("影片")
        .iter()
        .map(|m| m.media_id)
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "必须按 media.id 升序，实际 {ids:?}");
}

// ================================================================ can_play 语义

/// 一条有效 + 若干判死 -> **能播**（any，不是 all）。
#[tokio::test]
async fn can_play_is_true_when_at_least_one_media_is_valid() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let number = seed_movie(&db).await;
    let good = seed_media(&db, &number, library_id, "good").await;
    let dead1 = seed_media(&db, &number, library_id, "dead1").await;
    let dead2 = seed_media(&db, &number, library_id, "dead2").await;

    // **按主键**判死那两条。按 file_name 匹配会在文件名重复时误伤，
    // 而「误伤有效媒体」恰好让本测试失去意义 —— 它要验的就是
    // 「一条有效 + 两条判死」这个组合。
    sqlx::query("UPDATE media SET valid = FALSE WHERE id = ANY($1)")
        .bind(vec![dead1, dead2])
        .execute(db.pool())
        .await
        .expect("mark two dead");

    let attached = attach_movie_list_media(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("attach");
    let a = attached.get(&number).expect("影片");
    assert_eq!(a.media_count, 3, "三条媒体");
    // 先确认前置条件真的建立了
    assert_eq!(
        a.media_items.iter().filter(|m| m.valid).count(),
        1,
        "前置条件：恰好一条有效"
    );
    assert!(a.can_play, "一条有效就该能播 —— 写成 all 会让这里 false");
    assert!(
        a.media_items.iter().any(|m| m.media_id == good && m.valid),
        "有效那条不能被误判成 dead"
    );
}

/// 全部判死 -> 不能播。
#[tokio::test]
async fn can_play_is_false_when_every_media_is_dead() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let number = seed_movie(&db).await;
    seed_media(&db, &number, library_id, "d1").await;
    seed_media(&db, &number, library_id, "d2").await;
    sqlx::query("UPDATE media SET valid = FALSE WHERE movie_number = $1")
        .bind(&number)
        .execute(db.pool())
        .await
        .expect("mark dead");

    let attached = attach_movie_list_media(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("attach");
    let a = attached.get(&number).expect("影片");
    assert_eq!(a.media_count, 2, "媒体数不因 valid 而变");
    assert!(!a.can_play, "全部判死 -> 不能播");
}

// ================================================================ LEFT JOIN

/// 删库会 **CASCADE** 掉它的媒体 —— 所以「孤儿媒体」在当前 DDL 下不存在。
///
/// 这条测试的作用是**钉住这个事实**，因为摘要的 `LEFT JOIN` 容易被误读成
/// 「为了处理孤儿媒体」。写它的人（包括我自己）会以为删库后媒体还在、
/// `library_*` 会变成 `NULL` —— 那样就会写出错误的文档。
///
/// 若哪天有人把 `media_library_id_fk` 放宽成 `SET NULL`，这条测试会红，
/// 那时 `LEFT JOIN` 才真正开始起作用，而 `Option` 字段也终于名副其实。
#[tokio::test]
async fn deleting_a_library_cascades_to_its_media() {
    let db = TestDb::require().await;
    let number = seed_movie(&db).await;
    let library_id = seed_library(&db).await;
    seed_media(&db, &number, library_id, "will-cascade").await;

    let before = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media WHERE movie_number = $1")
        .bind(&number)
        .fetch_one(db.pool())
        .await
        .expect("count before");
    assert_eq!(before, 1, "前置条件：一条媒体");

    sqlx::query("DELETE FROM media_library WHERE id = $1")
        .bind(library_id)
        .execute(db.pool())
        .await
        .expect("delete library");

    let after = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media WHERE movie_number = $1")
        .bind(&number)
        .fetch_one(db.pool())
        .await
        .expect("count after");
    assert_eq!(
        after, 0,
        "media_library_id_fk 是 ON DELETE CASCADE，删库会连带删掉媒体 —— \
         若这里变成 1，说明 DDL 变了，`Option` 字段与 LEFT JOIN 的取舍要重新评估"
    );

    // 媒体没了，摘要自然也没有它
    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("query");
    assert!(
        !summaries.contains_key(&number),
        "媒体被 CASCADE 之后摘要里不该还有它"
    );
}

/// 非 JAV 媒体（`video_item_id`）**不**出现在任何影片的摘要里。
#[tokio::test]
async fn a_non_jav_media_is_not_attached_to_any_movie() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let number = seed_movie(&db).await;

    // 造一个非 JAV 媒体：movie_number 为空、video_item_id 非空。
    // 需要先有 video_item 行。
    let item: (i32,) = sqlx::query_as(
        "INSERT INTO video_item (title, created_at, updated_at) VALUES ($1, now(), now()) RETURNING id",
    )
    .bind(format!("vi-{}", n()))
    .fetch_one(db.pool())
    .await
    .expect("insert video_item");

    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("nonjav-{}.mp4", n()),
            file_size_bytes: 1,
            movie_number: None,
            video_item_id: Some(item.0),
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert non-jav media");

    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("query");
    assert!(
        !summaries.contains_key(&number),
        "非 JAV 媒体不属于任何影片，不该出现在摘要里"
    );
}

/// `video_info` 是 `JsonTextField`（TEXT 里的 JSON）—— 脏文本不该让查询失败。
#[tokio::test]
async fn a_malformed_video_info_does_not_break_the_query() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let number = seed_movie(&db).await;
    seed_media(&db, &number, library_id, "dirty").await;
    // 写一段不是 JSON 的文本
    sqlx::query("UPDATE media SET video_info = $2 WHERE movie_number = $1")
        .bind(&number)
        .bind("this is not json")
        .execute(db.pool())
        .await
        .expect("dirty video_info");

    let summaries = list_movie_media_summaries(db.pool(), std::slice::from_ref(&number))
        .await
        .expect("脏 video_info 不该让查询失败");
    let items = summaries.get(&number).expect("影片");
    assert_eq!(
        items[0].video_info.as_deref(),
        Some("this is not json"),
        "video_info 原样透传，不解析 —— 解析失败会让整个列表 500"
    );
}
