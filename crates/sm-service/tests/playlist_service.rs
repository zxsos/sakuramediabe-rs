//! `PlaylistService` 的集成测试。
//!
//! # 这一批定下的测试形态
//!
//! service 层测的是**业务规则**，不是「仓储能不能读写」—— 那是 `sm-db` 的
//! 职责，在那边已经测过 356 个。所以这里每个测试对应上游的一条规则，且
//! **断言状态码与错误码两者**，而不只是「失败了」。
//!
//! 理由：客户端按 `code` 分支、按状态码决定重试。两者错一个就是契约破坏，
//! 而只断言「返回了 Err」的话，422 变 409 也能通过。
//!
//! 上游出处：`src/service/collections/playlist_service.py`。

use sm_db::repo::{MovieRepository, NewMovie};
use sm_db::testing::TestDb;
use sm_service::collections::{PlaylistService, PlaylistUpdate};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 建一部影片，返回 `(id, movie_number)`。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("PLS-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
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
    (movie.id, number)
}

/// 断言一个错误是「状态码 + 错误码」这一对。
#[track_caller]
fn assert_error(err: &sm_service::error::ServiceError, status: u16, code: &str) {
    assert_eq!(
        (err.status, err.code()),
        (status, code),
        "状态码与错误码必须同时对上：客户端按 code 分支、按 status 决定重试"
    );
}

// ================================================================ 名称规则

#[tokio::test]
async fn a_blank_name_is_a_422_not_a_409() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    for blank in ["", "   ", "\t\n"] {
        let err = svc.create(blank, None).await.expect_err("空名应被拒");
        assert_error(&err, 422, "validation_error");
        assert_eq!(err.api.message, "Playlist name cannot be empty");
    }
}

#[tokio::test]
async fn a_duplicate_name_is_a_conflict_carrying_the_name() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let name = format!("dup-{}", n());

    svc.create(&name, None).await.unwrap();
    let err = svc.create(&name, None).await.expect_err("重名应被拒");
    assert_error(&err, 409, "playlist_name_conflict");
    // details 必须带上是哪个名字 —— 客户端据此定位到表单字段。
    assert_eq!(
        err.api.details.as_ref().unwrap().get("name"),
        Some(&serde_json::json!(name.as_str()))
    );
}

#[tokio::test]
async fn the_reserved_name_is_rejected_before_the_uniqueness_check() {
    // 顺序有意义：保留名冲突返回 `playlist_reserved_name`，而不是
    // `playlist_name_conflict`。客户端据此区分「这个名字你不能占」与
    // 「这个名字已被别人占了」。
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());

    let err = svc
        .create(sm_db::collections::RECENTLY_PLAYED_PLAYLIST_NAME, None)
        .await
        .expect_err("系统保留名不可占用");
    assert_error(&err, 409, "playlist_reserved_name");
    assert_eq!(err.api.message, "Playlist name is reserved");

    // 先把系统列表建出来，再试一次 —— 仍然是保留名错误，不是冲突错误。
    svc.recently_played().await.unwrap();
    let err = svc
        .create(sm_db::collections::RECENTLY_PLAYED_PLAYLIST_NAME, None)
        .await
        .expect_err("仍应是保留名错误");
    assert_error(&err, 409, "playlist_reserved_name");
}
// ================================================================ 系统列表保护

#[tokio::test]
async fn a_system_playlist_cannot_be_renamed_deleted_or_given_members() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let system = svc.recently_played().await.unwrap();
    assert!(system.is_system(), "它必须是系统列表，否则本测试无意义");

    // 改名
    let err = svc
        .update(
            system.id,
            PlaylistUpdate {
                name: Some(format!("hijack-{}", n())),
                description: None,
            },
        )
        .await
        .expect_err("系统列表不可改名");
    assert_error(&err, 409, "playlist_managed_by_system");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("playlist_id"),
        Some(&serde_json::json!(system.id))
    );

    // 删除
    let err = svc.delete(system.id).await.expect_err("系统列表不可删");
    assert_error(&err, 409, "playlist_managed_by_system");

    // 加成员 —— 这一条最容易漏：用户不能手动改「最近播放」，但播放行为
    // 本身要能写进去（走 touch_recently_played）。
    let (_, movie_number) = seed_movie(&db).await;
    let err = svc
        .add_movie(system.id, &movie_number)
        .await
        .expect_err("系统列表不可手动加成员");
    assert_error(&err, 409, "playlist_managed_by_system");

    // 移成员同理。
    let err = svc
        .remove_movie(system.id, &movie_number)
        .await
        .expect_err("系统列表不可手动移成员");
    assert_error(&err, 409, "playlist_managed_by_system");
}

// ================================================================ 更新

#[tokio::test]
async fn an_empty_update_is_rejected() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let list = svc.create(&format!("empty-{}", n()), None).await.unwrap();

    let err = svc
        .update(list.id, PlaylistUpdate::default())
        .await
        .expect_err("至少要给一个字段");
    assert_error(&err, 422, "validation_error");
    assert_eq!(err.api.message, "At least one field must be provided");
}

#[tokio::test]
async fn changing_only_the_description_does_not_trip_the_uniqueness_check() {
    // **最容易搞反的一条规则。** 上游写的是
    // `if name != playlist.name: _ensure_name_available(...)` ——
    // 少了那个判断，「只改描述」会因为撞到**自己**而失败。
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let name = format!("desc-only-{}", n());
    let list = svc.create(&name, Some("原描述")).await.unwrap();

    let updated = svc
        .update(
            list.id,
            PlaylistUpdate {
                name: None,
                description: Some("  新描述  ".to_owned()),
            },
        )
        .await
        .expect("只改描述不该触发唯一性检查");
    assert_eq!(updated.description, "新描述", "描述被 trim");
    assert_eq!(updated.name, name, "名字保持原样");
}

#[tokio::test]
async fn sending_the_same_name_again_is_allowed_but_a_taken_name_is_not() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let mine = format!("mine-{}", n());
    let theirs = format!("theirs-{}", n());
    let list = svc.create(&mine, None).await.unwrap();
    svc.create(&theirs, None).await.unwrap();

    // 送同一个名字（只有 trim 差异）—— 允许。
    svc.update(
        list.id,
        PlaylistUpdate {
            name: Some(format!("  {mine}  ")),
            description: None,
        },
    )
    .await
    .expect("送自己的同一个名字应被允许");

    // 改成别人的名字 —— 冲突，且**排除自己**的逻辑要生效。
    let err = svc
        .update(
            list.id,
            PlaylistUpdate {
                name: Some(theirs.clone()),
                description: None,
            },
        )
        .await
        .expect_err("改成别人的名字应冲突");
    assert_error(&err, 409, "playlist_name_conflict");
}

#[tokio::test]
async fn a_missing_playlist_is_a_404_with_the_id_in_details() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let ghost = 9_999_999;

    for err in [
        svc.delete(ghost).await.expect_err(""),
        svc.update(
            ghost,
            PlaylistUpdate {
                name: Some("x".to_owned()),
                description: None,
            },
        )
        .await
        .expect_err(""),
        svc.add_movie(ghost, "whatever").await.expect_err(""),
        svc.remove_movie(ghost, "whatever").await.expect_err(""),
    ] {
        assert_error(&err, 404, "playlist_not_found");
        assert_eq!(
            err.api.details.as_ref().unwrap().get("playlist_id"),
            Some(&serde_json::json!(ghost)),
            "details 必须带 playlist_id"
        );
    }
}
// ================================================================ 成员

#[tokio::test]
async fn adding_an_unknown_movie_is_a_404_with_the_number_in_details() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let list = svc.create(&format!("addm-{}", n()), None).await.unwrap();

    let err = svc
        .add_movie(list.id, "NO-SUCH-NUMBER")
        .await
        .expect_err("影片不存在应 404");
    assert_error(&err, 404, "movie_not_found");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("movie_number"),
        Some(&serde_json::json!("NO-SUCH-NUMBER")),
        "details 键是 movie_number 而不是 movie_id —— 与上游一致"
    );
}

#[tokio::test]
async fn removing_a_movie_that_is_not_in_the_list_succeeds_silently() {
    // **反直觉的一条**：影片不存在时**静默返回**，不是 404。
    //
    // 理由：列表里本来就没有它，目标状态已经达成。上游同样直接 `return`。
    // 判成错误会让「幂等地确保它不在列表里」这种调用无法写。
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let list = svc.create(&format!("silent-{}", n()), None).await.unwrap();

    // 影片根本不存在于 movie 表。
    svc.remove_movie(list.id, "NO-SUCH-NUMBER")
        .await
        .expect("影片不存在应静默成功");

    // 影片存在但不在列表里 —— 同样成功。
    let (_, number) = seed_movie(&db).await;
    svc.remove_movie(list.id, &number)
        .await
        .expect("不在列表里应静默成功");

    // 系统列表的保护**优先于**这个宽容规则 —— 改系统列表仍然 409。
    let system = svc.recently_played().await.unwrap();
    let err = svc
        .remove_movie(system.id, &number)
        .await
        .expect_err("系统列表仍受保护");
    assert_error(&err, 409, "playlist_managed_by_system");
}

#[tokio::test]
async fn re_adding_a_movie_updates_it_without_moving_it_in_the_order() {
    // **反直觉的一条**：`playlist_movie` **没有 `position` 列**，顺序靠
    // `id`。所以「重新加入」只更新 `updated_at`，**不会**把影片挪到末尾。
    //
    // 这与 `moment_collection` / `clip_collection` 不同 —— 那两个有
    // `position`，重复加入需要决定位置。
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let list = svc.create(&format!("order-{}", n()), None).await.unwrap();

    let (id_a, number_a) = seed_movie(&db).await;
    let (_id_b, number_b) = seed_movie(&db).await;

    svc.add_movie(list.id, &number_a).await.unwrap();
    svc.add_movie(list.id, &number_b).await.unwrap();

    // 重新加入第一部 —— 不改变顺序。
    svc.add_movie(list.id, &number_a).await.unwrap();

    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(list.id)
        .await
        .unwrap();
    let order: Vec<i32> = members.iter().map(|m| m.movie_id).collect();
    assert_eq!(order, vec![id_a, members[1].movie_id], "顺序仍是加入先后");
    assert_eq!(members.len(), 2, "重复加入不新增行");
}

// ================================================================ 最近播放

#[tokio::test]
async fn recently_played_is_a_singleton_created_on_first_use() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());

    let first = svc.recently_played().await.unwrap();
    assert_eq!(
        first.kind,
        sm_db::collections::PLAYLIST_KIND_RECENTLY_PLAYED
    );
    assert_eq!(
        first.name,
        sm_db::collections::RECENTLY_PLAYED_PLAYLIST_NAME
    );
    assert!(first.is_system());

    // 再取一次 —— 同一个 id，不是新建。
    let second = svc.recently_played().await.unwrap();
    assert_eq!(first.id, second.id, "系统最近播放是单例");

    // 全表只有一个 recently_played。
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlist WHERE kind = $1")
        .bind(sm_db::collections::PLAYLIST_KIND_RECENTLY_PLAYED)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1, "不允许出现第二个实例");
}

#[tokio::test]
async fn touch_recently_played_writes_into_the_system_list() {
    // 用户不能手动改「最近播放」，但**播放行为本身**要能写进去 ——
    // 那正是 `_require_custom_playlist` 存在的原因。
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let (movie_id, _) = seed_movie(&db).await;

    svc.touch_recently_played(movie_id).await.unwrap();

    let system = svc.recently_played().await.unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(system.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].movie_id, movie_id);

    // 重复 touch 不新增行。
    svc.touch_recently_played(movie_id).await.unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(system.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 1, "重复记录不新增行");
}
