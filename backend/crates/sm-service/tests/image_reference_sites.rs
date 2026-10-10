//! 图片引用方清单的**对拍**测试。
//!
//! # 这个文件存在的唯一理由
//!
//! `IMAGE_REFERENCE_SITES` 决定「一张图还有没有人用」，而它被用在**删除**路径
//! 上。漏一处的后果不是报错，是**删掉一张正在被引用的图** —— 表现为「封面
//! 忽然裂了」，而且删掉的记录回滚不回来。
//!
//! 所以这里不抄清单，而是**直接问 PostgreSQL**：把所有指向 `image(id)` 的
//! 外键列读出来，与常量逐条对拍。这样「新增一张引用 `image` 的表」会在 CI 里
//! 红，而不是等线上裂图。
//!
//! # 它本来会抓到什么
//!
//! 骨架期那份清单只有五项，且第五项写成 `plot_image.image_id` —— **那张表不
//! 存在**（真名 `movie_plot_image`）。漏掉的三处里 `media_point.image_id`
//! 最危险：它 `NOT NULL` + `RESTRICT`，也就是「每个时刻点都钉着一张图」，
//! 漏查等于每次删时刻点都顺手删掉那张图。
//!
//! 上游出处：`catalog/image_cleanup_service.image_record_is_still_used`。

use sm_db::repo::{ImageRepository, NewImage, IMAGE_REFERENCE_SITES};
use sm_db::testing::TestDb;
use sm_service::catalog::image_cleanup::ImageCleanupService;
use sm_service::system::config::ConfigService;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 建一个清理服务。配置指向一个不存在的文件 —— `ConfigService::snapshot`
/// 对缺文件返回 schema 缺省值，正是测试要的「干净起点」。
fn cleanup_service(db: &TestDb) -> ImageCleanupService {
    let config_path = std::env::temp_dir().join(format!("sm-image-cleanup-{}.json", n()));
    ImageCleanupService::new(db.pool(), &ConfigService::new(config_path))
}

async fn seed_image(db: &TestDb, origin: &str) -> i32 {
    ImageRepository::new(db.pool().clone())
        .upsert(&NewImage {
            origin: origin.to_owned(),
        })
        .await
        .expect("upsert image")
        .0
}

/// 建一个**指向该图**的时刻点。
///
/// `media_id` 可空，所以不必再造一条 `media` 行 —— 这条测试要的只是「有一行
/// 外键指过去」。
async fn seed_point_holding(db: &TestDb, image_id: i32) -> i32 {
    let row = sqlx::query_as::<_, (i32,)>(
        "INSERT INTO media_point \
             (media_id, thumbnail_id, image_id, movie_number, video_item_id, offset_seconds, \
              created_at, updated_at) \
         VALUES (NULL, NULL, $1, NULL, NULL, 0, $2, $2) RETURNING id",
    )
    .bind(image_id)
    .bind(sm_db::common::time::now_utc())
    .fetch_one(db.pool())
    .await
    .expect("insert media_point");
    row.0
}

/// ★ 所有指向 `image(id)` 的外键列，与 `IMAGE_REFERENCE_SITES` 逐条相等。
#[tokio::test]
async fn every_foreign_key_to_image_is_listed_in_the_reference_sites() {
    let db = TestDb::require().await;

    // ⚠️ schema 用 `current_schema()`，**不能写死 `'public'`**。
    //
    // `TestDb` 把 DDL 应用到一个独立的 `smdb_test_<hash>` schema，并 `SET
    // search_path` 指过去（`sm-db/src/testing/db.rs:120-136`）—— 表不在 `public`
    // 里。写死 `'public'` 时这个查询返回**空集**，于是 `extra` 断言把 8 个已登记
    // 的引用方全报成「DDL 里不存在」，看起来像常量写错了，实际是查询找错了地方。
    // `current_schema()` = `search_path` 里第一个**存在**的 schema，生产和测试
    // 两种布局都对。
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT tc.table_name::text, kcu.column_name::text \
         FROM information_schema.table_constraints AS tc \
         JOIN information_schema.key_column_usage AS kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
         JOIN information_schema.constraint_column_usage AS ccu \
           ON tc.constraint_name = ccu.constraint_name \
          AND tc.table_schema = ccu.table_schema \
         WHERE tc.constraint_type = 'FOREIGN KEY' \
           AND tc.table_schema = current_schema() \
           AND ccu.table_name = 'image' \
           AND ccu.column_name = 'id'",
    )
    .fetch_all(db.pool())
    .await
    .expect("读 information_schema");

    let mut from_db: Vec<String> = rows
        .into_iter()
        .map(|(table, column)| format!("{table}.{column}"))
        .collect();
    from_db.sort();

    let mut declared: Vec<String> = IMAGE_REFERENCE_SITES
        .iter()
        .map(|site| (*site).to_owned())
        .collect();
    declared.sort();

    let missing: Vec<&String> = from_db.iter().filter(|s| !declared.contains(s)).collect();
    let extra: Vec<&String> = declared.iter().filter(|s| !from_db.contains(s)).collect();
    assert!(
        missing.is_empty(),
        "DDL 里有指向 image(id) 的外键没被登记：{missing:?}\n\
         漏登记 = 那张图被判成「没人用」然后被删。补进 IMAGE_REFERENCE_SITES，\
         并同步 ImageRepository 的 `IMAGE_REFERENCED_SQL`。"
    );
    assert!(
        extra.is_empty(),
        "登记的引用方在 DDL 里不存在：{extra:?}\n\
         大多是表名/列名写错（骨架期就把 movie_plot_image 写成了 plot_image）。"
    );
}

/// 被时刻点钉住的图**不能**被删记录。
#[tokio::test]
async fn an_image_held_by_a_media_point_is_referenced() {
    let db = TestDb::require().await;
    let service = cleanup_service(&db);
    let image_id = seed_image(&db, &format!("pt/{}.jpg", n())).await;
    let point_id = seed_point_holding(&db, image_id).await;

    assert!(
        service
            .image_record_is_still_used(image_id)
            .await
            .expect("查引用"),
        "被 media_point 钉住的图必须算「仍在用」"
    );
    assert!(
        service
            .delete_image_record_if_unused(Some(image_id))
            .await
            .expect("删记录")
            .is_empty(),
        "仍在用的图不该被删，也不该回传 origin"
    );
    assert!(
        ImageRepository::new(db.pool().clone())
            .find_by_id(image_id)
            .await
            .expect("复查")
            .is_some(),
        "记录必须还在"
    );

    // 收尾：放开引用后再删，确认「删掉引用者之后就能删」—— 否则这条测试
    // 可能在「is_referenced 恒为 true」的错误实现下也通过。
    sqlx::query("DELETE FROM media_point WHERE id = $1")
        .bind(point_id)
        .execute(db.pool())
        .await
        .expect("删时刻点");
    assert!(
        !service
            .image_record_is_still_used(image_id)
            .await
            .expect("复查引用"),
        "引用者删掉后就该算「没人用」"
    );
}

/// 无人引用的图：删记录并回传 origin，供调用方删磁盘文件。
#[tokio::test]
async fn an_unreferenced_image_loses_its_record_and_yields_its_origin() {
    let db = TestDb::require().await;
    let service = cleanup_service(&db);
    let origin = format!("movies/ab/ABC-{:03}/1.jpg", n());
    let image_id = seed_image(&db, &origin).await;

    assert!(
        !service
            .image_record_is_still_used(image_id)
            .await
            .expect("查引用"),
        "没有任何引用方"
    );
    assert_eq!(
        service
            .delete_image_record_if_unused(Some(image_id))
            .await
            .expect("删记录"),
        vec![origin],
        "回传的 origin 是调用方删磁盘文件的依据"
    );
    assert!(
        ImageRepository::new(db.pool().clone())
            .find_by_id(image_id)
            .await
            .expect("复查")
            .is_none(),
        "记录应当已被删掉"
    );
}

/// `None` 与不存在的 id 都返回空集，**不报错**。
///
/// 上游：`image is None -> set()`，以及 `require_by_id` 之外的那条路。报错会让
/// 「本来就很干净」的库无法完成清理 —— 而清理是删除流程的最后一步，卡在那
/// 里意味着媒体永远删不掉。
#[tokio::test]
async fn a_missing_or_absent_image_yields_no_origins_and_no_error() {
    let db = TestDb::require().await;
    let service = cleanup_service(&db);

    assert!(service
        .delete_image_record_if_unused(None)
        .await
        .expect("None 不该报错")
        .is_empty());

    assert!(
        service
            .delete_image_record_if_unused(Some(i32::MAX))
            .await
            .expect("不存在的 id 不该报错")
            .is_empty(),
        "不存在的 id 没有任何 origin 可回传"
    );
}

/// 空 `origin` 不进回传列表。
///
/// 上游是 `{relative_path} if relative_path else set()`。空路径传到删文件那一层
/// 没有意义，而且它会被解析成**图片根本身**（本仓的 `resolve_inside` 会因此
/// 报错，但上游会直接对根目录 `unlink`）。
#[tokio::test]
async fn an_empty_origin_is_not_handed_to_the_file_deleter() {
    let db = TestDb::require().await;
    let service = cleanup_service(&db);
    // `NewImage::validate` 拒绝空 origin，所以只能裸 SQL 造这一行 —— 而它正是
    // 上游那个 `if relative_path` 分支要防的形状。
    let row: (i32,) = sqlx::query_as(
        "INSERT INTO image (origin, created_at, updated_at) VALUES ('', $1, $1) RETURNING id",
    )
    .bind(sm_db::common::time::now_utc())
    .fetch_one(db.pool())
    .await
    .expect("插入空 origin 的 image");

    assert!(
        service
            .delete_image_record_if_unused(Some(row.0))
            .await
            .expect("删记录")
            .is_empty(),
        "空 origin 不该进回传列表"
    );
}
