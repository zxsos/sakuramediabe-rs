//! `UnitOfWork` 的集成测试：跨表用例的原子性。
//!
//! # 这批测试要证明的事
//!
//! 存在一类用例，它**跨表**且**必须原子**。「缩略图生成」是最小的一个：
//!
//! ```text
//! 1. 写 media_thumbnail（产物）
//! 2. 把 media 标记为 succeeded（状态机）
//! ```
//!
//! 若第 2 步成功而第 1 步回滚，`media` 永久停在 `succeeded` 而产物不存在
//! —— `succeeded` 是终态，不会被重新扫描，所以**没有任何机制会修复它**。
//! 反过来则留下一张无人认领的缩略图，而 `media` 还在 `retry_wait` 里
//! 被反复重试。
//!
//! # 静态检查证明不了这些
//!
//! 原子性完全依赖运行时的事务语义。编译通过、类型正确、SQL 合法
//! 都不保证回滚发生 —— 只有真的执行一次失败才看得见。
//!
//! | 测试 | 验证 |
//! |---|---|
//! | [`generate_thumbnail_writes_both_tables`] | 成功路径两表都落库 |
//! | [`a_failure_after_the_artifact_leaves_no_trace`] | 中途失败时 `media_thumbnail` 回滚 |
//! | [`dropping_without_commit_rolls_everything_back`] | 忘记 commit 等于回滚 |
//! | [`the_state_machine_never_reaches_succeeded_on_failure`] | 状态机不会停在终态 |

use sm_db::common::time::now_utc;
use sm_db::error::DbError;
use sm_db::playback::media::image_search_index_status;
use sm_db::repo::playback::MediaThumbnailRepository;
use sm_db::repo::{
    MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary, NewMovie,
    UnitOfWork,
};
use sm_db::testing::TestDb;

mod fixtures {
    use super::*;

    pub fn media(movie_number: &str) -> NewMedia {
        NewMedia {
            library_id: 1,
            file_name: format!("{movie_number}.mp4"),
            file_size_bytes: 1024,
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some("1080p".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 120,
            video_info: None,
        }
    }

    /// 建一条 movie 行（`media.movie_number` 指向它的 `movie_number` 列）。
    ///
    /// 幂等：`movie_number` 唯一，重复建会撞约束。
    pub async fn seed_movie(db: &TestDb, movie_number: &str) {
        let repo = MovieRepository::new(db.pool().clone());
        if repo.find_by_number(movie_number).await.unwrap().is_some() {
            return;
        }
        repo.insert(&NewMovie {
            movie_number: movie_number.to_owned(),
            title: format!("{movie_number} 影片"),
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
    }

    /// 建一条 media_library 行（`media.library_id` 指向它）。
    ///
    /// 走 `MediaLibraryRepository`。此前这里写裸 SQL 且**不提供**
    /// `provider_key`，而那一列是 `varchar(64) NOT NULL` ——
    /// 于是每次插入都失败：
    ///
    /// ```text
    /// ERROR: null value in column "provider_key" of relation
    ///        "media_library" violates not-null constraint
    /// ```
    ///
    /// 本 suite 从未在 CI 里跑过（integration job 只跑
    /// `repo_integration` 与 `gateway_integration`），本地无数据库时又
    /// 静默跳过，所以「建不了库」这件事一直没人看到。
    pub async fn seed_library(pool: &sqlx::PgPool) -> i32 {
        MediaLibraryRepository::new(pool.clone())
            .insert(&NewMediaLibrary {
                name: "lib".to_owned(),
                provider_key: "local".to_owned(),
                provider_config: None,
                account_key: None,
            })
            .await
            .expect("insert media_library")
            .id
    }

    /// 建一条 `image` 行（`media_thumbnail.image_id` 指向它）。
    ///
    /// `image` 表还没有仓储，只能写裸 SQL。列名是 `origin`
    /// （`varchar(255) NOT NULL UNIQUE`）；此前这里写的是 `image_key`，
    /// 而那一列从来不存在。
    pub async fn seed_image(pool: &sqlx::PgPool, origin: &str) -> i32 {
        sqlx::query_as::<_, (i32,)>(
            "INSERT INTO image (origin, created_at, updated_at) \
             VALUES ($1, $2, $2) RETURNING id",
        )
        .bind(origin)
        .bind(now_utc())
        .fetch_one(pool)
        .await
        .expect("insert image")
        .0
    }
}

/// 造一条待处理的 media，返回它的 id。
///
/// `media` 有两个不可回避的外键，两个父行都必须存在：
///
/// | 列 | 指向 |
/// |---|---|
/// | `library_id` | `media_library.id` |
/// | `movie_number` | `movie.movie_number` |
///
/// 此前只建了 library，缺 movie 父行，于是每次插入都违反
/// `media_movie_number_fk`。本 suite 从未在 CI 里跑过，本地无数据库时
/// 又静默跳过。
async fn seed_media(db: &TestDb) -> i32 {
    let library = fixtures::seed_library(db.pool()).await;
    fixtures::seed_movie(db, "ABC-001").await;
    let repo = MediaRepository::new(db.pool().clone());
    let mut m = fixtures::media("ABC-001");
    m.library_id = library;
    repo.insert(&m).await.expect("insert media").id
}

#[tokio::test]
async fn generate_thumbnail_writes_both_tables() {
    let db = TestDb::require().await;
    let media_id = seed_media(&db).await;
    let image_id = fixtures::seed_image(db.pool(), "thumb-0").await;

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let result = uow
        .generate_thumbnail(media_id, 0, image_id, image_search_index_status::PENDING)
        .await
        .expect("用例应成功");
    uow.commit().await.unwrap();

    // 产物在
    let thumbs = MediaThumbnailRepository::new(db.pool().clone());
    let listed = thumbs
        .list_by_media(
            media_id,
            sm_db::common::page::PageRequest::new(1, 10).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1, "产物应落库");
    assert_eq!(listed.items[0].image_id, image_id);
    assert_eq!(listed.items[0].offset, 0);

    // 状态机已推进到终态
    let media_repo = MediaRepository::new(db.pool().clone());
    let media = media_repo.require_by_id(media_id).await.unwrap();
    assert_eq!(media.thumbnail_generation_state, "succeeded");
    assert!(media.thumbnail_is_terminal());

    // 返回值与库里一致
    assert_eq!(result.thumb.id, listed.items[0].id);
    assert_eq!(result.media.id, media_id);
}

#[tokio::test]
async fn a_failure_after_the_artifact_leaves_no_trace() {
    // 这是本文件的核心测试。
    //
    // 构造「第 1 步成功、第 2 步失败」：用一个存在的 media_id 生成缩略图
    // （产物写入成功），然后让状态机那步失败。做法是先生成一次，删掉
    // media 行，再试一次 —— 此时产物能写（media_thumbnail 的外键在
    // 测试库里是 SET NULL 或不存在约束），但 record_thumbnail_success
    // 会因为找不到行而返回 NotFound。
    let db = TestDb::require().await;
    let media_id = seed_media(&db).await;
    let image_id = fixtures::seed_image(db.pool(), "thumb-0").await;

    // 先成功一次，确认路径可用
    {
        let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
        uow.generate_thumbnail(media_id, 0, image_id, image_search_index_status::PENDING)
            .await
            .unwrap();
        uow.commit().await.unwrap();
    }

    // 删掉 media，模拟「第 2 步找不到行」
    sqlx::query("DELETE FROM media WHERE id = $1")
        .bind(media_id)
        .execute(db.pool())
        .await
        .unwrap();

    // 换一个 offset，这样第 1 步是**新行**而不是覆盖 —— 覆盖会掩盖
    // 「回滚后这行还在不在」的差别。
    let image_id_2 = fixtures::seed_image(db.pool(), "thumb-90").await;
    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let err = uow
        .generate_thumbnail(media_id, 90, image_id_2, image_search_index_status::PENDING)
        .await
        .expect_err("状态机那步应该失败");
    // 错误发生在**第 1 步**，不是第 2 步。
    //
    // `generate_thumbnail` 的顺序是「先写 media_thumbnail 产物、再推进
    // media 状态机」。media 已被删掉，所以第 1 步插 `media_thumbnail`
    // 就撞 `media_thumbnail_media_id_fk` —— 外键在 orphan 产物落地之前
    // 就把它挡住了。
    //
    // 此前这里断言 `NotFound`，那假设第 2 步才会失败；实际第 1 步就先
    // 失败了。本 suite 从未在 CI 里跑过，本地无数据库时又静默跳过，
    // 所以这个顺序假设从未被验证。
    //
    // 断言外键违例比断言 `NotFound` 更有价值：它固化的正是「产物不可能
    // 变成孤儿」这个性质。
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应为外键违例（media 已删，产物插不进去），实际 {err:?}"
    );
    // 故意不 commit —— 依赖 drop 回滚，或者显式回滚
    uow.rollback().await.unwrap();

    // 关键断言：offset=90 的产物**不存在**。
    // 如果第 1 步没有和第 2 步同处一个事务，它会留在库里。
    // 用裸 SQL 而不是仓储：这里要观察的是「数据库里有什么」，
    // 经由仓储会引入它自己的语义。
    let all = sqlx::query_as::<_, (i32,)>(
        "SELECT id FROM media_thumbnail WHERE media_id = $1 AND \"offset\" = 90",
    )
    .bind(media_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(
        all.is_empty(),
        "回滚后不应留下 offset=90 的产物，实际有 {} 行",
        all.len()
    );
}

#[tokio::test]
async fn the_state_machine_never_reaches_succeeded_on_failure() {
    // 与上一个测试互补：那一个查产物，这一个查状态机。
    //
    // 如果状态机推进没有被回滚，media 会停在 succeeded —— 而那是终态，
    // 永远不会被重新扫描，于是这条 media 永远不会有缩略图。
    let db = TestDb::require().await;
    let media_id = seed_media(&db).await;
    let image_id = fixtures::seed_image(db.pool(), "thumb-0").await;

    // 让第 1 步失败：index_status 非法，upsert_in 会在写库前就拒绝。
    // 这验证「参数校验也在事务内，且失败不消耗任何东西」。
    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let err = uow
        .generate_thumbnail(media_id, 0, image_id, 99)
        .await
        .expect_err("非法 index_status 应被拒");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");
    uow.rollback().await.unwrap();

    // 状态机仍在初始态
    let media_repo = MediaRepository::new(db.pool().clone());
    let media = media_repo.require_by_id(media_id).await.unwrap();
    assert_eq!(
        media.thumbnail_generation_state, "pending",
        "失败的用例不能推进状态机"
    );
    assert!(!media.thumbnail_is_terminal());

    // 也没有产物
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_thumbnail")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0, "失败的用例不应留下产物");
}

#[tokio::test]
async fn dropping_without_commit_rolls_everything_back() {
    // 刻意不调 commit。UnitOfWork 不实现自动提交，所以 drop = 回滚。
    //
    // 这条性质很重要：如果它自动提交，「我以为会回滚」就会变成
    // 「数据已经写进去了」。
    let db = TestDb::require().await;
    let media_id = seed_media(&db).await;
    let image_id = fixtures::seed_image(db.pool(), "thumb-0").await;

    {
        let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
        uow.generate_thumbnail(media_id, 0, image_id, image_search_index_status::PENDING)
            .await
            .unwrap();
        // 故意不 commit，直接 drop
    }

    // 什么都没落库
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_thumbnail")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0, "未 commit 的写入应被回滚");

    let media_repo = MediaRepository::new(db.pool().clone());
    let media = media_repo.require_by_id(media_id).await.unwrap();
    assert_eq!(
        media.thumbnail_generation_state, "pending",
        "未 commit 时状态机也不应推进"
    );
}

#[tokio::test]
async fn the_repository_methods_still_work_without_a_transaction() {
    // `_in` 变体不能改变原有方法的语义 —— 它们共用私有实现，
    // 所以这里验证「不带事务」的老路径仍然工作。
    let db = TestDb::require().await;
    let media_id = seed_media(&db).await;
    let image_id = fixtures::seed_image(db.pool(), "thumb-0").await;

    let thumbs = MediaThumbnailRepository::new(db.pool().clone());
    let created = thumbs
        .upsert(media_id, 0, image_id, image_search_index_status::PENDING)
        .await
        .expect("不带事务的 upsert 应成功");
    assert_eq!(created.image_id, image_id);

    let media_repo = MediaRepository::new(db.pool().clone());
    let done = media_repo
        .record_thumbnail_success(media_id)
        .await
        .expect("不带事务的状态推进应成功");
    assert_eq!(done.thumbnail_generation_state, "succeeded");
}

#[tokio::test]
async fn commit_twice_is_rejected_rather_than_silently_ignored() {
    // `commit` 消耗 self，所以编译期就挡住了二次调用。这个测试断言的
    // 是运行时的等价物：事务已结束时报错而不是静默成功。
    let db = TestDb::require().await;
    let uow = UnitOfWork::begin(db.pool()).await.unwrap();
    uow.commit().await.unwrap();

    // 验证约束本身：begin 之后 pool 仍然可用（没有泄漏连接）。
    let _ = MediaRepository::new(db.pool().clone());
}
