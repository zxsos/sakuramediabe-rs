//! 缩略图产物落盘的端到端测试：真实数据库 + 真实图片根 + 真实 zip 字节。
//!
//! # 为什么必须端到端
//!
//! `persist` 是**三段式**的（写临时包 -> 备份后原子替换 -> 登记 DB），而它存在的
//! 主要理由就是「中间任何一步失败都不能留下不一致的状态」。只看代码看不出：
//!
//! | 断言 | 错了会怎样 |
//! |---|---|
//! | 包里的条目名 | 同一帧在包里存两份（provider 换过文件名）|
//! | 登记失败后包的样子 | 「文件是新的、记录是旧的」——**没有重试机会** |
//! | 登记本身失败后数据库的样子 | 包里有 12 条、库里 3 条，那 9 条成幽灵 |
//!
//! 上游出处：`playback/thumbnails/artifacts.persist`。

mod support;

use std::path::PathBuf;

use sm_db::repo::{MediaRepository, MediaThumbnailRepository};
use sm_db::testing::TestDb;
use sm_service::catalog::image_store::{backup_pack_path, read_pack_entry};
use sm_service::playback::thumbnails::artifacts::{ThumbnailArtifact, ThumbnailArtifactService};
use support::{n, seed_media, ImageRoot};

/// 造一件产物：`source` 是 provider 产出的文件（**在图片根之外**，那是 workspace）。
fn artifact(image_root: &ImageRoot, offset: i64) -> (ThumbnailArtifact, PathBuf) {
    let source_relative = format!("workspace/artifact-{}-{}.webp", n(), offset);
    image_root.write_loose(&source_relative, format!("bytes-{offset}").as_bytes());
    let path = image_root.root().join(&source_relative);
    (
        ThumbnailArtifact {
            thumbnail_id: 0,
            offset_seconds: offset,
            path: path.clone(),
        },
        path,
    )
}

async fn media_row(db: &TestDb, media_id: i32) -> sm_db::Media {
    MediaRepository::new(db.pool().clone())
        .find_by_id(media_id)
        .await
        .expect("查 media")
        .expect("媒体应存在")
}

async fn rows(db: &TestDb, media_id: i32) -> Vec<sm_db::playback::media::MediaThumbnail> {
    MediaThumbnailRepository::new(db.pool().clone())
        .list_all_by_media(media_id)
        .await
        .expect("列缩略图")
}

/// ★ 落盘：包里有全部条目（按 offset 升序、条目名是 `<offset>.webp`），
/// 数据库每条一行且 `origin` 指向包内的那一份。
#[tokio::test]
async fn persist_writes_the_pack_and_registers_every_row() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("THP-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    // 刻意乱序：条目顺序必须由 offset 决定，不受入参顺序影响。
    let batch = vec![artifact(&image_root, 180), artifact(&image_root, 60)];

    let count = service.persist(&media, &batch).await.expect("落盘");
    assert_eq!(count, 2);

    let pack_path = service.thumbnail_pack_file(&media).expect("包路径");
    let file = std::fs::File::open(&pack_path).expect("开包");
    let mut archive = zip::ZipArchive::new(file).expect("解析包");
    let names: Vec<String> = (0..archive.len())
        .map(|index| archive.by_index(index).expect("取条目").name().to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["60.webp".to_owned(), "180.webp".to_owned()],
        "条目名是 `<offset>.webp` 且按 offset 升序 —— 顺序不稳定会让包字节每次不同"
    );

    let rows = rows(&db, media_id).await;
    assert_eq!(
        rows.iter().map(|row| row.offset).collect::<Vec<_>>(),
        vec![60, 180]
    );
    assert_eq!(
        rows[0].image_search_index_status,
        sm_db::playback::media::image_search_index_status::PENDING,
        "JAV 缩略图要进检索索引（PENDING）"
    );

    // `origin` 必须指回包所在的那个目录 —— 它在数据库里是唯一键，写错就再也
    // 找不到这张图。
    let thumbnails = service.list_media_thumbnails(media_id).await.expect("列出");
    let directory = service.thumbnail_directory(&media).expect("目录");
    let image_root_path = image_root.root();
    let relative_dir = directory
        .strip_prefix(image_root_path)
        .expect("目录在图片根内")
        .to_string_lossy()
        .replace('\\', "/");
    for (value, offset) in thumbnails.iter().zip([60, 180]) {
        assert_eq!(
            value.image_origin,
            format!("{relative_dir}/{offset}.webp"),
            "origin 必须是**库内相对路径**（POSIX 分隔符）"
        );
    }

    // 包里的字节就是 provider 给的那一份。
    assert_eq!(
        read_pack_entry(&pack_path, "60.webp").as_deref(),
        Some(b"bytes-60".as_slice())
    );
}

/// 非 JAV 媒体：目录落在 `videos/<video_item_id>/` 下，状态位是 `SKIPPED`。
///
/// 两个断言各锁一条：`videos/` 那条命名空间分支，以及「图搜是影片维度的能力，
/// 非 JAV 缩略图不该进待处理队列」。
#[tokio::test]
async fn a_video_item_media_lands_under_videos_and_is_skipped() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let media_id = seed_media(&db, None).await;

    // 造一条 video_item 并挂到媒体上 —— `media.video_item_id` 是外键。
    let now = sm_db::common::time::now_utc();
    let (video_item_id,): (i32,) = sqlx::query_as(
        "INSERT INTO video_item (title, summary, created_at, updated_at) \
         VALUES ($1, '', $2, $2) RETURNING id",
    )
    .bind(format!("video-{}", n()))
    .bind(now)
    .fetch_one(db.pool())
    .await
    .expect("insert video_item");
    sqlx::query("UPDATE media SET video_item_id = $1 WHERE id = $2")
        .bind(video_item_id)
        .bind(media_id)
        .execute(db.pool())
        .await
        .expect("挂到媒体上");

    let media = media_row(&db, media_id).await;
    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);

    let directory = service.thumbnail_directory(&media).expect("目录");
    assert_eq!(
        directory,
        image_root
            .root()
            .join("videos")
            .join(video_item_id.to_string())
            .join("media")
            .join(media_id.to_string())
            .join("thumbnails"),
        "非 JAV 媒体走 `videos/<video_item_id>/...`"
    );

    service
        .persist(&media, &[artifact(&image_root, 0)])
        .await
        .expect("落盘");
    assert_eq!(
        rows(&db, media_id).await[0].image_search_index_status,
        sm_db::playback::media::image_search_index_status::SKIPPED,
        "非 JAV 缩略图落 SKIPPED —— 否则它会永久滞留在待处理队列里"
    );
}

/// 归属都定不下来时，落盘**在写任何文件之前**就失败，数据库不留痕。
#[tokio::test]
async fn an_unowned_media_fails_without_touching_anything() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let media_id = seed_media(&db, None).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    let error = service
        .persist(&media, &[artifact(&image_root, 0)])
        .await
        .expect_err("无归属的媒体拿不到缩略图目录");
    assert_eq!(error.code(), "thumbnail_namespace_unresolved");
    assert!(
        rows(&db, media_id).await.is_empty(),
        "失败不能留下任何登记行"
    );
}

/// ★ 空输入：返回 0 且**不触碰任何文件**。
///
/// 「顺手把旧包删掉」是错的 —— 那等于用一次空输入抹掉已有产物。
#[tokio::test]
async fn an_empty_batch_touches_nothing() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("THP-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    let pack_path = service.thumbnail_pack_file(&media).expect("包路径");
    std::fs::create_dir_all(pack_path.parent().expect("父目录")).expect("建目录");
    std::fs::write(&pack_path, b"existing pack").expect("造一个旧包");

    assert_eq!(service.persist(&media, &[]).await.expect("空输入"), 0);
    assert_eq!(
        std::fs::read(&pack_path).expect("旧包应当原样还在"),
        b"existing pack"
    );
}

/// ★ 登记失败 → **包退回旧的那一份**，数据库也没留下半批登记。
///
/// 怎么造出「登记失败」：偏移在 `ThumbnailArtifact` 里是 `i64` 而列是 `i32`，
/// 收窄失败会在**包已替换之后**报错。用 `i64::MAX` 就精确命中那个时刻。
///
/// 两条断言各有分量：
///
/// - 包必须与**第一次**落盘的结果一致（不是第二次的半成品）——
///   否则「文件是新的、记录是旧的」，而那个状态没有重试机会；
/// - 数据库里**不能有** `180` 那一条：它在失败的批次里先被插入、随后整批回滚。
///   没有事务的话它会留下 —— 而包已经退回旧版，于是它成了指向不存在条目的幽灵。
#[tokio::test]
async fn a_failed_registration_rolls_the_pack_back_and_atomic_rolls_the_rows() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("THP-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    let pack_path = service.thumbnail_pack_file(&media).expect("包路径");

    // 第一次：成功落两件。
    let first = vec![artifact(&image_root, 60), artifact(&image_root, 120)];
    service.persist(&media, &first).await.expect("第一次落盘");
    let settled = std::fs::read(&pack_path).expect("读第一次的包");

    // 第二次：一个正常偏移 + 一个越界偏移（排在最后，所以正常那个**先被插入**）。
    let second = vec![artifact(&image_root, 180), artifact(&image_root, i64::MAX)];
    let error = service
        .persist(&media, &second)
        .await
        .expect_err("越界偏移必须让整批失败");
    assert_eq!(error.code(), "thumbnail_offset_invalid");

    assert_eq!(
        std::fs::read(&pack_path).expect("读回滚后的包"),
        settled,
        "包必须退回旧的那一份（新旧内容不一致 = 包与记录已经对不上）"
    );
    assert!(
        !backup_pack_path(&pack_path).exists(),
        "回滚成功后备份要消失，否则它会一直躺在目录里"
    );

    let rows = rows(&db, media_id).await;
    assert_eq!(
        rows.iter().map(|row| row.offset).collect::<Vec<_>>(),
        vec![60, 120],
        "失败批次里先插入的那条也必须回滚（整批一个事务）"
    );
}

/// 同一批偏移**再来一次**是幂等的：行数不翻倍，包被覆盖成新字节。
///
/// 上游用的是裸 `create`，重新生成会撞唯一索引；这里两个仓储都用 upsert ——
/// 表的约束本来就是这么设计的。
#[tokio::test]
async fn persisting_the_same_offsets_again_is_idempotent() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("THP-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    let pack_path = service.thumbnail_pack_file(&media).expect("包路径");

    let first = vec![artifact(&image_root, 30)];
    service.persist(&media, &first).await.expect("第一次");
    let created_at = rows(&db, media_id).await[0].created_at;

    let again = vec![artifact(&image_root, 30)];
    assert_eq!(service.persist(&media, &again).await.expect("第二次"), 1);

    let rows = rows(&db, media_id).await;
    assert_eq!(rows.len(), 1, "同一偏移不产生第二行");
    assert_eq!(
        rows[0].created_at, created_at,
        "重试不该改写 created_at（那是「首次识别出这个时刻点」的时间）"
    );
    assert_eq!(
        read_pack_entry(&pack_path, "30.webp").as_deref(),
        Some(std::fs::read(&again[0].1).expect("第二次的产物").as_slice()),
        "包要被覆盖成新的字节"
    );
}
