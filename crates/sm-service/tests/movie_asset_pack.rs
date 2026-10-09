//! 影片图片包的**端到端**测试：真实数据库 + 真实临时图片根 + 真实 zip 字节。
//!
//! # 为什么必须端到端
//!
//! 这一批的行为全都在「数据库活跃集」与「磁盘字节」之间：
//!
//! | 判定 | 只看代码看不出来 |
//! |---|---|
//! | 包里有哪几条 | 取决于 `live_origins`（LIKE 粗筛 + Rust 侧精筛）|
//! | 二次重建的字节从哪来 | loose 文件已被清掉，只能来自**旧包** |
//! | 嵌套目录会不会混进包 | `media/<id>/thumbnails/` 属于另一个包 |
//!
//! 这三条错任何一条都不会报错，只会「包里少几条 / 多几条」。
//!
//! 上游出处：`catalog/movie_asset_pack_service.py`。

use std::path::Path;

mod support;

use sm_db::testing::TestDb;
use sm_service::catalog::image_cleanup::ImageCleanupService;
use sm_service::catalog::movie_asset_pack::{pack_entry_name, MovieAssetPackService};
use support::{pack_entries, seed_image, ImageRoot};

async fn delete_image_row(db: &TestDb, image_id: i32) {
    sqlx::query("DELETE FROM image WHERE id = $1")
        .bind(image_id)
        .execute(db.pool())
        .await
        .expect("删 image 行");
}

const MOVIE_DIR: &str = "movies/ab/ABC-001";

/// ★ 重建：包里有活跃集的全部条目，且**清掉了已入包的 loose 文件**。
#[tokio::test]
async fn a_rebuild_packs_the_live_set_and_clears_the_loose_files() {
    let db = TestDb::require().await;
    let fixture = ImageRoot::new();
    let service = MovieAssetPackService::new(db.pool(), &fixture.config);

    for name in ["cover.jpg", "thin.jpg", "1.jpg"] {
        let origin = format!("{MOVIE_DIR}/{name}");
        seed_image(&db, &origin).await;
        fixture.write_loose(&origin, format!("bytes-of-{name}").as_bytes());
    }

    assert!(
        service
            .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
            .await
            .expect("重建"),
        "有图片时应当建成包（Ok(true)）"
    );

    let pack_path = service
        .movie_asset_pack_path(Path::new(MOVIE_DIR))
        .expect("包路径");
    assert_eq!(
        pack_entries(&pack_path),
        vec!["1.jpg", "cover.jpg", "thin.jpg"],
        "包内条目名就是文件名，且按 origin 升序"
    );

    for name in ["cover.jpg", "thin.jpg", "1.jpg"] {
        assert!(
            !fixture.loose_exists(&format!("{MOVIE_DIR}/{name}")),
            "{name} 已入包，loose 文件该被清掉 —— 否则同一张图在磁盘上存两份"
        );
    }
    assert!(
        pack_path.is_file(),
        "清理 loose 文件时必须放过包本身（否则刚建的包会被自己删掉）"
    );
}

/// ★ 二次重建的字节来自**旧包**（loose 文件已经没了）。
///
/// 这一条锁的是 `load_entries` 的兜底分支。没有它，第二次重建会走到
/// 「有活跃行但拿不到字节」→ 三次重试 → 返回 `Ok(pack_path.is_file())` = true
/// ——**看起来成功，但包一个条目都没更新**。
#[tokio::test]
async fn a_second_rebuild_takes_the_bytes_from_the_old_pack() {
    let db = TestDb::require().await;
    let fixture = ImageRoot::new();
    let service = MovieAssetPackService::new(db.pool(), &fixture.config);

    let origin = format!("{MOVIE_DIR}/cover.jpg");
    seed_image(&db, &origin).await;
    fixture.write_loose(&origin, b"the-cover-bytes");

    assert!(service
        .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
        .await
        .expect("第一次重建"));
    let pack_path = service
        .movie_asset_pack_path(Path::new(MOVIE_DIR))
        .expect("包路径");
    assert!(!fixture.loose_exists(&origin), "loose 已清");

    // 第二次：loose 不在，只能从旧包里取。
    assert!(service
        .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
        .await
        .expect("第二次重建"));
    assert_eq!(pack_entries(&pack_path), vec!["cover.jpg".to_owned()]);

    let bytes = sm_service::catalog::image_store::read_pack_entry(&pack_path, "cover.jpg")
        .expect("条目应按原样保留");
    assert_eq!(
        bytes, b"the-cover-bytes",
        "字节必须原样来自旧包 —— 丢了内容就是「包在但图坏了」"
    );
}

/// ★ 子目录里的图片**不进**影片资产包。
///
/// `movies/<shard>/<番号>/media/<id>/thumbnails/1.jpg` 属于 `thumbnails.zip`
/// （另一个包）。混进 `assets.zip` 会让同一张图存在于两个包，删其一另一份就成
/// 幽灵 —— 而不会有任何报错。
#[tokio::test]
async fn nested_thumbnails_do_not_leak_into_the_movie_pack() {
    let db = TestDb::require().await;
    let fixture = ImageRoot::new();
    let service = MovieAssetPackService::new(db.pool(), &fixture.config);

    let direct = format!("{MOVIE_DIR}/cover.jpg");
    let nested = format!("{MOVIE_DIR}/media/12/thumbnails/1.jpg");
    seed_image(&db, &direct).await;
    seed_image(&db, &nested).await;
    fixture.write_loose(&direct, b"cover");
    fixture.write_loose(&nested, b"thumb");

    let live = service
        .live_origins(Path::new(MOVIE_DIR))
        .await
        .expect("活跃集");
    assert_eq!(
        live,
        vec![direct.clone()],
        "只取直接子文件；嵌套的缩略图归 thumbnails.zip"
    );

    assert!(service
        .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
        .await
        .expect("重建"));
    let pack_path = service
        .movie_asset_pack_path(Path::new(MOVIE_DIR))
        .expect("包路径");
    assert_eq!(pack_entries(&pack_path), vec!["cover.jpg".to_owned()]);
    assert!(
        fixture.loose_exists(&nested),
        "另一个包的 loose 文件不该被清理 —— remove_loose_files 不许进子目录"
    );
}

/// 活跃集为空 → `Ok(false)` **并且删掉已有的包**。
///
/// 「没有图片」不是错误；但包的存在意味着「这里有全部图片」，留着空包会让分发方
/// 以为已经打包好了。
#[tokio::test]
async fn an_empty_live_set_reports_false_and_removes_the_pack() {
    let db = TestDb::require().await;
    let fixture = ImageRoot::new();
    let service = MovieAssetPackService::new(db.pool(), &fixture.config);

    let origin = format!("{MOVIE_DIR}/cover.jpg");
    let image_id = seed_image(&db, &origin).await;
    fixture.write_loose(&origin, b"cover");
    assert!(service
        .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
        .await
        .expect("重建"));
    let pack_path = service
        .movie_asset_pack_path(Path::new(MOVIE_DIR))
        .expect("包路径");
    assert!(pack_path.is_file());

    delete_image_row(&db, image_id).await;
    assert!(
        !service
            .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
            .await
            .expect("重建"),
        "没有活跃图片时必须是 Ok(false)"
    );
    assert!(!pack_path.is_file(), "空活跃集要删掉包，别留个空壳");
}

/// ★ `image_cleanup` 删掉一条图片后，包被**按活跃集重建**（那条自然消失）。
///
/// 这一条把两个服务串起来：`delete_image_record_if_unused` 只删 DB 行，
/// 「包里的条目也跟着少一条」是 `delete_obsolete_image_files` 触发的重建做的。
#[tokio::test]
async fn cleanup_rebuilds_the_pack_so_the_deleted_entry_disappears() {
    let db = TestDb::require().await;
    let fixture = ImageRoot::new();
    let packs = MovieAssetPackService::new(db.pool(), &fixture.config);

    let keep = format!("{MOVIE_DIR}/cover.jpg");
    let drop_origin = format!("{MOVIE_DIR}/1.jpg");
    let keep_id = seed_image(&db, &keep).await;
    let drop_id = seed_image(&db, &drop_origin).await;
    fixture.write_loose(&keep, b"keep");
    fixture.write_loose(&drop_origin, b"drop");
    let _ = keep_id;

    assert!(packs
        .rebuild_movie_asset_pack(Path::new(MOVIE_DIR))
        .await
        .expect("重建"));
    let pack_path = packs
        .movie_asset_pack_path(Path::new(MOVIE_DIR))
        .expect("包路径");
    assert_eq!(pack_entries(&pack_path), vec!["1.jpg", "cover.jpg"]);

    let cleanup = ImageCleanupService::new(db.pool(), &fixture.config);
    let obsolete = cleanup
        .delete_image_record_if_unused(Some(drop_id))
        .await
        .expect("删记录");
    assert_eq!(obsolete, vec![drop_origin.clone()], "回传 origin 供删文件");
    cleanup
        .delete_obsolete_image_files(&obsolete)
        .await
        .expect("删文件");

    assert_eq!(
        pack_entries(&pack_path),
        vec!["cover.jpg".to_owned()],
        "包已按活跃集重建 —— 被删的条目必须消失，且保留的那个字节不能坏"
    );
    assert_eq!(
        sm_service::catalog::image_store::read_pack_entry(&pack_path, "cover.jpg").as_deref(),
        Some(b"keep".as_slice())
    );
}

/// 条目名推导是纯函数，顺手钉一下（包内条目名 = 去掉前导斜杠的相对路径）。
#[test]
fn entry_names_are_relative_paths() {
    assert_eq!(pack_entry_name(MOVIE_DIR), MOVIE_DIR);
    assert_eq!(pack_entry_name("/a/b.jpg"), "a/b.jpg");
}
