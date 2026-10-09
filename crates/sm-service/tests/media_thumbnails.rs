//! 缩略图目录布局与列表的端到端测试：真实数据库 + 真实临时图片根。
//!
//! # 为什么必须端到端
//!
//! 这一批有两个「只看代码看不出来」的东西：
//!
//! | 判定 | 错了会怎样 |
//! |---|---|
//! | 缩略图目录落在哪 | 写进一个**没人找得到**的目录（不再有报错，只是图没了）|
//! | 读字节时包优先 | 读到**过期**的那一份（loose 是旧图、包是新的，反之亦然）|
//!
//! 目录布局还直接决定「删一部影片」能不能是一次 `rm -rf`：散在全局
//! `thumbnails/<media_id>/` 就得全表扫描才知道该删哪些。
//!
//! 上游出处：`playback/thumbnails/artifacts.py`。

mod support;

use sm_db::repo::MediaRepository;
use sm_db::testing::TestDb;
use sm_service::catalog::image_store::{read_image_bytes, write_pack};
use sm_service::catalog::media_paths::{self, image_pack_relative_path};
use sm_service::playback::thumbnails::artifacts::ThumbnailArtifactService;
use support::{n, seed_image, seed_media, seed_thumbnail, ImageRoot};

/// 取媒体行 —— 目录布局要靠它算。
async fn media_row(db: &TestDb, media_id: i32) -> sm_db::Media {
    MediaRepository::new(db.pool().clone())
        .find_by_id(media_id)
        .await
        .expect("查 media")
        .expect("媒体应存在")
}

/// ★ 目录随媒体归属走，**不是**扁平的 `<root>/thumbnails/<media_id>/`。
#[tokio::test]
async fn the_thumbnail_directory_follows_the_movie_namespace() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("MTH-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;
    let media = media_row(&db, media_id).await;

    let service = ThumbnailArtifactService::new(db.pool(), &image_root.config);
    let directory = service.thumbnail_directory(&media).expect("目录");
    let pack = service.thumbnail_pack_file(&media).expect("包路径");

    let shard =
        media_paths::movie_asset_shard(&media_paths::normalize_asset_dir_name(&movie_number));
    let expected_dir = image_root
        .root()
        .join("movies")
        .join(&shard)
        .join(&movie_number)
        .join("media")
        .join(media_id.to_string())
        .join("thumbnails");
    assert_eq!(
        directory, expected_dir,
        "JAV 缩略图要落在影片自己的目录树下（`movies/<分片>/<番号>/media/<id>/thumbnails`）"
    );
    assert_eq!(
        pack,
        expected_dir.with_file_name("thumbnails.zip"),
        "包与目录**同级同名** —— `image_pack_relative_path` 靠这条反推包路径，改名会让它失效"
    );
}

/// 两者都没有（既无番号也无视频条目）→ **报错**，不写进幻影目录。
///
/// 上游这里会静默产出 `videos/None/`：缩略图写进去再也没有人找得到。
#[tokio::test]
async fn a_media_without_any_owner_is_refused() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let media_id = seed_media(&db, None).await;
    let media = media_row(&db, media_id).await;

    let error = ThumbnailArtifactService::new(db.pool(), &image_root.config)
        .thumbnail_directory(&media)
        .expect_err("两者都没有时不该给出目录");
    assert_eq!(error.code(), "thumbnail_namespace_unresolved");
}

/// ★ 列表按 `(offset, id)` 升序，且 `width`/`height` 来自第一条。
#[tokio::test]
async fn thumbnails_are_listed_by_offset_then_id() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("MTH-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;

    // 刻意乱序插入：若实现按 id 排，这里就会露出来。
    for offset in [300_i32, 60, 180] {
        let origin = format!("movies/ab/{movie_number}/media/{media_id}/thumbnails/{offset}.webp");
        let image_id = seed_image(&db, &origin).await;
        seed_thumbnail(&db, media_id, image_id, offset).await;
    }

    let values = ThumbnailArtifactService::new(db.pool(), &image_root.config)
        .list_media_thumbnails(media_id)
        .await
        .expect("列出缩略图");

    assert_eq!(
        values
            .iter()
            .map(|value| value.offset_seconds)
            .collect::<Vec<_>>(),
        vec![60, 180, 300],
        "按 offset 升序 —— 选图逻辑（取中位数）依赖位置语义"
    );
    assert!(
        values.iter().all(|value| value.media_id == media_id),
        "每条都要带 media_id"
    );
    assert!(
        values
            .iter()
            .all(|value| value.image_origin.ends_with(".webp")),
        "带的是**未签名**的相对路径（签名在 API 层）"
    );
}

/// 字节解不出来时**列表明照样返回**，只是 `width`/`height` 为空。
///
/// 尺寸是锦上添花（前端拿它做占位比例），拿不到不该让整个列表 500 ——
/// 上游同款：只记一条 warn。
///
/// > 正向路径（真的解出宽高）由 `svc-image` 的 `decode` 测试覆盖；这里锁的是
/// > **解不出来时的降级行为**，以及「列表本身不受影响」。
#[tokio::test]
async fn undecodable_bytes_only_null_the_dimensions() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("MTH-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;

    let origin = format!("movies/ab/{movie_number}/media/{media_id}/thumbnails/0.webp");
    let image_id = seed_image(&db, &origin).await;
    // 扩展名是 `.webp` 但内容不是 —— provider 撒谎的典型形状。
    image_root.write_loose(&origin, b"definitely not a webp");
    seed_thumbnail(&db, media_id, image_id, 0).await;

    let values = ThumbnailArtifactService::new(db.pool(), &image_root.config)
        .list_media_thumbnails(media_id)
        .await
        .expect("解不出尺寸不该让列表失败");
    assert_eq!(values.len(), 1);
    assert!(values[0].width.is_none(), "解不出来时留空，不编一个值");
    assert!(values[0].height.is_none());
}

/// ★ 读字节时**包优先**：包在就用包里的那一份。
///
/// 打包后 loose 文件会被清掉，所以「包是权威」是常态；但两者同时存在时
/// （重新生成刚写完 loose、包还没重建）顺序就决定了读到新的还是旧的。
#[tokio::test]
async fn pack_bytes_win_over_the_loose_file() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let movie_number = format!("MTH-{:06}", n());
    let media_id = seed_media(&db, Some(&movie_number)).await;

    let origin = format!("movies/ab/{movie_number}/media/{media_id}/thumbnails/120.webp");
    image_root.write_loose(&origin, b"loose-and-stale");

    let pack_relative = image_pack_relative_path(&origin).expect("属于某个包");
    let pack_path = image_root.root().join(&pack_relative);
    write_pack(
        &pack_path,
        &[("120.webp".to_owned(), b"pack-and-fresh".to_vec())],
    )
    .expect("写包");

    let bytes = read_image_bytes(image_root.root(), &origin).expect("读字节");
    assert_eq!(
        bytes, b"pack-and-fresh",
        "包存在时以包为准（打包后 loose 会被清掉，包才是权威）"
    );

    // 包没了则回退单文件 —— 否则「包被删过一次」就等于图全丢。
    std::fs::remove_file(&pack_path).expect("删包");
    assert_eq!(
        read_image_bytes(image_root.root(), &origin).expect("回退读单文件"),
        b"loose-and-stale"
    );
}
