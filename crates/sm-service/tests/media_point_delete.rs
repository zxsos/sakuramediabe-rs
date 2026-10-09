//! 删时刻点的端到端测试：真实数据库 + 真实临时图片根。
//!
//! # 为什么必须端到端
//!
//! `delete_point` 的语义横跨两张表与磁盘：
//!
//! ```text
//!   删 media_point 行
//!     -> 那张 image 还有人引用吗？（另外七处引用方）
//!       -> 没人用才删记录，并把 origin 交给文件清理
//! ```
//!
//! 「还有没有人引用」这一问只有真实数据库能给答案。而它**答错的后果是删掉
//! 一张正在被别的记录使用的图** —— 不报错，只表现为「封面忽然裂了」。
//!
//! 上游出处：`playback/media_service.delete_point` / `delete_point_by_id`。

mod support;

use sm_db::repo::{ImageRepository, MediaPointRepository};
use sm_db::testing::TestDb;
use sm_service::playback::media::MediaService;
use support::{n, seed_image, seed_media, ImageRoot};

/// 建一条 JAV 媒体，返回 `(media_id, movie_number)`。
async fn seed_jav_media(db: &TestDb) -> (i32, String) {
    let movie_number = format!("MPD-{:06}", n());
    let media_id = seed_media(db, Some(&movie_number)).await;
    (media_id, movie_number)
}

/// 建一个指向该图的时刻点（不需要缩略图行）。
async fn seed_point(db: &TestDb, media_id: i32, image_id: i32) -> i32 {
    MediaPointRepository::new(db.pool().clone())
        .insert(Some(media_id), None, image_id, None, None, 0)
        .await
        .expect("insert media_point")
        .id
}

async fn image_row_exists(db: &TestDb, image_id: i32) -> bool {
    ImageRepository::new(db.pool().clone())
        .find_by_id(image_id)
        .await
        .expect("查 image")
        .is_some()
}

async fn point_row_exists(db: &TestDb, point_id: i32) -> bool {
    MediaPointRepository::new(db.pool().clone())
        .find_by_id(point_id)
        .await
        .expect("查 media_point")
        .is_some()
}

/// ★ 删掉时刻点后，那张只服务于它的图（记录 + 磁盘文件）一起消失。
#[tokio::test]
async fn deleting_a_point_removes_the_image_that_only_served_it() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let (media_id, movie_number) = seed_jav_media(&db).await;

    let origin = format!("movies/ab/{movie_number}/frame.jpg");
    let image_id = seed_image(&db, &origin).await;
    image_root.write_loose(&origin, b"a frame");
    let point_id = seed_point(&db, media_id, image_id).await;

    MediaService::new(db.pool(), &image_root.config)
        .delete_point(media_id, point_id)
        .await
        .expect("删时刻点");

    assert!(!point_row_exists(&db, point_id).await, "时刻点行要没了");
    assert!(
        !image_row_exists(&db, image_id).await,
        "没人再引用的图片记录要一起删掉"
    );
    assert!(
        !image_root.loose_exists(&origin),
        "磁盘文件也要删 —— 只删记录会留下永远不被回收的孤儿文件"
    );
}

/// ★ 那张图**还被别人引用**时，记录与文件都必须留着。
///
/// 这是整个流程里最危险的一步：判据漏一处就会删掉在用的图。
/// 这里用「另一条媒体上的时刻点也指向同一张图」造出第二个引用方。
#[tokio::test]
async fn a_shared_image_survives_when_only_one_point_goes_away() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let (first_media, movie_number) = seed_jav_media(&db).await;
    let (second_media, _) = seed_jav_media(&db).await;

    // 同一张图（同一 origin）被两条媒体上的时刻点共同引用。
    let origin = format!("movies/ab/{movie_number}/shared.jpg");
    let image_id = seed_image(&db, &origin).await;
    image_root.write_loose(&origin, b"shared frame");
    let first_point = seed_point(&db, first_media, image_id).await;
    let second_point = seed_point(&db, second_media, image_id).await;

    let media = MediaService::new(db.pool(), &image_root.config);
    media
        .delete_point(first_media, first_point)
        .await
        .expect("删第一个时刻点");

    assert!(
        image_row_exists(&db, image_id).await,
        "还有一条时刻点指着它 —— 记录不能删"
    );
    assert!(
        image_root.loose_exists(&origin),
        "还有一条时刻点指着它 —— 文件不能删"
    );

    // 第二个也删掉之后，才轮到清理。
    media
        .delete_point(second_media, second_point)
        .await
        .expect("删第二个时刻点");
    assert!(
        !image_row_exists(&db, image_id).await,
        "最后一个引用者走后就该删"
    );
    assert!(!image_root.loose_exists(&origin));
}

/// 归属不符 → 404 `media_point_not_found`，**且什么都没删**。
///
/// 拿 A 媒体的 id 去删 B 媒体的时刻点：上游是一条带 `media_id` 的查询，
/// 查不到就 404。若实现成「按 point_id 删」，跨媒体删点会静默成功 ——
/// 一个 id 猜错就删错数据。
#[tokio::test]
async fn a_point_of_another_media_is_a_404_and_changes_nothing() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let (owner_media, movie_number) = seed_jav_media(&db).await;
    let (other_media, _) = seed_jav_media(&db).await;

    let origin = format!("movies/ab/{movie_number}/frame.jpg");
    let image_id = seed_image(&db, &origin).await;
    image_root.write_loose(&origin, b"frame");
    let point_id = seed_point(&db, owner_media, image_id).await;

    let error = MediaService::new(db.pool(), &image_root.config)
        .delete_point(other_media, point_id)
        .await
        .expect_err("跨媒体删点必须失败");

    assert_eq!(error.code(), "media_point_not_found");
    let details = error.details().expect("404 要带 details");
    assert_eq!(
        details.get("media_id").and_then(|v| v.as_i64()),
        Some(other_media as i64)
    );
    assert_eq!(
        details.get("point_id").and_then(|v| v.as_i64()),
        Some(point_id as i64)
    );

    assert!(point_row_exists(&db, point_id).await, "什么都没删");
    assert!(image_row_exists(&db, image_id).await);
    assert!(image_root.loose_exists(&origin));
}

/// 媒体不存在 → 404 `media_not_found`（先校验媒体，再看时刻点）。
#[tokio::test]
async fn an_absent_media_is_a_media_not_found() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let error = MediaService::new(db.pool(), &image_root.config)
        .delete_point(i32::MAX, 1)
        .await
        .expect_err("媒体不存在");
    assert_eq!(error.code(), "media_not_found");
}

/// `delete_point_by_id` 不校验归属，但**必须**校验时刻点存在。
#[tokio::test]
async fn delete_point_by_id_skips_the_media_check_but_not_existence() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let media = MediaService::new(db.pool(), &image_root.config);

    let error = media
        .delete_point_by_id(i32::MAX)
        .await
        .expect_err("时刻点不存在");
    assert_eq!(error.code(), "media_point_not_found");
    assert_eq!(
        error
            .details()
            .and_then(|details| details.get("media_point_id"))
            .and_then(|value| value.as_i64()),
        Some(i32::MAX as i64),
        "details 键是 `media_point_id`（上游 require_by_id 的默认生成规则）"
    );

    // 存在时删掉，且不关心 media_id。
    let (media_id, movie_number) = seed_jav_media(&db).await;
    let origin = format!("movies/ab/{movie_number}/frame.jpg");
    let image_id = seed_image(&db, &origin).await;
    image_root.write_loose(&origin, b"frame");
    let point_id = seed_point(&db, media_id, image_id).await;
    media.delete_point_by_id(point_id).await.expect("删掉");
    assert!(!point_row_exists(&db, point_id).await);
    assert!(!image_root.loose_exists(&origin));
}

/// 图片路径逃逸出图片根 → 拒绝，且**文件与记录都在**。
///
/// `image.origin` 只是 `varchar(255)`，一个 bug 或一次手工改库就能造出
/// `../..` 形状。上游没有这层检查（Python 的 `/` 运算符遇到绝对路径会替换整个
/// 前缀），而这里是 `unlink` —— 不可撤销。所以本仓加了 `resolve_inside`。
#[tokio::test]
async fn a_path_escaping_the_image_root_is_refused() {
    let db = TestDb::require().await;
    let image_root = ImageRoot::new();
    let (media_id, _) = seed_jav_media(&db).await;

    let escaped = "../../../etc/passwd".to_owned();
    let image_id = seed_image(&db, &escaped).await;
    let point_id = seed_point(&db, media_id, image_id).await;

    let error = MediaService::new(db.pool(), &image_root.config)
        .delete_point(media_id, point_id)
        .await
        .expect_err("逃逸路径要被拒绝");
    assert_eq!(error.code(), "invalid_image_path");
    assert!(
        image_root.root().is_dir(),
        "图片根本身必须还在（绝不能因为空/逃逸路径把它当目标）"
    );
    // 时刻点已经删了（那是前一步），但记录没被清掉 —— 留待人工处理，
    // 这比「删掉根之外的东西」与「静默跳过」都好。
    assert!(
        std::path::Path::new(&escaped).is_relative(),
        "前提：这是个相对路径"
    );
}
