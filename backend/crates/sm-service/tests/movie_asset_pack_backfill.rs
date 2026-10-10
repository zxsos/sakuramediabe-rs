//! 存量影片图片打包回填的**端到端**测试：真库 + 真临时图片根 + 真 zip 字节。
//!
//! # 为什么必须端到端
//!
//! 上游这个服务的行为由**两处状态共同决定**，而它们只有合起来才看得懂：
//!
//! | 判定 | 来自 |
//! |---|---|
//! | 哪些影片进候选 | **库**（封面 / 薄封面 / 剧照三处，取并集去重）|
//! | 这一部怎么处理 | **磁盘**（包在不在、有没有残留散文件、活跃图片文件在不在）|
//!
//! 光看代码看不出「已有包但有散文件」与「已有包且干净」的分叉，也看不出
//! 「缺文件」为什么是**跳过**而不是失败。
//!
//! 上游出处：`catalog/movie_asset_pack_backfill_service.py`。

use std::path::PathBuf;

use sm_db::repo::{MoviePlotImageRepository, MovieRepository};
use sm_db::testing::TestDb;
use sm_service::catalog::media_paths;
use sm_service::catalog::movie_asset_pack::MovieAssetPackService;
use sm_service::catalog::movie_asset_pack_backfill::MovieAssetPackBackfillService;
use support::{seed_image, ImageRoot};

mod support;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 某番号的资产目录（`movies/<shard>/<番号>`）。
fn movie_dir(number: &str) -> PathBuf {
    media_paths::movie_asset_relative_dir(&media_paths::normalize_asset_dir_name(number))
}

/// 该目录下一个文件的 **origin**（库里的 origin 是 POSIX 相对路径）。
fn origin_of(number: &str, name: &str) -> String {
    format!(
        "{}/{}",
        movie_dir(number).to_string_lossy().replace('\\', "/"),
        name
    )
}

fn service(db: &TestDb, root: &ImageRoot) -> MovieAssetPackBackfillService {
    MovieAssetPackBackfillService::new(db.pool(), &root.config)
}

fn packs(db: &TestDb, root: &ImageRoot) -> MovieAssetPackService {
    MovieAssetPackService::new(db.pool(), &root.config)
}

async fn movie_id(db: &TestDb, number: &str) -> i32 {
    MovieRepository::new(db.pool().clone())
        .find_by_number(number)
        .await
        .expect("查影片")
        .expect("影片存在")
        .id
}

/// 建一部带封面的影片：先落影片行，再把 `cover_image_id` 指向那张图。
///
/// 走裸 SQL 而不是 `NewMovie` 字面量：这里只需要「那一列非空」，
/// 而 `NewMovie` 有二十来个字段（与 `support::seed_media` 同一个理由）。
async fn seed_movie_with_cover(db: &TestDb, number: &str, image_id: i32) {
    support::seed_movie_if_missing(db, number).await;
    sqlx::query("UPDATE movie SET cover_image_id = $1 WHERE movie_number = $2")
        .bind(i64::from(image_id))
        .bind(number)
        .execute(db.pool())
        .await
        .expect("挂封面");
}

async fn seed_movie_with_thin_cover(db: &TestDb, number: &str, image_id: i32) {
    support::seed_movie_if_missing(db, number).await;
    sqlx::query("UPDATE movie SET thin_cover_image_id = $1 WHERE movie_number = $2")
        .bind(i64::from(image_id))
        .bind(number)
        .execute(db.pool())
        .await
        .expect("挂薄封面");
}

async fn seed_movie_with_plot_image(db: &TestDb, number: &str, image_id: i32) {
    support::seed_movie_if_missing(db, number).await;
    MoviePlotImageRepository::new(db.pool().clone())
        .link(movie_id(db, number).await, image_id)
        .await
        .expect("挂剧照");
}

// ================================================================ 候选

/// 候选是**三处的并集**且去重、按番号升序。
///
/// 只查封面会漏掉「只有剧照」的影片（那正是最需要回填的存量），
/// 不去重会让同一部影片被处理两次（第二次变成「已打包」的噪声计数）。
#[tokio::test]
async fn the_candidates_union_the_three_image_sources() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();

    let cover_only = format!("BFC-{:03}", n());
    let thin_only = format!("BFC-{:03}", n());
    let plot_only = format!("BFC-{:03}", n());
    let both = format!("BFC-{:03}", n());
    let none = format!("BFC-{:03}", n());

    let cover_id = seed_image(&db, &origin_of(&cover_only, "cover.jpg")).await;
    seed_movie_with_cover(&db, &cover_only, cover_id).await;

    let thin_id = seed_image(&db, &origin_of(&thin_only, "thin.jpg")).await;
    seed_movie_with_thin_cover(&db, &thin_only, thin_id).await;

    let plot_id = seed_image(&db, &origin_of(&plot_only, "1.jpg")).await;
    seed_movie_with_plot_image(&db, &plot_only, plot_id).await;

    // 封面 + 剧照：两个来源里的同一部影片，只能出现一次。
    let both_cover = seed_image(&db, &origin_of(&both, "cover.jpg")).await;
    let both_plot = seed_image(&db, &origin_of(&both, "1.jpg")).await;
    seed_movie_with_cover(&db, &both, both_cover).await;
    MoviePlotImageRepository::new(db.pool().clone())
        .link(movie_id(&db, &both).await, both_plot)
        .await
        .expect("挂剧照");

    // 一张图都没有的影片：不进候选。
    support::seed_movie_if_missing(&db, &none).await;

    let mut expected = vec![cover_only, thin_only, plot_only, both];
    expected.sort();

    assert_eq!(
        service(&db, &root).candidates().await.expect("候选"),
        expected,
        "三处并集 + 去重 + 按番号升序；无图的影片不进候选"
    );
}

// ================================================================ 逐部处理

/// 有图、有散文件、没有包 → **建包**（`packed_movies`）。
#[tokio::test]
async fn a_movie_with_loose_files_gets_a_pack() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let number = format!("BFP-{:03}", n());

    let origin = origin_of(&number, "cover.jpg");
    let image_id = seed_image(&db, &origin).await;
    seed_movie_with_cover(&db, &number, image_id).await;
    root.write_loose(&origin, b"cover-bytes");

    let stats = service(&db, &root).backfill(None).await.expect("回填");
    assert_eq!(stats.candidate_movies, 1);
    assert_eq!(stats.packed_movies, 1, "新建了包");
    assert_eq!(stats.cleaned_movies, 0);
    assert_eq!(stats.skipped_missing_files, 0);
    assert_eq!(stats.failed_movies, 0);

    let pack_path = packs(&db, &root)
        .movie_asset_pack_path(&movie_dir(&number))
        .expect("包路径");
    assert!(pack_path.is_file(), "回填要真的落出 assets.zip");
    assert!(
        !root.loose_exists(&origin),
        "散文件已入包，该被清掉（否则同一张图在磁盘上存两份）"
    );
}

/// 库里记着图片、磁盘上却**没有文件** → **跳过**（不是失败），且不建包。
///
/// 那部影片的记录本来就该被 `image_cleanup` 清理；报错会让这个任务永远跑不完。
#[tokio::test]
async fn a_movie_whose_files_are_missing_is_skipped_not_failed() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let number = format!("BFM-{:03}", n());

    // 只登记图片行，**不写文件**。
    let origin = origin_of(&number, "cover.jpg");
    let image_id = seed_image(&db, &origin).await;
    seed_movie_with_cover(&db, &number, image_id).await;

    let stats = service(&db, &root).backfill(None).await.expect("回填");
    assert_eq!(stats.candidate_movies, 1);
    assert_eq!(
        stats.skipped_missing_files, 1,
        "缺文件是「整条跳过」，与重建失败分开计数"
    );
    assert_eq!(stats.failed_movies, 0, "跳过不是失败");
    assert_eq!(stats.packed_movies, 0);

    let pack_path = packs(&db, &root)
        .movie_asset_pack_path(&movie_dir(&number))
        .expect("包路径");
    assert!(!pack_path.is_file(), "拿不到字节时不许留半成品包");
}

/// 已有包 + 又出现散文件 → **重建并清理**（`cleaned_movies`）。
///
/// 这一档与「新建包」分开计数：它说明的是「包落后了」而不是「从没打过包」。
#[tokio::test]
async fn an_existing_pack_with_new_loose_files_is_cleaned() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let number = format!("BFC-{:03}", n());

    let first = origin_of(&number, "cover.jpg");
    let first_id = seed_image(&db, &first).await;
    seed_movie_with_cover(&db, &number, first_id).await;
    root.write_loose(&first, b"cover-bytes");

    // 先建出包（这一步之后散文件已被清）。
    let stats = service(&db, &root)
        .backfill(None)
        .await
        .expect("第一次回填");
    assert_eq!(stats.packed_movies, 1);

    // 再冒出一个新散文件（例如某次图片重生成留下的）。
    let extra = origin_of(&number, "1.jpg");
    seed_image(&db, &extra).await;
    root.write_loose(&extra, b"plot-bytes");

    let stats = service(&db, &root)
        .backfill(None)
        .await
        .expect("第二次回填");
    assert_eq!(stats.cleaned_movies, 1, "已有包 + 有散文件 → 清理档");
    assert_eq!(stats.packed_movies, 0, "不是在新建包");
    assert_eq!(stats.already_packed_movies, 0, "有散文件就不能算「已打包」");
    assert!(!root.loose_exists(&extra), "新散文件已被清掉");

    let pack_path = packs(&db, &root)
        .movie_asset_pack_path(&movie_dir(&number))
        .expect("包路径");
    let entries = support::pack_entries(&pack_path);
    assert_eq!(
        entries,
        vec!["1.jpg".to_owned(), "cover.jpg".to_owned()],
        "重建后包里是**当前活跃集**（新图进去了）"
    );
}

/// 已有包且目录干净 → **不碰它**（`already_packed_movies`）。
///
/// 上游在这条上刻意**不回读包做逐条校验** —— 大库上那是重复开销。
#[tokio::test]
async fn a_clean_packed_movie_is_left_alone() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let number = format!("BFA-{:03}", n());

    let origin = origin_of(&number, "cover.jpg");
    let image_id = seed_image(&db, &origin).await;
    seed_movie_with_cover(&db, &number, image_id).await;
    root.write_loose(&origin, b"cover-bytes");

    let pack_path = packs(&db, &root)
        .movie_asset_pack_path(&movie_dir(&number))
        .expect("包路径");
    assert!(packs(&db, &root)
        .rebuild_movie_asset_pack(&movie_dir(&number))
        .await
        .expect("预置包"));
    let before = std::fs::metadata(&pack_path)
        .expect("包存在")
        .modified()
        .expect("有 mtime");

    let stats = service(&db, &root).backfill(None).await.expect("回填");
    assert_eq!(stats.already_packed_movies, 1);
    assert_eq!(stats.cleaned_movies, 0);
    assert_eq!(stats.packed_movies, 0);

    let after = std::fs::metadata(&pack_path)
        .expect("包还在")
        .modified()
        .expect("有 mtime");
    assert_eq!(before, after, "干净的包不该被重写");
}
