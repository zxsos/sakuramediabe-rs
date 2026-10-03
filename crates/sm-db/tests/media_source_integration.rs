//! `image` 与 `video_item` 的集成测试。
//!
//! 这两张表此前**完全没有仓储**，而它们是两条真实阻塞：
//!
//! - `media_point.image_id` 是 `NOT NULL` 且**无 DEFAULT** —— 没有 `image`
//!   仓储，「给时刻配一张图」只能手写 SQL。
//! - `media` 上有 `CHECK ((movie_number IS NULL) <> (video_item_id IS NULL))`
//!   且 `MediaRepository::insert` 强制同一规则 —— 没有 `video_item` 仓储，
//!   **非 JAV 媒体根本写不进去**。
//!
//! 本文件同时验证这两条阻塞解除后，非 JAV 媒体真的能走通全链路。

use sm_db::common::page::PageRequest;
use sm_db::error::DbError;
use sm_db::repo::{
    ImageRepository, MediaRepository, NewImage, NewMedia, NewVideoItem, VideoItemRepository,
};
use sm_db::testing::TestDb;

fn page() -> PageRequest {
    PageRequest::new(1, 50).unwrap()
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

// ================================================================ image

#[tokio::test]
async fn upsert_is_idempotent_on_origin() {
    // `origin` 是 `varchar(255) NOT NULL UNIQUE`，所以重复登记同一张图
    // **不是错误** —— 刮削任务重跑、同一部影片入库两次都会走到这里。
    //
    // 冲突时更新 `updated_at` 而不碰 `origin`（它是冲突键，改了就不是
    // 「同一张图」了）。返回 `false` 让调用方能区分「刚建的」与
    // 「早就有的」—— 后者意味着磁盘上的文件可能已经不在了。
    let db = TestDb::require().await;
    let repo = ImageRepository::new(db.pool().clone());
    let origin = format!("actors/ABD-{:03}/1.jpg", n());

    let (first_id, created) = repo
        .upsert(&NewImage {
            origin: origin.clone(),
        })
        .await
        .unwrap();
    assert!(created, "首次应新建");

    let (second_id, created_again) = repo
        .upsert(&NewImage {
            origin: origin.clone(),
        })
        .await
        .unwrap();
    assert!(!created_again, "重复应命中冲突分支");
    assert_eq!(first_id, second_id, "是同一行，不该产生新 id");

    let found = repo.find_by_origin(&origin).await.unwrap().unwrap();
    assert_eq!(found.id, first_id);
}

#[tokio::test]
async fn blank_origin_is_rejected() {
    let db = TestDb::require().await;
    let repo = ImageRepository::new(db.pool().clone());
    for blank in ["", "   ", "\t\n"] {
        let err = repo
            .upsert(&NewImage {
                origin: blank.to_owned(),
            })
            .await
            .expect_err("空 origin 无意义，且会让前缀查询命中一切");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }
}

#[tokio::test]
async fn like_prefix_escapes_underscores_so_it_cannot_match_a_sibling() {
    // **本文件最重要的一个测试。**
    //
    // `origin` 是文件路径，而目录名里出现 `_` 极其常见 ——
    // `ABD-001_1080p`、`scene_01`。
    //
    // 不转义的话，`LIKE 'actors/ABD-001_%'` 里的 `_` 是**单字符通配符**，
    // 会把 `ABD-001X` 也匹配进来 —— 那是**另一个演员**的目录。
    //
    // 上游给 `origin` 建了 `text_pattern_ops` 索引，那条索引只对
    // `LIKE 'prefix%'` 有用，所以前缀查询是热路径，这个转义不能错。
    let db = TestDb::require().await;
    let repo = ImageRepository::new(db.pool().clone());

    // 序号**取一次**存进局部变量：测试是并行跑的，而 `n()` 是进程级
    // 计数器 —— 连续调用三次会拿到三个不同的值，前缀就对不上了。
    // （第一版就是这么写的，于是期望里的前缀与实际插入的不是同一个。）
    let id = n();
    let wanted = format!("actors/ABD-{id:03}_1080p/1.jpg");
    let sibling = format!("actors/ABD-{id:03}X1080p/1.jpg");
    let deeper = format!("actors/ABD-{id:03}_1080p/2.jpg");

    for origin in [&wanted, &sibling, &deeper] {
        repo.upsert(&NewImage {
            origin: origin.clone(),
        })
        .await
        .unwrap();
    }

    let prefix = format!("actors/ABD-{id:03}_1080p/");
    let result = repo
        .list_by_origin_pattern(&ImageRepository::like_prefix(&prefix), page())
        .await
        .unwrap();

    let origins: Vec<&str> = result.items.iter().map(|i| i.origin.as_str()).collect();
    assert_eq!(
        origins,
        vec![wanted.as_str(), deeper.as_str()],
        "只该命中该目录下的两张图，同前缀的兄弟目录必须排除"
    );
    assert_eq!(result.total, 2, "total 与 items 必须是同一个集合");
}

#[tokio::test]
async fn like_prefix_escapes_the_prefix_but_keeps_the_wildcard_suffix() {
    // 固化 `like_prefix` 的**精确**语义，两半都要写清：
    //
    // - 前缀里的 `_` / `%` 被转义 → 按字面匹配
    // - 末尾追加的 `%` **不**转义 → 保持通配，那正是「前缀」的意思
    //
    // 第一版把这个测试写成「转义后一条都命中不了」，那是错的：追加的
    // `%` 就是通配符，`like_prefix` 的产物是一个**前缀模式**而非字面串。
    // 用 `_` 重写才能真正体现两种语义的分界。
    let db = TestDb::require().await;
    let repo = ImageRepository::new(db.pool().clone());
    let id = n();

    // 一个含 `_` 的目录，一个只差一个字符的兄弟目录。
    repo.upsert(&NewImage {
        origin: format!("pat/a_b{}.jpg", id),
    })
    .await
    .unwrap();
    repo.upsert(&NewImage {
        origin: format!("pat/aXb{}.jpg", id),
    })
    .await
    .unwrap();

    // 裸模式：`_` 是单字符通配，两条都命中。
    let bare = repo
        .list_by_origin_pattern("pat/a_%", page())
        .await
        .unwrap();
    assert_eq!(bare.total, 2, "裸模式里 _ 是通配符");

    // 经 `like_prefix`：`_` 按字面匹配，只有 `a_b` 那条。
    let escaped = repo
        .list_by_origin_pattern(&ImageRepository::like_prefix("pat/a_b"), page())
        .await
        .unwrap();
    assert_eq!(escaped.total, 1, "转义后 _ 是字面量，兄弟目录必须被排除");
    assert!(
        escaped.items[0].origin.contains("_"),
        "命中的应该是含下划线的那条: {}",
        escaped.items[0].origin
    );

    // 末尾的 `%` 仍是通配：同一个前缀下再加一张图也会被命中。
    repo.upsert(&NewImage {
        origin: format!("pat/a_b{}.png", id),
    })
    .await
    .unwrap();
    let after = repo
        .list_by_origin_pattern(&ImageRepository::like_prefix("pat/a_b"), page())
        .await
        .unwrap();
    assert_eq!(after.total, 2, "追加的 % 保持通配语义");
}
// ================================================================ video_item

#[tokio::test]
async fn blank_title_is_rejected() {
    let db = TestDb::require().await;
    let repo = VideoItemRepository::new(db.pool().clone());
    for blank in ["", "   ", "\t\n"] {
        let err = repo
            .insert(&NewVideoItem {
                title: blank.to_owned(),
                summary: String::new(),
                cover_image_id: None,
                release_date: None,
                extra: None,
            })
            .await
            .expect_err("空标题没有意义");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }
}
#[tokio::test]
async fn a_non_jav_media_file_can_now_be_written_at_all() {
    // **这正是 `video_item` 仓储存在的理由。**
    //
    // `media` 有 `CHECK ((movie_number IS NULL) <> (video_item_id IS NULL))`，
    // `MediaRepository::insert` 在 Rust 侧强制同一规则。所以在这张仓储落地
    // 之前，JAV 影片能写（`movie` 有仓储）而**非 JAV 媒体一条都写不进去** ——
    // 一张叶子表卡住了一整类数据的入口。
    let db = TestDb::require().await;
    let videos = VideoItemRepository::new(db.pool().clone());
    let libraries = sm_db::repo::MediaLibraryRepository::new(db.pool().clone());
    let media = MediaRepository::new(db.pool().clone());

    let item = videos
        .insert(&NewVideoItem {
            title: format!("非 JAV 影片 {}", n()),
            summary: "一段简介".to_owned(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .unwrap();

    let library = libraries
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .unwrap();

    // movie_number 与 video_item_id 恰好给一个 —— 给 video_item_id。
    let file = media
        .insert(&NewMedia {
            library_id: library.id,
            file_name: "non-jav.mkv".to_owned(),
            file_size_bytes: 1,
            movie_number: None,
            video_item_id: Some(item.id),
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("非 JAV 媒体现在可以写入了");

    assert_eq!(file.video_item_id, Some(item.id));
    assert!(file.movie_number.is_none(), "两者必须恰好其一");

    // 反向列出：这个条目下有哪些文件。
    let files = videos.list_media(item.id).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, file.id);
}

#[tokio::test]
async fn the_both_or_neither_rule_still_holds() {
    // 有了 `video_item` 仓储之后，那条 CHECK 更容易被「想当然」地理解
    // —— 顺手就能造出「两个都给」或「两个都不给」。固化它。
    let db = TestDb::require().await;
    let media = MediaRepository::new(db.pool().clone());
    let library = sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .unwrap();

    let err = media
        .insert(&NewMedia {
            library_id: library.id,
            file_name: "orphan.mkv".to_owned(),
            file_size_bytes: 1,
            movie_number: None,
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect_err("两者都空应被拒");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    assert!(
        err.to_string().contains("movie"),
        "错误应指明是归属问题: {err}"
    );
}
#[tokio::test]
async fn recent_listing_puts_dated_entries_first_and_undated_last() {
    // PostgreSQL 的 `DESC` 默认把 NULL 排在**前**面。不显式写
    // `(release_date IS NULL)` 的话，「最近上映」列表的第一页会被一堆
    // 无日期条目占满 —— 而那些恰恰是最不可能是「最近」的。
    let db = TestDb::require().await;
    let repo = VideoItemRepository::new(db.pool().clone());

    let dated = d(2024, 3, 1);
    let older = d(2020, 1, 1);

    repo.insert(&NewVideoItem {
        title: format!("新片 {}", n()),
        summary: String::new(),
        cover_image_id: None,
        release_date: Some(dated),
        extra: None,
    })
    .await
    .unwrap();
    repo.insert(&NewVideoItem {
        title: format!("老片 {}", n()),
        summary: String::new(),
        cover_image_id: None,
        release_date: Some(older),
        extra: None,
    })
    .await
    .unwrap();
    repo.insert(&NewVideoItem {
        title: format!("无日期 {}", n()),
        summary: String::new(),
        cover_image_id: None,
        release_date: None,
        extra: None,
    })
    .await
    .unwrap();

    let result = repo.list_recent(None, page()).await.unwrap();
    assert_eq!(result.total, 3);
    assert_eq!(result.items[0].release_date, Some(dated), "最新的在最前");
    assert_eq!(
        result.items[2].release_date, None,
        "无日期的必须排在**最后**，不能占住第一页"
    );
}

#[tokio::test]
async fn an_optional_date_bound_narrows_the_listing() {
    // 验证 `Option<T>` 走 `PageArg` blanket impl：`None` 时过滤失效。
    //
    // 手写分页会丢掉 `paged_list!` 保证的 count/items 一致性 —— 上一轮就有
    // 一个 `count` 漏了 `AND plugin_key IS NOT NULL` 而 `items` 有，于是
    // `total` 与 `items.len()` 描述的不是同一个集合。所以这里用同一个宏，
    // 而不是复制两段 SQL。
    let db = TestDb::require().await;
    let repo = VideoItemRepository::new(db.pool().clone());
    let recent = d(2024, 3, 1);

    repo.insert(&NewVideoItem {
        title: format!("2024 {}", n()),
        summary: String::new(),
        cover_image_id: None,
        release_date: Some(recent),
        extra: None,
    })
    .await
    .unwrap();
    repo.insert(&NewVideoItem {
        title: format!("2019 {}", n()),
        summary: String::new(),
        cover_image_id: None,
        release_date: Some(d(2019, 1, 1)),
        extra: None,
    })
    .await
    .unwrap();

    let all = repo.list_recent(None, page()).await.unwrap();
    assert_eq!(all.total, 2, "None 表示不设下界");

    let bounded = repo.list_recent(Some(recent), page()).await.unwrap();
    assert_eq!(bounded.total, 1, "下界应生效");
    assert_eq!(
        bounded.total as usize,
        bounded.items.len(),
        "total 与 items 必须是同一个集合"
    );
}
#[tokio::test]
async fn title_lookup_is_the_idempotency_key_because_the_table_has_no_unique_constraint() {
    // `video_item` **没有**任何唯一约束，所以「同一个非 JAV 影片被登记两次」
    // 数据库拦不住 —— 这也是本仓储不提供 `upsert` 的原因：拿不出一个
    // 正确的冲突键，提供就是骗人。
    //
    // 但同时**刻意不**把 (title, release_date) 声明成唯一：同标题、同一天
    // 上映的两部不同影片是可能的（重拍、翻拍），那比重复更糟。
    let db = TestDb::require().await;
    let repo = VideoItemRepository::new(db.pool().clone());
    let date = d(2023, 6, 1);
    let title = format!("同名影片 {}", n());

    let first = repo
        .insert(&NewVideoItem {
            title: title.clone(),
            summary: String::new(),
            cover_image_id: None,
            release_date: Some(date),
            extra: None,
        })
        .await
        .unwrap();
    // 再插一条同名同日期的 —— 数据库**允许**。
    let second = repo
        .insert(&NewVideoItem {
            title: title.clone(),
            summary: String::new(),
            cover_image_id: None,
            release_date: Some(date),
            extra: None,
        })
        .await
        .expect("没有唯一约束，重复登记是允许的");
    assert_ne!(first.id, second.id);

    // 所以「幂等」必须由调用方在应用层做：先查再决定插不插。
    let found = repo
        .find_by_title_and_date(&title, Some(date))
        .await
        .unwrap()
        .expect("查得到");
    assert!(found.id == first.id || found.id == second.id);

    // `IS NOT DISTINCT FROM` 让「无日期」也能匹配上 —— 否则无日期的条目
    // 永远查不到自己，应用层的幂等检查对它就失效了。
    let undated = format!("无日期条目 {}", n());
    let row = repo
        .insert(&NewVideoItem {
            title: undated.clone(),
            summary: String::new(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .unwrap();
    let found = repo
        .find_by_title_and_date(&undated, None)
        .await
        .unwrap()
        .expect("release_date 为 NULL 也要能查到");
    assert_eq!(found.id, row.id);
}

#[tokio::test]
async fn a_cover_image_can_be_attached_and_detached() {
    // `video_item.cover_image_id` 指向 `image.id`，而 `image` 此前没有仓储
    // —— 所以「给非 JAV 影片配封面」此前也写不进去。
    let db = TestDb::require().await;
    let videos = VideoItemRepository::new(db.pool().clone());
    let images = ImageRepository::new(db.pool().clone());

    let item = videos
        .insert(&NewVideoItem {
            title: format!("带封面 {}", n()),
            summary: String::new(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .unwrap();
    assert!(item.cover_image_id.is_none());

    let (cover_id, _) = images
        .upsert(&NewImage {
            origin: format!("covers/{}.jpg", n()),
        })
        .await
        .unwrap();

    assert!(videos.set_cover(item.id, Some(cover_id)).await.unwrap());
    let after = videos.find_by_id(item.id).await.unwrap().unwrap();
    assert_eq!(after.cover_image_id, Some(cover_id));

    // 置空：封面没了但条目还在。
    assert!(videos.set_cover(item.id, None).await.unwrap());
    let after = videos.find_by_id(item.id).await.unwrap().unwrap();
    assert!(after.cover_image_id.is_none());
    assert!(videos.find_by_id(item.id).await.unwrap().is_some());
}

/// 构造一个 `NaiveDateTime`。
fn d(year: i32, month: u32, day: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
}
