//! 时刻合集与片段合集 service 的集成测试。
//!
//! 两个 service 由同一个宏生成，所以测试也成对写 —— 但**断言各自独立的**
//! 错误码（`moment_collection_name_conflict` vs
//! `clip_collection_name_conflict`），因为那两个码是客户端分支的依据，
//! 宏参数传错的话测试要能抓到。
//!
//! 上游出处：`src/service/collections/moment_collection_service.py`（343 行）
//! 与 `clip_collection_service.py`（271 行）。

use sm_db::repo::{ImageRepository, MediaPointRepository, NewImage, NewMediaClip};
use sm_db::testing::TestDb;
use sm_service::collections::{ClipCollectionService, CollectionUpdate, MomentCollectionService};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[track_caller]
fn assert_error(err: &sm_service::error::ServiceError, status: u16, code: &str) {
    assert_eq!(
        (err.status, err.code()),
        (status, code),
        "状态码与错误码必须同时对上"
    );
}

/// 建一个 `media_point`。它是 `moment_collection_item.point_id` 的父行。
async fn seed_point(db: &TestDb) -> i32 {
    // media_point 需要一条 media + 一张 image，两条都走仓储。
    use sm_db::repo::{
        MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary,
        NewMovie,
    };
    let number = format!("MOM-{:06}", n());
    MovieRepository::new(db.pool().clone())
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
    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id;
    let media = MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id: library,
            file_name: "m.mp4".to_owned(),
            file_size_bytes: 1,
            movie_number: Some(number),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id;
    let image = ImageRepository::new(db.pool().clone())
        .upsert(&NewImage {
            origin: format!("pt/{}.jpg", n()),
        })
        .await
        .expect("upsert image")
        .0;
    MediaPointRepository::new(db.pool().clone())
        .insert(image, 0, Some(media), None, None)
        .await
        .expect("insert media_point")
        .id
}

/// 建一个 `media_clip`。它是 `clip_collection_item.clip_id` 的父行。
async fn seed_clip(db: &TestDb) -> i32 {
    use sm_db::repo::{
        MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary,
        NewMovie,
    };
    let number = format!("CLP-{:06}", n());
    MovieRepository::new(db.pool().clone())
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
    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id;
    let media = MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id: library,
            file_name: "c.mp4".to_owned(),
            file_size_bytes: 1,
            movie_number: Some(number),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id;
    sm_db::repo::playback::MediaClipRepository::new(db.pool().clone())
        .insert(&NewMediaClip {
            media_id: Some(media),
            movie_number: None,
            start_offset_seconds: 0,
            end_offset_seconds: 10,
            title: String::new(),
            file_path: "c.mp4".to_owned(),
            file_size_bytes: 1,
            duration_seconds: 10,
        })
        .await
        .expect("insert media_clip")
        .id
}

// ================================================================ 名称规则

#[tokio::test]
async fn each_table_enforces_its_own_uniqueness_with_its_own_code() {
    // 宏传错错误码的话客户端分支就错了 —— 客户端按 `code` 决定提示文案。
    //
    // 唯一性是**每张表各自**的（`name varchar(255) NOT NULL UNIQUE` 在
    // `moment_collection` 与 `clip_collection` 上各有一份），所以同名的
    // 时刻合集与片段合集**可以共存**。
    //
    // 第一版这个测试反过来断言跨表冲突，失败了。查下去发现是**假设错了**：
    // 表不同，约束也互不相干 —— 而代码行为是对的。
    let db = TestDb::require().await;
    let moments = MomentCollectionService::new(db.pool());
    let clips = ClipCollectionService::new(db.pool());

    let name = format!("shared-{}", n());
    moments.create(&name, None).await.unwrap();
    clips.create(&name, None).await.expect("跨表同名是合法的");

    // 各自表内仍然冲突，且用自己那张表的码。
    let err = moments
        .create(&name, None)
        .await
        .expect_err("同表重名应被拒");
    assert_error(&err, 409, "moment_collection_name_conflict");
    let err = clips.create(&name, None).await.expect_err("同表重名应被拒");
    assert_error(&err, 409, "clip_collection_name_conflict");
}

#[tokio::test]
async fn a_missing_collection_is_404_with_its_own_code() {
    let db = TestDb::require().await;
    let ghost = 9_999_999;

    let err = MomentCollectionService::new(db.pool())
        .delete(ghost)
        .await
        .expect_err("");
    assert_error(&err, 404, "moment_collection_not_found");
    assert_eq!(err.api.message, "Moment collection not found");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("collection_id"),
        Some(&serde_json::json!(ghost))
    );

    let err = ClipCollectionService::new(db.pool())
        .delete(ghost)
        .await
        .expect_err("");
    assert_error(&err, 404, "clip_collection_not_found");
    assert_eq!(err.api.message, "Clip collection not found");
}

#[tokio::test]
async fn an_empty_update_is_rejected_and_description_only_works() {
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc
        .create(&format!("upd-{}", n()), Some("原"))
        .await
        .unwrap();

    let err = svc
        .update(list.id, CollectionUpdate::default())
        .await
        .expect_err("");
    assert_error(&err, 422, "validation_error");
    assert_eq!(err.api.message, "At least one field must be provided");

    // 只改描述 —— 不该触发唯一性检查（否则撞到自己）。
    let updated = svc
        .update(
            list.id,
            CollectionUpdate {
                name: None,
                description: Some("  新  ".to_owned()),
            },
        )
        .await
        .expect("只改描述不该失败");
    assert_eq!(updated.description, "新");
    assert_eq!(updated.name, list.name);
}

// ================================================================ 成员

#[tokio::test]
async fn add_is_a_no_op_when_the_member_is_already_present() {
    // **反直觉的一条**：已在合集里就**无操作**，**不**改位置。
    //
    // 与 `playlist_movie` 的「重新加入只更新时间」不同 —— 那张表**没有**
    // `position`，顺序靠 id；这三张表**有** `position`，所以重复加入需要
    // 决定位置，而上游的选择是「不动」。
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("add-{}", n()), None).await.unwrap();

    let p1 = seed_point(&db).await;
    let p2 = seed_point(&db).await;
    svc.add(list.id, p1).await.unwrap();
    svc.add(list.id, p2).await.unwrap();
    let before = svc.list_members(list.id).await.unwrap();
    assert_eq!(
        before.iter().map(|m| m.position).collect::<Vec<_>>(),
        vec![0, 1]
    );

    // 重新加入第一个 —— 位置不变。
    svc.add(list.id, p1).await.unwrap();
    let after = svc.list_members(list.id).await.unwrap();
    assert_eq!(after.len(), 2, "重复加入不新增行");
    assert_eq!(
        after.iter().map(|m| m.position).collect::<Vec<_>>(),
        vec![0, 1],
        "重复加入不改变位置 —— 不是移到末尾"
    );
    assert_eq!(after[0].point_id, p1, "第一个仍是 p1");
}

#[tokio::test]
async fn add_rejects_a_point_that_does_not_exist() {
    // 校验的是 **`media_point` 行**存在，而不是 `moment_collection_item`
    // 行存在 —— 后者是关联表，它的行存在只说明关联存在。
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("bad-{}", n()), None).await.unwrap();

    let err = svc.add(list.id, 9_999_999).await.expect_err("");
    // 码用**实体名**（`media_point`），详情键用外键列名（`point_id`）。
    // 上游 `require_by_id(MediaPoint, id, "media_point",
    // error_details_key="point_id")` 就是这个分工 —— 此前实现把两者混用，
    // 产出 `point_id_not_found`，而这个断言把那个错误码照抄了下来。
    assert_error(&err, 404, "media_point_not_found");
    assert_eq!(err.api.message, "Media point not found");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("point_id"),
        Some(&serde_json::json!(9_999_999))
    );
}

#[tokio::test]
async fn remove_only_advances_the_parent_when_a_row_was_deleted() {
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("rm-{}", n()), None).await.unwrap();

    let p1 = seed_point(&db).await;
    let p2 = seed_point(&db).await;
    svc.add(list.id, p1).await.unwrap();
    svc.add(list.id, p2).await.unwrap();

    // 移一个不在合集里的 —— 成功，且不推进列表时间。
    let before = svc.list_members(list.id).await.unwrap().len();
    assert_eq!(before, 2);
    svc.remove(list.id, 9_999_999)
        .await
        .expect("移出不存在的成员应成功");
    assert_eq!(
        svc.list_members(list.id).await.unwrap().len(),
        2,
        "不应少一个"
    );

    // 真移一个。
    svc.remove(list.id, p1).await.unwrap();
    let left = svc.list_members(list.id).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].point_id, p2);
    // **不重排**留下的空位 —— 末尾成员的 position 保持 1。
    assert_eq!(left[0].position, 1, "unlink 不重排");
}

// ================================================================ set_members

#[tokio::test]
async fn set_members_dedupes_while_preserving_the_first_occurrence_order() {
    // **本批最重要的一条。** 上游用一个 `seen` 集合边走边滤，保留**首次
    // 出现**的位置 —— 不是 `sorted(set(...))`，那会把拖拽排序的顺序丢掉。
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("set-{}", n()), None).await.unwrap();

    let p1 = seed_point(&db).await;
    let p2 = seed_point(&db).await;
    let p3 = seed_point(&db).await;

    // 故意乱序 + 重复。
    svc.set_members(list.id, &[p3, p1, p3, p2, p1])
        .await
        .unwrap();

    let members = svc.list_members(list.id).await.unwrap();
    let order: Vec<i32> = members.iter().map(|m| m.point_id).collect();
    assert_eq!(order, vec![p3, p1, p2], "保留首次出现的顺序，且去重");
    let positions: Vec<i32> = members.iter().map(|m| m.position).collect();
    assert_eq!(positions, vec![0, 1, 2], "位置被重排为 0 起连续");
}

#[tokio::test]
async fn set_members_with_an_empty_slice_clears_the_collection() {
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("clr-{}", n()), None).await.unwrap();
    svc.add(list.id, seed_point(&db).await).await.unwrap();
    assert_eq!(svc.list_members(list.id).await.unwrap().len(), 1);

    svc.set_members(list.id, &[]).await.unwrap();
    assert!(svc.list_members(list.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn set_members_validates_all_before_writing_anything() {
    // 全部校验通过再写 —— 否则「清空之后才发现有一个 point 不存在」，
    // 那时合集已经空了。
    let db = TestDb::require().await;
    let svc = MomentCollectionService::new(db.pool());
    let list = svc.create(&format!("atomic-{}", n()), None).await.unwrap();
    let good = seed_point(&db).await;
    svc.set_members(list.id, &[good]).await.unwrap();

    let err = svc
        .set_members(list.id, &[9_999_999, good])
        .await
        .expect_err("不存在的 point 应被拒");
    assert_error(&err, 404, "media_point_not_found");

    // 关键：失败后**原来的成员还在**。
    let members = svc.list_members(list.id).await.unwrap();
    assert_eq!(members.len(), 1, "校验失败不应清空已有成员");
    assert_eq!(members[0].point_id, good);
}

// ================================================================ clip 同形

#[tokio::test]
async fn clip_collection_has_the_same_shape_with_its_own_codes() {
    let db = TestDb::require().await;
    let svc = ClipCollectionService::new(db.pool());
    let list = svc.create(&format!("clip-{}", n()), None).await.unwrap();

    let c1 = seed_clip(&db).await;
    let c2 = seed_clip(&db).await;
    let c3 = seed_clip(&db).await;

    svc.set_members(list.id, &[c2, c1, c2, c3]).await.unwrap();
    let members = svc.list_members(list.id).await.unwrap();
    assert_eq!(
        members.iter().map(|m| m.clip_id).collect::<Vec<_>>(),
        vec![c2, c1, c3],
        "去重且保留首次顺序"
    );

    let err = svc.add(list.id, 9_999_999).await.expect_err("");
    assert_error(&err, 404, "media_clip_not_found");
    assert_eq!(err.api.message, "Media clip not found");

    // `name varchar(255)`，所以 256 字符会违反约束。
    //
    // 那一层**不**在 service：用户侧的两个合集 service 都不校验名称长度
    //（上游 `_normalize_name` 也不校验），只有插件侧的
    // `plugin_collection_service` 才查 255 上限。所以这里断言的是
    // 「数据库会拦住」，而不是「service 预先拒绝」—— 两者是不同层的职责。
    let too_long = "x".repeat(256);
    let err = svc
        .create(&too_long, None)
        .await
        .expect_err("超长名称应被数据库拒绝");
    // 数据库约束错误映射成 500 —— 它不是调用方能修正的输入问题。
    assert_error(&err, 500, "internal_error");
}
