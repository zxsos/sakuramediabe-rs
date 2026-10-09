//! `VideoItemService` 与 `VideoCollectionService` 的集成测试。
//!
//! # 这一批定下的测试形态
//!
//! 沿用 `playlist_service.rs` 的规矩：每个测试对应上游一条规则，且
//! **同时断言状态码与错误码**，断言 `details` 里那个「客户端要拿来高亮
//! 表单字段的键」。只断言「返回了 Err」的话，422 变 409 也能通过。
//!
//! # 为什么 seed 帮手这么啰嗦
//!
//! `media` 上有一条 CHECK：`(movie_number IS NULL) <> (video_item_id IS NULL)` ——
//! 恰好其一非空。而 `media_thumbnail` 同时外键到 `media` 与 `image`，
//! `media` 又外键到 `media_library`。所以「建一个缩略图」要建四张表。
//! `sm_db::testing::db` 的文档记着这个坑：上一轮 `seed_media` 硬编码
//! `library_id: 1` 却从不建那一行，18 个用例全撞外键，而测试**因为会跳过**
//! 一直显示为通过。
//!
//! 上游出处：`src/service/videos/video_item_service.py` 与
//! `video_collection_service.py`。

use sm_db::playback::media::image_search_index_status;
use sm_db::repo::{
    ImageRepository, MediaLibraryRepository, MediaRepository, MediaThumbnailRepository, NewImage,
    NewMedia, NewMediaLibrary, NewVideoCollection, NewVideoItem, VideoItemRepository,
};
use sm_db::testing::TestDb;
use sm_service::videos::{
    Added, Field, VideoCollectionService, VideoCollectionUpdate, VideoItemCreate, VideoItemService,
    VideoItemUpdate,
};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
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

// ================================================================ seed

async fn seed_video(db: &TestDb) -> i32 {
    VideoItemRepository::new(db.pool().clone())
        .insert(&NewVideoItem {
            title: format!("VID-{:06}", n()),
            summary: String::new(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .expect("insert video_item")
        .id
}

/// 建一个媒体库，返回 id。`media.library_id` 的外键目标。
async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{:06}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

/// 建一张图片，返回 id。`media_thumbnail.image_id` 的外键目标。
async fn seed_image(db: &TestDb) -> i32 {
    ImageRepository::new(db.pool().clone())
        .upsert(&NewImage {
            // `origin` 全局唯一，所以每次都要新的。
            origin: format!("videos/{:06}/cover/0.webp", n()),
        })
        .await
        .expect("insert image")
        .0
}

/// 建一条挂到非 JAV 条目下的媒体，返回 id。
async fn seed_media(db: &TestDb, video_item_id: i32) -> i32 {
    let library_id = seed_library(db).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("v-{:06}.mp4", n()),
            file_size_bytes: 1024,
            // XOR 约束：给了 video_item_id 就必须不给 movie_number。
            movie_number: None,
            video_item_id: Some(video_item_id),
            storage_ref: None,
            resolution: Some("1920x1080".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 120,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id
}

/// 给某条媒体建一张缩略图，返回缩略图 id。
async fn seed_thumbnail(db: &TestDb, media_id: i32) -> i32 {
    let image_id = seed_image(db).await;
    MediaThumbnailRepository::new(db.pool().clone())
        .upsert(media_id, 0, image_id, image_search_index_status::SKIPPED)
        .await
        .expect("upsert media_thumbnail")
        .id
}

/// 建一个合集，返回 id。
async fn seed_collection(db: &TestDb) -> i32 {
    sm_db::repo::VideoCollectionRepository::new(db.pool().clone())
        .insert(&NewVideoCollection {
            name: format!("COL-{:06}", n()),
            description: String::new(),
        })
        .await
        .expect("insert video_collection")
        .id
}

// ================================================================ 条目：存在性

#[tokio::test]
async fn a_missing_video_is_404_carrying_the_upstream_details_key() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());

    for err in [
        svc.get(999_999).await.unwrap_err(),
        svc.delete(999_999).await.unwrap_err(),
        svc.update(999_999, VideoItemUpdate::default())
            .await
            .unwrap_err(),
    ] {
        assert_error(&err, 404, "video_item_not_found");
        assert_eq!(err.api.message, "Video item not found");
        // 键名由上游 `require_by_id(VideoItem, id, "video_item")` 生成，
        // 客户端按它做「定位到该条目」。
        assert_eq!(
            err.api.details.as_ref().unwrap().get("video_item_id"),
            Some(&serde_json::json!(999_999))
        );
    }
}

// ================================================================ 条目：标题

#[tokio::test]
async fn a_blank_title_is_a_422_on_create() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());

    for blank in ["", "   ", "\t\n"] {
        let err = svc
            .create(&VideoItemCreate {
                title: blank.to_owned(),
                ..Default::default()
            })
            .await
            .expect_err("空白标题应被拒");
        assert_error(&err, 422, "validation_error");
        assert_eq!(err.api.message, "title cannot be blank");
    }
}

#[tokio::test]
async fn a_title_is_stripped_on_create() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let title = format!("  带空格  {:06}", n());

    let created = svc
        .create(&VideoItemCreate {
            title: title.clone(),
            ..Default::default()
        })
        .await
        .expect("create");
    assert_eq!(created.title, title.trim());
}

// ================================================================ 条目：更新

#[tokio::test]
async fn an_empty_update_is_422() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;

    let err = svc
        .update(id, VideoItemUpdate::default())
        .await
        .expect_err("空更新应被拒");
    assert_error(&err, 422, "validation_error");
    assert_eq!(err.api.message, "At least one field must be provided");
}

#[tokio::test]
async fn a_null_only_update_is_accepted_and_only_bumps_the_timestamp() {
    // 上游 `{"title": null}` 不是空更新：它过了检查、什么都没改，
    // 只推进 `updated_at`。这与「空更新 422」是两件事。
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;
    let before = svc.get(id).await.unwrap();

    let updated = svc
        .update(
            id,
            VideoItemUpdate {
                title: Field::Null,
                summary: Field::Null,
                ..Default::default()
            },
        )
        .await
        .expect("只带 null 的更新应当成功");

    assert_eq!(updated.title, before.title, "null 的标题必须被忽略");
    assert_eq!(updated.summary, before.summary);
    assert!(
        updated.updated_at > before.updated_at,
        "仍然要推进 updated_at（上游无条件 `video.updated_at = utc_now_for_db()`）"
    );
}

#[tokio::test]
async fn release_date_null_clears_the_column_while_title_null_does_not() {
    // 三个字段对显式 null 的处理各不相同：release_date 清空，title/summary
    // 忽略。合成一个 `Option<T>` 会让「清空」与「不动」无法区分。
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;

    let set = svc
        .update(
            id,
            VideoItemUpdate {
                release_date: Field::Value(
                    chrono::NaiveDate::from_ymd_opt(2024, 5, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(set.release_date.is_some());

    let cleared = svc
        .update(
            id,
            VideoItemUpdate {
                release_date: Field::Null,
                title: Field::Null,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cleared.release_date, None,
        "显式 null 必须清空 release_date"
    );
}

#[tokio::test]
async fn an_empty_summary_is_a_legitimate_value_not_a_rejection() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;

    let updated = svc
        .update(
            id,
            VideoItemUpdate {
                summary: Field::Value("  有简介  ".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary, "有简介", "summary 会被 strip()");

    let cleared = svc
        .update(
            id,
            VideoItemUpdate {
                summary: Field::Value("   ".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cleared.summary, "", "纯空白的简介归一成空串，而不是被拒");
}

// ================================================================ 条目：封面

#[tokio::test]
async fn an_explicit_null_cover_thumbnail_is_422_not_a_clear_operation() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;

    let err = svc
        .update(
            id,
            VideoItemUpdate {
                cover_thumbnail_id: Field::Null,
                ..Default::default()
            },
        )
        .await
        .expect_err("显式 null 封面应被拒");
    assert_error(&err, 422, "video_cover_thumbnail_required");
    assert_eq!(err.api.message, "cover_thumbnail_id cannot be null");
}

#[tokio::test]
async fn a_cover_thumbnail_of_another_video_is_404_with_both_ids() {
    // 上游那条查询把「缩略图存在」与「缩略图属于本条目」合在同一个 WHERE，
    // 命中不了就统一 404 —— 所以「存在但不属于」不是 403 也不是 422。
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let mine = seed_video(&db).await;
    let theirs = seed_video(&db).await;
    let their_thumbnail = seed_thumbnail(&db, seed_media(&db, theirs).await).await;

    let err = svc
        .update(
            mine,
            VideoItemUpdate {
                cover_thumbnail_id: Field::Value(their_thumbnail),
                ..Default::default()
            },
        )
        .await
        .expect_err("别人的缩略图不可用");
    assert_error(&err, 404, "video_cover_thumbnail_not_found");
    let details = err.api.details.as_ref().unwrap();
    assert_eq!(details.get("video_id"), Some(&serde_json::json!(mine)));
    assert_eq!(
        details.get("thumbnail_id"),
        Some(&serde_json::json!(their_thumbnail))
    );
}

#[tokio::test]
async fn a_missing_cover_thumbnail_is_the_same_404() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;

    let err = svc
        .update(
            id,
            VideoItemUpdate {
                cover_thumbnail_id: Field::Value(999_999),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_error(&err, 404, "video_cover_thumbnail_not_found");
}

#[tokio::test]
async fn a_cover_thumbnail_of_this_video_is_applied() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;
    let media_id = seed_media(&db, id).await;
    let thumbnail_id = seed_thumbnail(&db, media_id).await;
    let expected_image = MediaThumbnailRepository::new(db.pool().clone())
        .find_by_id(thumbnail_id)
        .await
        .unwrap()
        .expect("刚建的缩略图应当存在")
        .image_id;

    let updated = svc
        .update(
            id,
            VideoItemUpdate {
                cover_thumbnail_id: Field::Value(thumbnail_id),
                ..Default::default()
            },
        )
        .await
        .expect("本条目自己的缩略图应可设");
    assert_eq!(updated.cover_image_id, Some(expected_image));
}

#[tokio::test]
async fn a_rejected_cover_leaves_the_title_untouched() {
    // 封面校验在字段写入之前：非法封面不应该让同一次请求里的标题改动生效。
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;
    let before = svc.get(id).await.unwrap();

    let _ = svc
        .update(
            id,
            VideoItemUpdate {
                title: Field::Value("新标题".to_owned()),
                cover_thumbnail_id: Field::Value(999_999),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert_eq!(svc.get(id).await.unwrap().title, before.title);
}

// ================================================================ 条目：删除

#[tokio::test]
async fn deleting_a_video_takes_its_media_rows_with_it() {
    // `media.video_item_id` 是 CASCADE。上游还要清磁盘文件与缩略图产物，
    // 那是另一切片的事 —— 这里是「库里的行确实一起没了」。
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;
    let _media_id = seed_media(&db, id).await;

    svc.delete(id).await.expect("delete");
    assert_eq!(svc.get(id).await.unwrap_err().status, 404);
    assert_eq!(
        svc.list_media(id).await.unwrap_err().status,
        404,
        "条目没了，媒体查询也该被 404 挡住"
    );
    // 直接问仓储，绕开 service 的存在性检查 —— 这才是 CASCADE 的证据。
    assert!(
        VideoItemRepository::new(db.pool().clone())
            .list_media(id)
            .await
            .unwrap()
            .is_empty(),
        "媒体行必须随条目一起消失（外键 CASCADE）"
    );
}

#[tokio::test]
async fn media_of_a_video_are_listed_in_id_order() {
    let db = TestDb::require().await;
    let svc = VideoItemService::new(db.pool());
    let id = seed_video(&db).await;
    let first = seed_media(&db, id).await;
    let second = seed_media(&db, id).await;

    let ids: Vec<i32> = svc
        .list_media(id)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, vec![first.min(second), first.max(second)]);
}

// ================================================================ 合集：存在性与名称

#[tokio::test]
async fn a_missing_collection_is_404_with_the_collection_id_key() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());

    for err in [
        svc.delete(999_999).await.unwrap_err(),
        svc.list_items(999_999).await.unwrap_err(),
        svc.update(999_999, VideoCollectionUpdate::default())
            .await
            .unwrap_err(),
    ] {
        assert_error(&err, 404, "video_collection_not_found");
        assert_eq!(err.api.message, "Video collection not found");
        // 上游显式传了 error_details_key="collection_id"，
        // 所以键不是 `video_collection_id`。
        assert_eq!(
            err.api.details.as_ref().unwrap().get("collection_id"),
            Some(&serde_json::json!(999_999))
        );
    }
}

#[tokio::test]
async fn a_duplicate_collection_name_is_409_carrying_the_name() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let name = format!("dup-{:06}", n());

    svc.create(&name, None).await.unwrap();
    let err = svc.create(&name, None).await.expect_err("重名应被拒");
    assert_error(&err, 409, "video_collection_name_conflict");
    assert_eq!(err.api.message, "Video collection name already exists");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("name"),
        Some(&serde_json::json!(name.as_str()))
    );
}

#[tokio::test]
async fn a_blank_collection_name_is_422() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    for blank in ["", "  "] {
        let err = svc.create(blank, None).await.unwrap_err();
        assert_error(&err, 422, "validation_error");
        assert_eq!(err.api.message, "name cannot be blank");
    }
}

#[tokio::test]
async fn renaming_to_the_current_name_skips_the_uniqueness_check() {
    // 少了这个判断，「只改描述不改名字」会撞到自己而失败。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let id = seed_collection(&db).await;
    let current = svc.get(id).await.unwrap().name;

    let updated = svc
        .update(
            id,
            VideoCollectionUpdate {
                name: Field::Value(current.clone()),
                description: Field::Value("新简介".to_owned()),
            },
        )
        .await
        .expect("同名更新应当通过");
    assert_eq!(updated.name, current);
    assert_eq!(updated.description, "新简介");
}

#[tokio::test]
async fn renaming_to_another_collections_name_is_409() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let taken = format!("taken-{:06}", n());
    let mine = seed_collection(&db).await;
    svc.create(&taken, None).await.unwrap();

    let err = svc
        .update(
            mine,
            VideoCollectionUpdate {
                name: Field::Value(taken),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_error(&err, 409, "video_collection_name_conflict");
}

#[tokio::test]
async fn an_empty_collection_update_is_422() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let id = seed_collection(&db).await;
    let err = svc
        .update(id, VideoCollectionUpdate::default())
        .await
        .unwrap_err();
    assert_error(&err, 422, "validation_error");
    assert_eq!(err.api.message, "At least one field must be provided");
}

// ================================================================ 合集：成员

#[tokio::test]
async fn adding_an_unknown_video_is_404() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;

    let err = svc.add_item(collection, 999_999).await.unwrap_err();
    assert_error(&err, 404, "video_item_not_found");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("video_item_id"),
        Some(&serde_json::json!(999_999))
    );
}

#[tokio::test]
async fn adding_the_same_video_twice_is_idempotent_and_keeps_the_first_position() {
    // 上游「先查后插，撞唯一约束就 return」。重复加入**不是** 409：
    // UI 上连点两下是常态。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let a = seed_video(&db).await;
    let b = seed_video(&db).await;

    let Added::Added {
        position: first, ..
    } = svc.add_item(collection, a).await.unwrap()
    else {
        panic!("首次加入应当写入");
    };
    let Added::Added {
        position: second, ..
    } = svc.add_item(collection, b).await.unwrap()
    else {
        panic!("第二个成员应当写入");
    };
    assert_eq!((first, second), (0, 1), "位置从 0 开始连续递增");

    let again = svc.add_item(collection, a).await.unwrap();
    assert_eq!(again, Added::AlreadyPresent);

    let items = svc.list_items(collection).await.unwrap();
    assert_eq!(items.len(), 2, "重复加入不能产生第二行");
    assert_eq!(
        items[0].id, a as i32,
        "位置不能变 —— 上游是 `return`，不是「移到末尾」"
    );
    assert_eq!(items[0].position, 0);
}

#[tokio::test]
async fn remove_item_takes_the_link_id_not_the_video_id() {
    // 两个 id 空间相同、外观相同，传错**不会报错**，只会删掉错的行。
    // 所以名字与文档是唯一防线 —— 这个测试就是那道防线。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let video = seed_video(&db).await;
    svc.add_item(collection, video).await.unwrap();

    let items = svc.list_items(collection).await.unwrap();
    let link_id = items[0].id;
    let member_id = items[0].video_item_id;
    if link_id == member_id {
        // id 撞车时这个测试证明不了什么，跳过而不是给出假绿。
        eprintln!("SKIP: link_id 与 video_item_id 恰好相同，无法区分");
        return;
    }

    // 传成员 id：静默成功，但什么都没删 —— 与上游一致。
    svc.remove_item(collection, member_id).await.unwrap();
    assert_eq!(
        svc.list_items(collection).await.unwrap().len(),
        1,
        "按成员 id 调 remove_item 不应删掉关联行"
    );

    // 传关联行 id：真删。
    svc.remove_item(collection, link_id).await.unwrap();
    assert!(svc.list_items(collection).await.unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_missing_member_does_not_touch_the_collection() {
    // 否则「移除一个不在合集里的视频」会把合集顶到「最近活跃」最前面。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let video = seed_video(&db).await;
    svc.add_item(collection, video).await.unwrap();
    let before = sm_db::repo::VideoCollectionRepository::new(db.pool().clone())
        .find_by_id(collection)
        .await
        .unwrap()
        .unwrap();

    svc.remove_item(collection, 999_999).await.unwrap();
    svc.remove_items_by_video_ids(collection, &[999_999])
        .await
        .unwrap();

    let after = sm_db::repo::VideoCollectionRepository::new(db.pool().clone())
        .find_by_id(collection)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.updated_at, before.updated_at,
        "没删掉行就不能推进 updated_at"
    );
}

#[tokio::test]
async fn remove_items_by_video_ids_removes_only_the_listed_ones() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let a = seed_video(&db).await;
    let b = seed_video(&db).await;
    let c = seed_video(&db).await;
    for v in [a, b, c] {
        svc.add_item(collection, v).await.unwrap();
    }

    // 重复 id 不必去重，也不该导致重复删除。
    svc.remove_items_by_video_ids(collection, &[a, a, 999_999])
        .await
        .unwrap();

    let left = svc.list_items(collection).await.unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.iter().all(|l| l.video_item_id != a));
}

#[tokio::test]
async fn deleting_a_collection_keeps_its_videos() {
    // 成员随外键 CASCADE 消失，但视频条目本身必须保留。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let video = seed_video(&db).await;
    svc.add_item(collection, video).await.unwrap();

    svc.delete(collection).await.unwrap();
    assert_eq!(svc.list_items(collection).await.unwrap_err().status, 404);
    assert_eq!(
        VideoItemService::new(db.pool())
            .get(video)
            .await
            .unwrap()
            .id,
        video,
        "视频条目必须还在"
    );
}

#[tokio::test]
async fn clear_items_empties_members_but_keeps_the_collection() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    for _ in 0..2 {
        let v = seed_video(&db).await;
        svc.add_item(collection, v).await.unwrap();
    }

    assert_eq!(svc.clear_items(collection).await.unwrap(), 2);
    assert!(svc.list_items(collection).await.unwrap().is_empty());
    // 合集本身还在。
    assert!(svc
        .add_item(collection, seed_video(&db).await)
        .await
        .is_ok());
}

// ================================================================ 合集：重排

#[tokio::test]
async fn reorder_requires_exactly_full_coverage() {
    // 宽松版（「只处理给出的那几个」）会让漏排的成员停在旧位置，
    // 而前端已经按新顺序播了。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let mut link_ids = Vec::new();
    for _ in 0..3 {
        let v = seed_video(&db).await;
        svc.add_item(collection, v).await.unwrap();
    }
    for item in svc.list_items(collection).await.unwrap() {
        link_ids.push(item.id);
    }

    for bad in [
        link_ids[0..2].to_vec(),                              // 漏了一个
        vec![link_ids[0], link_ids[1], link_ids[2], 999_999], // 多了一个
        vec![link_ids[0]],                                    // 只有一个
    ] {
        let err = svc.reorder(collection, &bad).await.unwrap_err();
        assert_error(&err, 422, "invalid_collection_reorder");
        assert_eq!(
            err.api.message,
            "ordered_item_ids must cover exactly all collection items"
        );
        assert_eq!(
            err.api.details.as_ref().unwrap().get("collection_id"),
            Some(&serde_json::json!(collection))
        );
    }
}

#[tokio::test]
async fn reorder_rejects_an_empty_list() {
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    let v = seed_video(&db).await;
    svc.add_item(collection, v).await.unwrap();

    // 上游 `ordered_item_ids: list[int] = Field(min_length=1)`，空列表在
    // pydantic 层就被拒，到不了 service。
    let err = svc.reorder(collection, &[]).await.unwrap_err();
    assert_error(&err, 422, "validation_error");
}

#[tokio::test]
async fn reorder_rewrites_positions_and_keeps_link_ids() {
    // 必须是「改 position」而不是「清空重插」—— 后者会让 id 全变，
    // 而客户端手里握着的正是这些 id。
    let db = TestDb::require().await;
    let svc = VideoCollectionService::new(db.pool());
    let collection = seed_collection(&db).await;
    for _ in 0..3 {
        let v = seed_video(&db).await;
        svc.add_item(collection, v).await.unwrap();
    }
    let before: Vec<i32> = svc
        .list_items(collection)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.id)
        .collect();

    let mut target = before.clone();
    target.reverse();
    let after = svc
        .reorder(collection, &target)
        .await
        .expect("完整覆盖应当成功");

    let got: Vec<i32> = after.iter().map(|l| l.id).collect();
    assert_eq!(got, target, "顺序必须等于入参");
    assert_eq!(
        after.iter().map(|l| l.position).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "位置必须重写为 0..n"
    );
    let mut still_there = before.clone();
    still_there.sort_unstable();
    let mut surviving = got.clone();
    surviving.sort_unstable();
    assert_eq!(surviving, still_there, "关联行 id 不能变");
}
