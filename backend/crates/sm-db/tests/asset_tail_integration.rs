//! `subtitle` / `movie_plot_image` / `system_notification` /
//! `schema_migration` 的集成测试 —— `sm_db` 全表覆盖的最后一批。
//!
//! 四张表都很小，但每张都有一个「反直觉之处」：
//!
//! | 表 | 反直觉之处 |
//! |---|---|
//! | `subtitle` | 唯一索引是 `(movie_id, file_path)` —— 同一部影片可以有**多**个字幕文件 |
//! | `movie_plot_image` | **没有时间戳**（全库第三张不继承 `TimestampedMixin` 的表） |
//! | `system_notification` | `dedupe_key` 唯一，但 **NULL 不参与唯一约束** |
//! | `schema_migration` | **没有时间戳**，且启动流程必须能**重入** |

use sm_db::common::page::PageRequest;
use sm_db::error::DbError;
use sm_db::repo::{
    MoviePlotImageRepository, MovieRepository, NewMovie, NewNotification, NewSubtitle,
    SchemaMigrationRepository, SubtitleRepository, SystemNotificationRepository,
};
use sm_db::system::activity::SystemNotification;
use sm_db::testing::TestDb;

fn page() -> PageRequest {
    PageRequest::new(1, 50).unwrap()
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

async fn seed_movie(db: &TestDb) -> i32 {
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("TAIL-{:06}", n()),
            title: "尾部影片".to_owned(),
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

fn notification(category: &str, title: &str) -> NewNotification {
    NewNotification {
        category: category.to_owned(),
        title: title.to_owned(),
        content: "内容".to_owned(),
        event_type: None,
        dedupe_key: None,
        resource_type: None,
        resource_id: None,
        related_task_run_id: None,
        related_resource_type: None,
        related_resource_id: None,
    }
}
// ================================================================ subtitle

#[tokio::test]
async fn one_movie_may_have_several_subtitle_files_but_a_path_registers_once() {
    // 唯一索引是 `(movie_id, file_path)` —— 唯一性**包含**影片，所以同一
    // 部影片可以有多个字幕轨（中文、英文），而同一个路径只登记一次。
    let db = TestDb::require().await;
    let repo = SubtitleRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;

    assert!(repo
        .upsert(&NewSubtitle {
            movie_id: movie,
            file_path: "zh.srt".to_owned(),
        })
        .await
        .unwrap());
    assert!(repo
        .upsert(&NewSubtitle {
            movie_id: movie,
            file_path: "en.srt".to_owned(),
        })
        .await
        .unwrap());

    // 重复登记同一路径 —— 幂等，`false` 表示早就有了。
    assert!(
        !repo
            .upsert(&NewSubtitle {
                movie_id: movie,
                file_path: "  zh.srt  ".to_owned(),
            })
            .await
            .unwrap(),
        "刮削重跑不该产生第二条"
    );
    assert_eq!(repo.list_by_movie(movie).await.unwrap().len(), 2);

    // 另一部影片用同样的路径 —— 不冲突（唯一索引含 movie_id）。
    let other = seed_movie(&db).await;
    assert!(repo
        .upsert(&NewSubtitle {
            movie_id: other,
            file_path: "zh.srt".to_owned(),
        })
        .await
        .unwrap());
}

#[tokio::test]
async fn blank_subtitle_path_is_rejected_and_clearing_respects_the_movie() {
    let db = TestDb::require().await;
    let repo = SubtitleRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;

    for blank in ["", "   "] {
        let err = repo
            .upsert(&NewSubtitle {
                movie_id: movie,
                file_path: blank.to_owned(),
            })
            .await
            .expect_err("空路径无意义");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }

    repo.upsert(&NewSubtitle {
        movie_id: movie,
        file_path: "a.srt".to_owned(),
    })
    .await
    .unwrap();
    let keep = seed_movie(&db).await;
    repo.upsert(&NewSubtitle {
        movie_id: keep,
        file_path: "b.srt".to_owned(),
    })
    .await
    .unwrap();

    assert_eq!(
        repo.clear_movie(movie).await.unwrap(),
        1,
        "只清这一部影片的"
    );
    assert!(repo.list_by_movie(movie).await.unwrap().is_empty());
    assert_eq!(repo.list_by_movie(keep).await.unwrap().len(), 1);
}

#[tokio::test]
async fn subtitles_go_away_with_their_movie() {
    // `subtitle_movie_id_fk` 是 `ON DELETE CASCADE`。
    let db = TestDb::require().await;
    let repo = SubtitleRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    repo.upsert(&NewSubtitle {
        movie_id: movie,
        file_path: "x.srt".to_owned(),
    })
    .await
    .unwrap();

    sqlx::query("DELETE FROM movie WHERE id = $1")
        .bind(movie)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(repo.list_by_movie(movie).await.unwrap().is_empty());
}
// ================================================================ movie_plot_image

#[tokio::test]
async fn a_plot_image_links_once_and_starts_pending() {
    // 唯一索引 `(movie_id, image_id)`，重复关联是 `DO NOTHING` 而不是报错
    // —— 刮削重跑会发现同一张图。
    //
    // 本表**没有时间戳**，所以 `DO NOTHING` 而不是 `DO UPDATE SET updated_at`
    // —— 没有可以更新的时刻字段。
    let db = TestDb::require().await;
    let repo = MoviePlotImageRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;
    let image = sm_db::repo::ImageRepository::new(db.pool().clone())
        .upsert(&sm_db::repo::NewImage {
            origin: format!("plot/{}.jpg", n()),
        })
        .await
        .unwrap()
        .0;

    assert!(repo.link(movie, image).await.unwrap(), "首次应真的新增");
    assert!(!repo.link(movie, image).await.unwrap(), "重复应幂等");

    let listed = repo.list_by_movie(movie).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].image_search_index_status,
        sm_db::catalog::asset::image_search_index_status::PENDING,
        "新关联的剧照状态是 PENDING"
    );
}

#[tokio::test]
async fn the_index_status_index_exists_for_a_worker_that_scans_pending() {
    // ```text
    // INDEX (image_search_index_status, id)
    // ```
    //
    // 那条索引**存在就是为了**这个查询：worker 扫 PENDING 去做向量化。
    // 所以在 `list_by_index_status` 里给出它，而不是只留一个按影片查的
    // 接口 —— 索引是为某个查询建的，而那个查询此前不存在。
    let db = TestDb::require().await;
    let repo = MoviePlotImageRepository::new(db.pool().clone());
    let images = sm_db::repo::ImageRepository::new(db.pool().clone());

    let pending_movie = seed_movie(&db).await;
    let done_movie = seed_movie(&db).await;
    let p_image = images
        .upsert(&sm_db::repo::NewImage {
            origin: format!("p/{}.jpg", n()),
        })
        .await
        .unwrap()
        .0;
    let d_image = images
        .upsert(&sm_db::repo::NewImage {
            origin: format!("d/{}.jpg", n()),
        })
        .await
        .unwrap()
        .0;

    repo.link(pending_movie, p_image).await.unwrap();
    repo.link(done_movie, d_image).await.unwrap();
    repo.set_index_status(
        done_movie,
        d_image,
        sm_db::catalog::asset::image_search_index_status::SUCCESS,
    )
    .await
    .unwrap();

    let pending = repo
        .list_by_index_status(
            sm_db::catalog::asset::image_search_index_status::PENDING,
            page(),
        )
        .await
        .unwrap();
    let ids: Vec<i32> = pending.items.iter().map(|r| r.image_id).collect();
    assert!(ids.contains(&p_image), "PENDING 的应被列出: {ids:?}");
    assert!(!ids.contains(&d_image), "已成功的不该出现");
    assert_eq!(
        pending.total as usize,
        pending.items.len(),
        "total 与 items 必须是同一个集合"
    );

    // 推进状态后它从 PENDING 里消失。
    repo.set_index_status(
        pending_movie,
        p_image,
        sm_db::catalog::asset::image_search_index_status::FAILED,
    )
    .await
    .unwrap();
    let failed = repo
        .list_by_index_status(
            sm_db::catalog::asset::image_search_index_status::FAILED,
            page(),
        )
        .await
        .unwrap();
    assert!(failed.items.iter().any(|r| r.image_id == p_image));

    // 解除关联。
    assert!(repo.unlink(pending_movie, p_image).await.unwrap());
    assert!(!repo.unlink(pending_movie, p_image).await.unwrap());
}

#[tokio::test]
async fn plot_images_go_away_with_either_side_of_the_link() {
    // 两个外键都是 `ON DELETE CASCADE`：影片没了或图没了，关联都不再成立。
    let db = TestDb::require().await;
    let repo = MoviePlotImageRepository::new(db.pool().clone());
    let images = sm_db::repo::ImageRepository::new(db.pool().clone());

    let movie = seed_movie(&db).await;
    let image = images
        .upsert(&sm_db::repo::NewImage {
            origin: format!("c/{}.jpg", n()),
        })
        .await
        .unwrap()
        .0;
    repo.link(movie, image).await.unwrap();
    assert_eq!(repo.list_by_movie(movie).await.unwrap().len(), 1);

    sqlx::query("DELETE FROM image WHERE id = $1")
        .bind(image)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        repo.list_by_movie(movie).await.unwrap().is_empty(),
        "图被删，关联随之消失（CASCADE）"
    );
}
// ================================================================ system_notification

#[tokio::test]
async fn the_same_event_notifies_once_but_plain_notifications_still_appear() {
    // `dedupe_key` 有唯一约束 —— 同一事件只产生一条通知。
    //
    // **但 NULL 不参与唯一约束**，所以没去重需求的普通通知（dedupe_key 为
    // None）照常产生多条。这两条语义必须同时成立，缺一个就会让「后台任务
    // 失败」要么刷屏、要么只报一次。
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());

    let mut deduped = notification("error", "任务失败");
    deduped.dedupe_key = Some("task-run-42".to_owned());

    let first = repo
        .notify(&deduped)
        .await
        .unwrap()
        .expect("首次应真的产生");
    assert!(first.is_deduplicated());
    assert_eq!(first.title, "任务失败");

    // 同一事件再触发一次（任务重试、worker 重启）—— 被去重。
    assert!(
        repo.notify(&deduped).await.unwrap().is_none(),
        "同一事件不该刷屏"
    );

    // 没有去重键的通知 —— 每次都产生。
    let plain = notification("error", "另一条");
    repo.notify(&plain).await.unwrap().expect("");
    repo.notify(&plain)
        .await
        .unwrap()
        .expect("无去重键应每次都产生");

    let listed = repo.list_by_category("error", page()).await.unwrap();
    assert_eq!(listed.total, 3, "1 条去重 + 2 条普通");
    assert_eq!(listed.total as usize, listed.items.len());
}

#[tokio::test]
async fn mark_read_writes_both_is_read_and_read_at() {
    // 只写 `is_read` 会留下不一致的状态 —— 模型的
    // `read_state_inconsistent` 正是为检测它而写的。所以仓储一起写。
    //
    // `WHERE is_read = false` 让重复标记返回 false，且**不**覆盖首次读的
    // 时刻：`read_at` 是「第一次被读到的时刻」。
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());

    let row = repo
        .notify(&notification("info", "系统消息"))
        .await
        .unwrap()
        .unwrap();
    assert!(!row.is_read);
    assert!(row.read_at.is_none());

    assert!(repo.mark_read(row.id).await.unwrap());
    let after = repo
        .list_by_category("info", page())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|r| r.id == row.id)
        .unwrap();
    assert!(after.is_read);
    assert!(after.read_at.is_some(), "read_at 必须一起写");
    assert!(!after.read_state_inconsistent(), "两个字段必须自相一致");

    // 重复标记：返回 false，且 read_at 不被改写。
    assert!(!repo.mark_read(row.id).await.unwrap());
    let again = repo
        .list_by_category("info", page())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|r| r.id == row.id)
        .unwrap();
    assert_eq!(again.read_at, after.read_at, "首次读的时刻不该被改写");

    // 已读的不出现在未读列表里。
    let unread = repo.list_unread(page()).await.unwrap();
    assert!(!unread.items.iter().any(|r| r.id == row.id));
}

#[tokio::test]
async fn resource_type_and_id_must_be_given_together() {
    // 单有一个无法定位资源。数据库不校验这个（两列是独立的可空列）。
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());

    let mut bad = notification("info", "半配置");
    bad.resource_type = Some("movie".to_owned());
    bad.resource_id = None;
    let err = repo.notify(&bad).await.expect_err("resource_id 缺失");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");

    let mut bad2 = notification("info", "半配置2");
    bad2.resource_id = Some(1);
    let err = repo.notify(&bad2).await.expect_err("resource_type 缺失");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");

    // 一起给出则可以。
    let mut good = notification("info", "完整");
    good.resource_type = Some("movie".to_owned());
    good.resource_id = Some(1);
    good.event_type = Some("movie.imported".to_owned());
    let row: SystemNotification = repo.notify(&good).await.unwrap().unwrap();
    assert!(row.uses_event_identity());
    assert!(!row.uses_legacy_relation());
}

// ================================================================ schema_migration

#[tokio::test]
async fn recording_a_migration_is_reentrant_and_keeps_the_first_applied_at() {
    // 启动流程**必须**能重入 —— 进程重启、并发启动多个实例都会走到
    // 「再记一次」。`ON CONFLICT DO NOTHING` 让它是无操作而不是报错：
    // 报错会把「第二次启动」变成启动失败。
    //
    // 也**不**更新 `applied_at`：那是「首次被应用的时刻」。
    let db = TestDb::require().await;
    let repo = SchemaMigrationRepository::new(db.pool().clone());
    let name = format!("2026_10_{:06}_add_index", n());

    assert!(repo.record(&name).await.unwrap(), "首次应真的记录");
    assert!(!repo.record(&name).await.unwrap(), "重入应无操作");
    assert!(repo.is_applied(&name).await.unwrap());

    let first_applied_at = repo
        .list(page())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|m| m.name == name)
        .unwrap()
        .applied_at;
    // 重入之后再读，时刻不变。
    repo.record(&name).await.unwrap();
    let again = repo
        .list(page())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|m| m.name == name)
        .unwrap()
        .applied_at;
    assert_eq!(again, first_applied_at, "首次应用的时刻不该被改写");

    // 迁移记录**没有时间戳**（全库第二张不继承 TimestampedMixin 的表）。
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'schema_migration' \
         ORDER BY column_name",
    )
    .bind(db.schema())
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(columns, vec!["applied_at", "id", "name"]);

    // 空白名被拒。
    assert!(repo.record("").await.is_err());
    assert!(repo.record("   ").await.is_err());
}

#[tokio::test]
async fn applied_names_are_ordered_by_when_they_were_applied() {
    let db = TestDb::require().await;
    let repo = SchemaMigrationRepository::new(db.pool().clone());
    let a = format!("m-a-{}", n());
    let b = format!("m-b-{}", n());

    repo.record(&a).await.unwrap();
    repo.record(&b).await.unwrap();
    repo.record("z-already-there").await.unwrap();

    let names = repo.applied_names().await.unwrap();
    let ia = names.iter().position(|x| x == &a).unwrap();
    let ib = names.iter().position(|x| x == &b).unwrap();
    assert!(ia < ib, "按应用时刻升序: {names:?}");

    // 未记录过的返回 false。
    assert!(!repo.is_applied("never-applied").await.unwrap());
}
