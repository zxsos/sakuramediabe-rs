//! 合集族（父表 + 成员表）的集成测试。
//!
//! # 为什么要专门一个文件
//!
//! 合集族此前**完全没有测试**：三个父表仓储在某次提交里落地时只带了文档
//! 说明，成员表仓储是后来补的。两者都没跑过 —— 与本仓库此前那批缺陷
//! 一样的形状，只是这批还没被发现。
//!
//! 这里覆盖四类行为，每一类都对应一个「文档声称但没人验证过」的说法：
//!
//! | 行为 | 声称 | 验证方式 |
//! |---|---|---|
//! | 插件归属半配置被拒 | `(plugin, NULL)` 与 `(NULL, key)` 是漏洞 | 入口就返回业务错误 |
//! | `list_plugin_owned` 排除半配置 | 「两列都非空才算」 | 造半配置行，断言不出现 |
//! | 成员唯一性 | 同成员不能重复入合集 | 第二次插入撞唯一约束 |
//! | `position` 可并列 | 唯一索引不含它 | 两个成员同 position 都能插入 |
//! | `unlink` 不重排 | 「不为了补洞而重写全部位置」 | 删中间一个，断言其余 position 不变 |

use sm_db::collections::PluginOwned;
use sm_db::common::time::now_utc;
use sm_db::error::DbError;
use sm_db::repo::playback::{MediaClipRepository, MediaPointRepository, NewMediaClip};
use sm_db::repo::{
    ClipCollectionItemRepository, ClipCollectionRepository, MediaLibraryRepository,
    MediaRepository, MomentCollectionItemRepository, MomentCollectionRepository, MovieRepository,
    NewCollection, NewMedia, NewMediaLibrary, NewMovie, PlaylistMovieRepository,
    PlaylistRepository,
};
use sm_db::testing::TestDb;

// ================================================================ 夹具
//
// 这些父行一律用**仓储**写，不用裸 SQL。
//
// 上一版夹具手写 INSERT，引用了 `media_point` 上根本不存在的列
// （`title` / `kind`），而且漏掉 NOT NULL 的 `image_id` —— 与本仓库
// 此前那批 `image_key` / `provider_key` 缺陷**完全同形**：手写 SQL
// 绕过了模型与仓储，而绕过的东西不会被编译期或对拍检查到。
//
// 用仓储的额外好处：`media_point.image_id` 是 NOT NULL 且无默认值，
// 漏了它就会在运行时炸 —— 那是应该炸的时候。

/// 建一个 `image` 行。`image` 表**还没有仓储**，只能裸 SQL。
///
/// 三列：`id` / `created_at` / `updated_at` / `origin varchar(255) NOT NULL
/// UNIQUE`。`origin` 唯一，所以每次给不同的值。
async fn seed_image(db: &TestDb) -> i32 {
    sqlx::query_scalar::<_, i32>(
        "INSERT INTO image (origin, created_at, updated_at) VALUES ($1, $2, $2) RETURNING id",
    )
    .bind(format!("origin-{}", unique_suffix()))
    .bind(now_utc())
    .fetch_one(db.pool())
    .await
    .expect("insert image")
}

/// 建一个 `media` 行 —— `media_point` / `media_clip` 的父行。
///
/// `MediaRepository::insert` 要求**恰好**归属 `movie_number`（JAV）或
/// `video_item_id`（非 JAV）之一，两者都空或都非空都拒。所以这里先建
/// movie 再用它 —— 不是为了凑约束，而是那条规则本来就该在夹具里被满足。
async fn seed_media(db: &TestDb) -> i32 {
    let movie_number = format!("MAB-{:06}", unique_suffix());
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: movie_number.clone(),
            title: "宿主影片".to_owned(),
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
        .expect("insert movie for media");

    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", unique_suffix()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id;

    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id: library,
            file_name: "holder.mp4".to_owned(),
            file_size_bytes: 1,
            movie_number: Some(movie_number),
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
        .id
}

/// 建一个 `media_point` 行（`moment_collection_item.point_id` 指向它）。
async fn seed_point(db: &TestDb) -> i32 {
    MediaPointRepository::new(db.pool().clone())
        .insert(
            seed_image(db).await,
            0,
            Some(seed_media(db).await),
            None,
            None,
        )
        .await
        .expect("insert media_point")
        .id
}

/// 建一个 `media_clip` 行（`clip_collection_item.clip_id` 指向它）。
async fn seed_clip(db: &TestDb) -> i32 {
    MediaClipRepository::new(db.pool().clone())
        .insert(&NewMediaClip {
            media_id: Some(seed_media(db).await),
            movie_number: None,
            start_offset_seconds: 0,
            end_offset_seconds: 10,
            title: String::new(),
            file_path: "clip.mp4".to_owned(),
            file_size_bytes: 1,
            duration_seconds: 10,
        })
        .await
        .expect("insert media_clip")
        .id
}

/// 建一个 `movie` 行（`playlist_movie.movie_id` 指向它）。
async fn seed_movie(db: &TestDb) -> i32 {
    let number = format!("ABD-{:06}", unique_suffix());
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "测试影片".to_owned(),
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

/// 让每次调用拿到不同的序号。
///
/// `image.origin` 与 `movie.movie_number` 都有唯一索引，而同一测试里
/// 可能连建多行 —— 所以序号必须**每次**都不同，不能按测试分组。
fn unique_suffix() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
// ================================================================ 父表：插件归属

#[tokio::test]
async fn half_configured_plugin_ownership_is_rejected_at_the_door() {
    // 声称：`(plugin, NULL)` 与 `(NULL, key)` 会被入口拒绝。
    //
    // 为什么必须拒：唯一索引 `(owner_plugin_id, plugin_key)` 在
    // `(A, NULL)` 与 `(B, NULL)` 之间**不冲突** —— NULL 不参与唯一约束。
    // 于是两个插件可以用同一个 key 而互不报错，各自「拥有」一份资源，
    // 而按 plugin_key 查时只会拿到任意一份。那是静默的数据分裂。
    let db = TestDb::require().await;
    let repo = PlaylistRepository::new(db.pool().clone());

    let mut only_owner = NewCollection::<sm_db::collections::Playlist>::host_owned("n", "");
    only_owner.owner_plugin_id = Some("plugin-a".to_owned());
    only_owner.plugin_key = None;
    let err = repo
        .insert(&only_owner)
        .await
        .expect_err("只有 owner 没有 key 应被拒");
    assert!(
        err.to_string().contains("plugin_key"),
        "错误应指明缺哪个字段: {err}"
    );

    let mut only_key = NewCollection::<sm_db::collections::Playlist>::host_owned("n", "");
    only_key.owner_plugin_id = None;
    only_key.plugin_key = Some("k".to_owned());
    assert!(
        repo.insert(&only_key).await.is_err(),
        "只有 key 没有 owner 应被拒"
    );

    // 空白串按「没给」处理，不是「给了空串」。
    let mut blank = NewCollection::<sm_db::collections::Playlist>::host_owned("n", "");
    blank.owner_plugin_id = Some("   ".to_owned());
    blank.plugin_key = Some("   ".to_owned());
    assert!(repo.insert(&blank).await.is_err(), "空白应被拒");
}

#[tokio::test]
async fn list_plugin_owned_excludes_half_configured_rows() {
    // 声称：`list_plugin_owned` 用 `plugin_key IS NOT NULL` 把半配置排除。
    //
    // 半配置行造不出来（入口已拒），所以这里直接用裸 SQL 造 —— 目的正是
    // 验证**读路径**不依赖「入口保证数据干净」。哪天有人加了别的写入口
    // 绕过校验，读路径这边仍然是对的。
    let db = TestDb::require().await;
    let repo = MomentCollectionRepository::new(db.pool().clone());

    let clean = repo
        .insert(&NewCollection::plugin_owned(
            "plugin-a", "key-1", "干净", "",
        ))
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO moment_collection (name, description, owner_plugin_id, plugin_key, \
                                         created_at, updated_at) \
         VALUES ($1, '', $2, NULL, $3, $3)",
    )
    .bind("半配置")
    .bind("plugin-a")
    .bind(now_utc())
    .execute(db.pool())
    .await
    .expect("裸 SQL 插入半配置行");

    let page = repo
        .list_plugin_owned(
            "plugin-a",
            sm_db::common::page::PageRequest::new(1, 50).unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(page.total, 1, "半配置行不属于任何插件，不该出现");
    assert_eq!(page.items[0].id, clean.id);
    assert!(page.items[0].is_plugin_owned());
}

#[tokio::test]
async fn the_same_plugin_key_is_unique_but_different_plugins_may_reuse_it() {
    // 唯一索引是 `(owner_plugin_id, plugin_key)`，不是单列 `plugin_key`。
    // 所以：同插件同 key 撞约束，不同插件用同一个 key 各自独立。
    let db = TestDb::require().await;
    let repo = ClipCollectionRepository::new(db.pool().clone());

    repo.insert(&NewCollection::plugin_owned(
        "plugin-a", "shared", "A 的", "",
    ))
    .await
    .unwrap();
    repo.insert(&NewCollection::plugin_owned(
        "plugin-b", "shared", "B 的", "",
    ))
    .await
    .expect("不同插件可以用同一个 key");

    let err = repo
        .insert(&NewCollection::plugin_owned(
            "plugin-a",
            "shared",
            "A 的第二个",
            "",
        ))
        .await
        .expect_err("同插件同 key 应撞唯一约束");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );

    // 宿主创建的合集两列都是 NULL，彼此不冲突 ——
    // NULL 不参与唯一约束，这正是「name 之外还有一条插件索引」的意义。
    repo.insert(&NewCollection::host_owned("宿主 1", ""))
        .await
        .unwrap();
    repo.insert(&NewCollection::host_owned("宿主 2", ""))
        .await
        .expect("宿主创建的合集之间互不冲突");
}
// ================================================================ 成员表：position 语义

#[tokio::test]
async fn append_assigns_increasing_positions() {
    let db = TestDb::require().await;
    let moments = MomentCollectionRepository::new(db.pool().clone());
    let items = MomentCollectionItemRepository::new(db.pool().clone());

    let collection = moments
        .insert(&NewCollection::host_owned("时刻合集", ""))
        .await
        .unwrap();

    let a = items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();
    let b = items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();
    let c = items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();

    assert_eq!(a.position, 0, "首个成员位置 0，而不是 1");
    assert_eq!(b.position, 1);
    assert_eq!(c.position, 2);

    let listed = items.list_by_collection(collection.id).await.unwrap();
    let order: Vec<i32> = listed.iter().map(|i| i.position).collect();
    assert_eq!(order, vec![0, 1, 2], "按位置返回");
}

#[tokio::test]
async fn positions_may_collide_because_the_unique_index_excludes_them() {
    // 声称：唯一索引是 `(collection_id, point_id)`，**不含 position**。
    // 所以两个成员可以同处一个位置，数据库不管 ——
    // 「合集有序」这件事完全靠应用保证。
    //
    // 这个测试固化的是数据库的真实形状，不是理想形状。
    // 若哪天有人给唯一索引加上 position，这个测试会开始失败，
    // 那时候 `insert_at` 的文档与实现都需要重新审视。
    let db = TestDb::require().await;
    let moments = MomentCollectionRepository::new(db.pool().clone());
    let items = MomentCollectionItemRepository::new(db.pool().clone());

    let collection = moments
        .insert(&NewCollection::host_owned("并列位置", ""))
        .await
        .unwrap();

    let a = items
        .insert_at(collection.id, seed_point(&db).await, 5)
        .await
        .unwrap();
    let b = items
        .insert_at(collection.id, seed_point(&db).await, 5)
        .await
        .unwrap();
    assert_eq!(a.position, 5);
    assert_eq!(b.position, 5, "同位置不撞约束：索引不含 position");

    // 并列时顺序仍然确定 —— 靠 id 作次级键。
    let listed = items.list_by_collection(collection.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed[0].id < listed[1].id,
        "并列时按 id 排，顺序必须确定，否则播放列表抖动"
    );
}

#[tokio::test]
async fn the_same_member_cannot_appear_twice_in_one_collection() {
    // 唯一索引 `(collection_id, point_id)` 保证的正是这一条。
    let db = TestDb::require().await;
    let moments = MomentCollectionRepository::new(db.pool().clone());
    let items = MomentCollectionItemRepository::new(db.pool().clone());

    let collection = moments
        .insert(&NewCollection::host_owned("去重", ""))
        .await
        .unwrap();
    let point = seed_point(&db).await;

    items.insert_at(collection.id, point, 0).await.unwrap();
    let err = items
        .insert_at(collection.id, point, 1)
        .await
        .expect_err("同一时刻不能在同一个合集里出现两次");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );

    // 换一个合集就能再放一次。
    let other = moments
        .insert(&NewCollection::host_owned("另一个", ""))
        .await
        .unwrap();
    items
        .insert_at(other.id, point, 0)
        .await
        .expect("不同合集之间互不影响");
}

#[tokio::test]
async fn unlink_leaves_the_remaining_positions_alone() {
    // 声称：`unlink` **不重排**留下的空位。
    //
    // 删中间一个就为补洞而重写全部 position，代价是 O(n) 次写，且会让
    // 并发读者看到中间状态。缺口在播放时不可见 ——
    // `ORDER BY position, id` 仍然给出确定顺序。
    let db = TestDb::require().await;
    let moments = MomentCollectionRepository::new(db.pool().clone());
    let items = MomentCollectionItemRepository::new(db.pool().clone());

    let collection = moments
        .insert(&NewCollection::host_owned("留空位", ""))
        .await
        .unwrap();

    let p1 = seed_point(&db).await;
    let p2 = seed_point(&db).await;
    let p3 = seed_point(&db).await;
    let first = items.append(collection.id, p1).await.unwrap();
    let middle = items.append(collection.id, p2).await.unwrap();
    let last = items.append(collection.id, p3).await.unwrap();

    assert!(items.unlink(collection.id, middle.point_id).await.unwrap());

    let listed = items.list_by_collection(collection.id).await.unwrap();
    assert_eq!(listed.len(), 2, "删掉一个，剩两个");
    assert_eq!(listed[0].id, first.id);
    assert_eq!(listed[1].id, last.id);
    assert_eq!(
        listed[1].position, 2,
        "末尾成员的 position 不该被重排 —— 那正是本测试要固化的行为"
    );

    // 再追加一个：max+1 会跳过空位继续往上。
    let next = items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();
    assert_eq!(next.position, 3, "max+1 从 2 起算");
}
#[tokio::test]
async fn replace_all_makes_the_order_exactly_the_argument() {
    let db = TestDb::require().await;
    let clips = ClipCollectionRepository::new(db.pool().clone());
    let items = ClipCollectionItemRepository::new(db.pool().clone());

    let collection = clips
        .insert(&NewCollection::host_owned("片段合集", ""))
        .await
        .unwrap();

    let c1 = seed_clip(&db).await;
    let c2 = seed_clip(&db).await;
    let c3 = seed_clip(&db).await;
    for c in [c1, c2, c3] {
        items.append(collection.id, c).await.unwrap();
    }
    assert_eq!(
        items.list_by_collection(collection.id).await.unwrap().len(),
        3
    );

    // 倒序重排。
    items
        .replace_all(collection.id, &[c3, c1, c2])
        .await
        .unwrap();

    let listed = items.list_by_collection(collection.id).await.unwrap();
    let order: Vec<i32> = listed.iter().map(|i| i.clip_id).collect();
    assert_eq!(order, vec![c3, c1, c2], "顺序必须等于入参");
    let positions: Vec<i32> = listed.iter().map(|i| i.position).collect();
    assert_eq!(positions, vec![0, 1, 2], "位置被重排为 0 起连续");

    // 传空列表等于清空。
    items.replace_all(collection.id, &[]).await.unwrap();
    assert!(items
        .list_by_collection(collection.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn deleting_a_collection_cascades_to_its_members() {
    // 两种方向都验证：删合集 → 成员行消失（CASCADE）。
    // 另一方向（删成员 → 所属合集里的行消失）由外键的 CASCADE 保证，
    // 下面用 media_point / media_clip 被删的场景验证。
    let db = TestDb::require().await;
    let moments = MomentCollectionRepository::new(db.pool().clone());
    let items = MomentCollectionItemRepository::new(db.pool().clone());

    let collection = moments
        .insert(&NewCollection::host_owned("级联", ""))
        .await
        .unwrap();
    items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();
    items
        .append(collection.id, seed_point(&db).await)
        .await
        .unwrap();
    assert_eq!(
        items.list_by_collection(collection.id).await.unwrap().len(),
        2
    );

    assert!(moments.delete(collection.id).await.unwrap());
    assert!(
        items
            .list_by_collection(collection.id)
            .await
            .unwrap()
            .is_empty(),
        "删合集会 CASCADE 掉成员行，不该留下孤儿"
    );

    // 成员被删 → 那一行从合集里消失。
    let keep = moments
        .insert(&NewCollection::host_owned("反向", ""))
        .await
        .unwrap();
    let point = seed_point(&db).await;
    items.append(keep.id, point).await.unwrap();
    sqlx::query("DELETE FROM media_point WHERE id = $1")
        .bind(point)
        .execute(db.pool())
        .await
        .expect("delete media_point");
    assert!(
        items.list_by_collection(keep.id).await.unwrap().is_empty(),
        "时刻被删，成员行应随之消失（CASCADE）"
    );
}

// ================================================================ playlist_movie：无 position

#[tokio::test]
async fn playlist_movie_add_is_idempotent_and_order_is_join_time() {
    // 本表**没有 position** —— 唯一索引 `(playlist_id, movie_id)`。
    // 顺序只能靠 `id`，即加入先后。
    let db = TestDb::require().await;
    let playlists = PlaylistRepository::new(db.pool().clone());
    let members = PlaylistMovieRepository::new(db.pool().clone());

    let playlist = playlists
        .insert(&NewCollection::host_owned("播放列表", ""))
        .await
        .unwrap();

    let m1 = seed_movie(&db).await;
    let m2 = seed_movie(&db).await;
    let m3 = seed_movie(&db).await;

    assert!(
        members.add(playlist.id, m1).await.unwrap(),
        "首次应真的新增"
    );
    assert!(!members.add(playlist.id, m1).await.unwrap(), "重复应幂等");
    members.add(playlist.id, m2).await.unwrap();
    members.add(playlist.id, m3).await.unwrap();

    let listed = members.list_by_playlist(playlist.id).await.unwrap();
    let order: Vec<i32> = listed.iter().map(|r| r.movie_id).collect();
    assert_eq!(order, vec![m1, m2, m3], "顺序 = 加入先后");
    assert_eq!(listed[0].playback_order_key(), listed[0].id);

    assert!(members.remove(playlist.id, m2).await.unwrap());
    assert!(
        !members.remove(playlist.id, m2).await.unwrap(),
        "重复移出返回 false"
    );
    let after = members.list_by_playlist(playlist.id).await.unwrap();
    assert_eq!(after.len(), 2);
}

#[tokio::test]
async fn playlist_movie_lists_every_playlist_a_movie_appears_in() {
    let db = TestDb::require().await;
    let playlists = PlaylistRepository::new(db.pool().clone());
    let members = PlaylistMovieRepository::new(db.pool().clone());

    let p1 = playlists
        .insert(&NewCollection::host_owned("列表 1", ""))
        .await
        .unwrap();
    let p2 = playlists
        .insert(&NewCollection::host_owned("列表 2", ""))
        .await
        .unwrap();
    let movie = seed_movie(&db).await;
    members.add(p1.id, movie).await.unwrap();
    members.add(p2.id, movie).await.unwrap();

    let page = members
        .list_by_movie(movie, sm_db::common::page::PageRequest::new(1, 50).unwrap())
        .await
        .unwrap();
    assert_eq!(page.total, 2, "同一部影片可以在多个列表里");
    assert_eq!(page.items.len(), 2);
}
