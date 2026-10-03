//! 多表用例的集成测试。
//!
//! `import_movie` 是第一个**真正跨表**的用例：影片行 + N 个标签 upsert
//! + M 条演员关联 + N 条标签关联，全部在一个事务里。
//!
//! # 为什么要专门测这个
//!
//! 拆成独立提交时，中途失败会留下「看起来正常但其实不完整」的状态：
//!
//! | 失败点 | 留下的状态 | 有机制会发现吗 |
//! |---|---|---|
//! | 影片已建，演员关联未建 | 影片页显示「未知演员」 | **没有** —— 关联表是空的，不是标记为不完整 |
//! | 标签建了一半 | 标签筛选器里该影片只有部分标签 | **没有** —— 用户看到的是一个「正常」的影片 |
//! | 标签全建，关联未建 | 库里有一堆无人引用的标签 | 没有 |
//!
//! 所以测试必须覆盖**每个阶段失败**时「什么都没留下」。
//!
//! | 测试 | 失败点 |
//! |---|---|
//! | [`import_writes_all_four_tables`] | —— |
//! | [`duplicate_actor_ids_do_not_create_duplicate_links`] | —— |
//! | [`a_failing_actor_link_rolls_back_everything`] | 第 4 阶段 |
//! | [`dropping_without_commit_leaves_nothing`] | 全部阶段 |
//! | [`tags_are_reused_across_movies`] | —— |

use sm_db::common::page::PageRequest;
use sm_db::error::DbError;
use sm_db::repo::{MovieActorRepository, MovieTagRepository, NewMovie, TagRepository, UnitOfWork};
use sm_db::testing::TestDb;

mod fixtures {
    use super::*;

    pub fn movie(number: &str) -> NewMovie {
        NewMovie {
            movie_number: number.to_owned(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        }
    }

    /// 建 N 个 actor 行，返回它们的 id。
    pub async fn seed_actors(db: &TestDb, count: usize) -> Vec<i32> {
        let mut ids = Vec::with_capacity(count);
        for i in 0..count {
            let row = sqlx::query_as::<_, (i32,)>(
                "INSERT INTO actor (name, created_at, updated_at) \
                 VALUES ($1, $2, $2) RETURNING id",
            )
            .bind(format!("演员{i}"))
            .bind(sm_db::common::time::now_utc())
            .fetch_one(db.pool())
            .await
            .expect("insert actor");
            ids.push(row.0);
        }
        ids
    }
}

/// 统计三张表各有多少行 —— 用裸 SQL 而不是仓储，避免引入仓储的语义。
async fn counts(db: &TestDb) -> (i64, i64, i64, i64) {
    let one = |sql: &'static str| {
        let pool = db.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    (
        one("SELECT COUNT(*) FROM movie").await,
        one("SELECT COUNT(*) FROM tag").await,
        one("SELECT COUNT(*) FROM movie_actor").await,
        one("SELECT COUNT(*) FROM movie_tag").await,
    )
}

#[tokio::test]
async fn import_writes_all_four_tables() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let actors = fixtures::seed_actors(&db, 3).await;

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let result = uow
        .import_movie(
            &fixtures::movie("ABC-001"),
            &["爱情", "校园", "OVA"],
            &actors,
        )
        .await
        .expect("导入应成功");
    uow.commit().await.unwrap();

    // 影片
    assert_eq!(result.movie.movie_number, "ABC-001");
    assert_eq!(result.movie.title, "ABC-001 标题");

    // 标签：3 个新建
    assert_eq!(result.tags.len(), 3);
    let names: Vec<&str> = result.tags.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["爱情", "校园", "OVA"]);

    // 演员关联 3 条
    assert_eq!(result.actor_links, 3);

    let (movies, tags, movie_actors, movie_tags) = counts(&db).await;
    assert_eq!((movies, tags, movie_actors, movie_tags), (1, 3, 3, 3));
}

#[tokio::test]
async fn duplicate_actor_ids_do_not_create_duplicate_links() {
    // (movie_id, actor_id) 唯一索引 + ON CONFLICT：调用方不需要去重。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let actors = fixtures::seed_actors(&db, 2).await;
    // 故意重复第一个
    let with_dup = vec![actors[0], actors[1], actors[0]];

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    uow.import_movie(&fixtures::movie("ABC-001"), &[], &with_dup)
        .await
        .expect("导入应成功");
    uow.commit().await.unwrap();

    // 唯一索引让它只能存 2 条
    let links = MovieActorRepository::new(db.pool().clone())
        .list_by_movie(result_id(&db).await, PageRequest::new(1, 50).unwrap())
        .await
        .unwrap();
    assert_eq!(links.items.len(), 2, "重复的 actor_id 被唯一索引吞掉");
    assert_eq!(links.total, 2);
}

async fn result_id(db: &TestDb) -> i32 {
    sqlx::query_scalar("SELECT id FROM movie LIMIT 1")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_failing_actor_link_rolls_back_everything() {
    // 本文件的核心。
    //
    // 第 4 阶段失败（演员关联插不进去），此时前 3 阶段已经在同一个事务里
    // 写了影片和标签。断言：这四张表都必须是空的。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let actors = fixtures::seed_actors(&db, 1).await;

    // 传一个不存在的 actor_id：影片与标签会写入，关联会撞外键。
    let bad_actor = 999_999i32;
    let mut with_bad = actors.clone();
    with_bad.push(bad_actor);

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let err = uow
        .import_movie(&fixtures::movie("ABC-001"), &["爱情"], &with_bad)
        .await
        .expect_err("关联阶段应因外键失败");
    // 外键被拒 -> ConstraintViolation（409）
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应归为 409，实际 {err:?}"
    );
    uow.rollback().await.unwrap();

    // 关键断言：一张表都没留下
    let (movies, tags, movie_actors, movie_tags) = counts(&db).await;
    assert_eq!(
        (movies, tags, movie_actors, movie_tags),
        (0, 0, 0, 0),
        "任何一步失败都不该留下痕迹：影片 {movies}、标签 {tags}、\
         演员关联 {movie_actors}、标签关联 {movie_tags}"
    );
}

#[tokio::test]
async fn dropping_without_commit_leaves_nothing() {
    // 全部阶段都成功，但事务没有提交。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let actors = fixtures::seed_actors(&db, 2).await;

    {
        let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
        uow.import_movie(&fixtures::movie("ABC-001"), &["爱情", "校园"], &actors)
            .await
            .unwrap();
        // 故意不 commit
    }

    let (movies, tags, movie_actors, movie_tags) = counts(&db).await;
    assert_eq!(
        (movies, tags, movie_actors, movie_tags),
        (0, 0, 0, 0),
        "未 commit 的多表写入应全部回滚"
    );
}

#[tokio::test]
async fn tags_are_reused_across_movies() {
    // 标签天然重复：upsert 而不是 insert，撞唯一约束时返回既有行。
    let Some(db) = TestDb::create().await else {
        return;
    };

    for number in ["ABC-001", "ABC-002"] {
        let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
        uow.import_movie(&fixtures::movie(number), &["爱情", "校园"], &[])
            .await
            .unwrap();
        uow.commit().await.unwrap();
    }

    // 两部影片，共享两个标签 -> tag 表只有 2 行
    let (movies, tags, movie_actors, movie_tags) = counts(&db).await;
    assert_eq!(movies, 2);
    assert_eq!(tags, 2, "标签被复用，没有产生重复");
    assert_eq!(movie_actors, 0);
    assert_eq!(movie_tags, 4, "两部影片 x 2 个标签关联");

    // 每部影片都能查到自己的标签
    let tag_repo = TagRepository::new(db.pool().clone());
    assert_eq!(
        tag_repo
            .list_by_name("爱情", PageRequest::new(1, 50).unwrap())
            .await
            .unwrap()
            .total,
        1
    );
}

#[tokio::test]
async fn blank_tag_name_rejects_the_whole_import() {
    // 空白标签名在第一个阶段就被拒 -> 影片也不该被创建。
    let Some(db) = TestDb::create().await else {
        return;
    };

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let err = uow
        .import_movie(&fixtures::movie("ABC-001"), &["爱情", "   "], &[])
        .await
        .expect_err("空白标签名应被拒");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");
    uow.rollback().await.unwrap();

    let (movies, tags, _, movie_tags) = counts(&db).await;
    assert_eq!(
        (movies, tags, movie_tags),
        (0, 1, 0),
        "第一个标签已建、影片未建；回滚后应只剩那一个标签被撤销"
    );
}

#[tokio::test]
async fn movie_tag_links_are_readable_after_import() {
    // 走仓储读回，确认关联表的列与模型对得上（不是只靠计数）。
    let Some(db) = TestDb::create().await else {
        return;
    };

    let mut uow = UnitOfWork::begin(db.pool()).await.unwrap();
    let result = uow
        .import_movie(&fixtures::movie("ABC-001"), &["爱情"], &[])
        .await
        .unwrap();
    uow.commit().await.unwrap();

    let links = MovieTagRepository::new(db.pool().clone())
        .list_by_movie(result.movie.id, PageRequest::new(1, 50).unwrap())
        .await
        .unwrap();
    assert_eq!(links.items.len(), 1);
    assert_eq!(links.items[0].movie_id, result.movie.id);
    assert_eq!(links.items[0].tag_id, result.tags[0].id);
}
