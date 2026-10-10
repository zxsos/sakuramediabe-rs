//! `video_collection` 与 `video_collection_item` 的集成测试。
//!
//! # 这两张表与 JAV 侧合集族的四处不同
//!
//! | | JAV 合集 | 视频合集 |
//! |---|---|---|
//! | 父表插件归属 | `owner_plugin_id` + `plugin_key` | **都没有** |
//! | 成员表 `position` | `moment`/`clip` 有，`playlist_movie` 没有 | **有，且 `DEFAULT 0`** |
//! | 模型位置 | `crate::collections` | `crate::videos` |
//! | 额外索引 | 无位置索引 | `video_collection_item_position_idx` |
//!
//! 第一行是本文件的主要理由：`video_collection` **不实现** `PluginOwned`，
//! 所以 `collection.rs` 的宏不能套用。硬套会生成引用不存在列的 SQL。
//!
//! 第二行影响行为，单独有测试。

use sm_db::common::page::PageRequest;
use sm_db::error::DbError;
use sm_db::repo::{
    NewVideoCollection, NewVideoItem, VideoCollectionItemRepository, VideoCollectionRepository,
    VideoItemRepository,
};
use sm_db::testing::TestDb;

fn page() -> PageRequest {
    PageRequest::new(1, 50).unwrap()
}

/// 建一个 `video_item` 行 —— 合集成员的父行。
async fn seed_item(db: &TestDb, tag: &str) -> i32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let id = C.fetch_add(1, Ordering::Relaxed);

    VideoItemRepository::new(db.pool().clone())
        .insert(&NewVideoItem {
            title: format!("{tag}-{id}"),
            summary: String::new(),
            cover_image_id: None,
            release_date: None,
            extra: None,
        })
        .await
        .expect("insert video_item")
        .id
}

/// 建一个 `video_collection` 行，返回 `(id, name)`。
///
/// **两个都返回**，而不是让调用方拿 `tag` 去拼名字：序号来自进程级
/// 原子计数器，而测试是并行跑的 —— 同一个 `tag` 在不同测试里会拿到
/// 不同的序号。第一版只返回 id，于是有测试去断言 `find_by_name("rename-0")`，
/// 而那次调用实际拿到的可能是 `rename-7`。夹具把名字一起返回，调用方
/// 就不必去猜计数器走到了哪。
async fn seed_collection(db: &TestDb, tag: &str) -> (i32, String) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let id = C.fetch_add(1, Ordering::Relaxed);
    let name = format!("{tag}-{id}");

    let row = VideoCollectionRepository::new(db.pool().clone())
        .insert(&NewVideoCollection {
            name: name.clone(),
            description: String::new(),
        })
        .await
        .expect("insert video_collection");
    (row.id, name)
}
// ================================================================ 父表

#[tokio::test]
async fn collection_names_are_globally_unique() {
    // `video_collection.name varchar(255) NOT NULL UNIQUE`，无软删除列，
    // 所以重名就是**同一个**合集 —— 撞唯一约束。
    let db = TestDb::require().await;
    let repo = VideoCollectionRepository::new(db.pool().clone());
    let name = format!(
        "dup-{}",
        sm_db::common::time::now_utc().and_utc().timestamp()
    );

    repo.insert(&NewVideoCollection {
        name: name.clone(),
        description: String::new(),
    })
    .await
    .unwrap();

    let err = repo
        .insert(&NewVideoCollection {
            name: name.clone(),
            description: String::new(),
        })
        .await
        .expect_err("name 全局唯一");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn blank_name_is_rejected_by_insert_and_rename_alike() {
    // 两个入口用**同一把尺子**。改名单独宽松会让「建的时候不能叫空、
    // 改了却能改成空」这种不一致发生。
    let db = TestDb::require().await;
    let repo = VideoCollectionRepository::new(db.pool().clone());

    for blank in ["", "   ", "\t\n"] {
        let err = repo
            .insert(&NewVideoCollection {
                name: blank.to_owned(),
                description: String::new(),
            })
            .await
            .expect_err("空名无意义");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }

    // 名字从夹具拿，而不是用 `tag` 拼 —— 序号是进程级的，而测试并行跑。
    let (id, name) = seed_collection(&db, "rename").await;
    for blank in ["", "   "] {
        let err = repo
            .rename(id, blank)
            .await
            .expect_err("改名也必须拒绝空名");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }
    // 两次失败的改名都没碰坏这一行。
    let after = repo
        .find_by_name(&name)
        .await
        .unwrap()
        .expect("失败的 rename 不应留下痕迹");
    assert_eq!(after.id, id);
    assert_eq!(after.name, name, "名字应保持原样");
}

#[tokio::test]
async fn this_table_has_no_plugin_ownership_columns() {
    // 固化一个「缺失」：上游没有给 `video_collection` 加
    // `owner_plugin_id` / `plugin_key`，所以它不实现 `PluginOwned`，
    // 也没有 `list_plugin_owned` / `find_by_plugin_key`。
    //
    // 与 `playlist` / `moment_collection` / `clip_collection` 不同 ——
    // 那三张都有插件归属，唯一索引 `(owner_plugin_id, plugin_key)`，
    // 且入口会拒「只有 owner 没有 key」的半配置。
    //
    // 本测试的意义是：哪天有人给这张表**也**加上归属列，这个测试会失败，
    // 那时候该给 `VideoCollectionRepository` 补上那三个方法与半配置校验。
    // 写一个「断言某方法不存在」的测试做不到，所以反过来断言列不存在。
    let db = TestDb::require().await;
    // `table_schema = $1` 不可省：`information_schema.columns` 跨全部
    // schema 可见，而每个测试建自己的 schema —— 不过滤会拿到历史残留的
    // 同名表，列名重复一份。这里用 `.any()` 所以重复**不会**报错，
    // 但那掩盖了「查的到底是哪张表」这个问题。
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'video_collection' \
         ORDER BY column_name",
    )
    .bind(db.schema())
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert!(
        !columns.iter().any(|c| c == "owner_plugin_id"),
        "上游没给 video_collection 加插件归属列: {columns:?}"
    );
    assert!(
        !columns.iter().any(|c| c == "plugin_key"),
        "上游没给 video_collection 加插件归属列: {columns:?}"
    );
    // 确认查的是对的表 —— 顺带钉住「同名表」这个混淆来源。
    assert!(columns.iter().any(|c| c == "name"), "{columns:?}");
}

// ================================================================ 成员表：position 的 DEFAULT 0

#[tokio::test]
async fn append_does_not_rely_on_the_position_default() {
    // **本表与另两个合集的关键差异。**
    //
    // ```text
    // video_collection_item.position     integer NOT NULL DEFAULT 0
    // moment_collection_item.position    integer NOT NULL
    // clip_collection_item.position      integer NOT NULL
    // ```
    //
    // 本表有 `DEFAULT 0`。所以「追加成员时省略 position」会**并排**在 0，
    // 而不是递增。`append` 因此总是显式算 `max(position) + 1`。
    //
    // 这个测试固化的是**仓储的行为**，而不是 DDL 的形状：它断言三个成员
    // 拿到 0/1/2 而不是 0/0/0。若哪天有人把 `append` 改成「不绑 position，
    // 让 DEFAULT 生效」，这里立刻失败。
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "append").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());

    let a = items
        .append(collection, seed_item(&db, "a").await)
        .await
        .unwrap();
    let b = items
        .append(collection, seed_item(&db, "b").await)
        .await
        .unwrap();
    let c = items
        .append(collection, seed_item(&db, "c").await)
        .await
        .unwrap();

    assert_eq!(a.position, 0, "首个成员位置 0");
    assert_eq!(b.position, 1, "不能与 a 并排在 0 —— 那是 DEFAULT 0 的陷阱");
    assert_eq!(c.position, 2);
}
#[tokio::test]
async fn positions_may_collide_and_ordering_stays_deterministic() {
    // 唯一索引是 `(collection_id, video_item_id)`，**不含 position**。
    // 所以两个成员可以同处一个位置，数据库不管。
    //
    // 顺序仍然确定 —— `playback_order_key()` 返回 `(position, id)`，
    // 列表查询写的就是 `ORDER BY position, id`。只按 `position` 排时
    // 并列之间顺序不确定，会导致播放列表抖动。
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "collide").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());

    let a = items
        .insert_at(collection, seed_item(&db, "ca").await, 5)
        .await
        .unwrap();
    let b = items
        .insert_at(collection, seed_item(&db, "cb").await, 5)
        .await
        .unwrap();
    assert_eq!(a.position, 5);
    assert_eq!(b.position, 5, "同位置不撞约束：索引不含 position");
    assert_eq!(a.playback_order_key(), (5, a.id));
    assert_eq!(b.playback_order_key(), (5, b.id));

    let listed = items.list_by_collection(collection).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].id < listed[1].id, "并列时按 id 排，顺序必须确定");
}

#[tokio::test]
async fn the_same_video_cannot_appear_twice_in_one_collection() {
    // 唯一索引 `(collection_id, video_item_id)` 保证的正是这一条。
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "dedup").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());
    let video = seed_item(&db, "one").await;

    items.insert_at(collection, video, 0).await.unwrap();
    let err = items
        .insert_at(collection, video, 1)
        .await
        .expect_err("同一个视频不能在同一个合集里出现两次");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );

    // 换一个合集就能再放一次。
    let other = seed_collection(&db, "dedup2").await.0;
    items
        .insert_at(other, video, 0)
        .await
        .expect("不同合集之间互不影响");
}

#[tokio::test]
async fn unlink_leaves_the_remaining_positions_alone() {
    // 与另两个合集同一套规则：删中间一个**不重排**留下的空位。
    // 补洞要重写全部 position，代价 O(n) 次写，且会让并发读者看到
    // 中间状态。缺口在播放时不可见 —— `ORDER BY position, id` 仍确定。
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "gap").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());

    let p1 = seed_item(&db, "g1").await;
    let p2 = seed_item(&db, "g2").await;
    let p3 = seed_item(&db, "g3").await;
    let first = items.append(collection, p1).await.unwrap();
    let middle = items.append(collection, p2).await.unwrap();
    let last = items.append(collection, p3).await.unwrap();

    assert!(items
        .unlink(collection, middle.video_item_id)
        .await
        .unwrap());

    let listed = items.list_by_collection(collection).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, first.id);
    assert_eq!(listed[1].id, last.id);
    assert_eq!(listed[1].position, 2, "末尾成员的 position 不该被重排");

    // 再追加：max+1 从 2 起算。
    let next = items
        .append(collection, seed_item(&db, "g4").await)
        .await
        .unwrap();
    assert_eq!(next.position, 3, "max+1 从当前最大值起算");
}

#[tokio::test]
async fn replace_all_makes_the_order_exactly_the_argument() {
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "reorder").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());

    let c1 = seed_item(&db, "r1").await;
    let c2 = seed_item(&db, "r2").await;
    let c3 = seed_item(&db, "r3").await;
    for c in [c1, c2, c3] {
        items.append(collection, c).await.unwrap();
    }
    assert_eq!(items.list_by_collection(collection).await.unwrap().len(), 3);

    items.replace_all(collection, &[c3, c1, c2]).await.unwrap();

    let listed = items.list_by_collection(collection).await.unwrap();
    let order: Vec<i32> = listed.iter().map(|i| i.video_item_id).collect();
    assert_eq!(order, vec![c3, c1, c2], "顺序必须等于入参");
    let positions: Vec<i32> = listed.iter().map(|i| i.position).collect();
    assert_eq!(positions, vec![0, 1, 2], "位置被重排为 0 起连续");

    // 空列表等于清空。
    items.replace_all(collection, &[]).await.unwrap();
    assert!(items
        .list_by_collection(collection)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn deleting_a_collection_cascades_to_its_members() {
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "cascade").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());
    let collections = VideoCollectionRepository::new(db.pool().clone());

    for _ in 0..2 {
        items
            .append(collection, seed_item(&db, "cc").await)
            .await
            .unwrap();
    }
    assert_eq!(items.list_by_collection(collection).await.unwrap().len(), 2);

    assert!(collections.delete(collection).await.unwrap());
    assert!(
        items
            .list_by_collection(collection)
            .await
            .unwrap()
            .is_empty(),
        "删合集会 CASCADE 掉成员行"
    );

    // 成员被删 -> 那一行从合集里消失。
    let keep = seed_collection(&db, "rev").await.0;
    let video = seed_item(&db, "cv").await;
    items.append(keep, video).await.unwrap();
    sqlx::query("DELETE FROM video_item WHERE id = $1")
        .bind(video)
        .execute(db.pool())
        .await
        .expect("delete video_item");
    assert!(
        items.list_by_collection(keep).await.unwrap().is_empty(),
        "视频被删，成员行应随之消失（CASCADE）"
    );
}

#[tokio::test]
async fn clear_keeps_the_collection_itself() {
    // 删合集会 CASCADE 掉成员；`clear` 是「保留合集、只清成员」——
    // 那是编辑页「全选取消」的操作。两个入口的差别要固化。
    let db = TestDb::require().await;
    let collection = seed_collection(&db, "clearme").await.0;
    let items = VideoCollectionItemRepository::new(db.pool().clone());
    let collections = VideoCollectionRepository::new(db.pool().clone());

    for _ in 0..3 {
        items
            .append(collection, seed_item(&db, "cx").await)
            .await
            .unwrap();
    }
    assert_eq!(items.clear(collection).await.unwrap(), 3, "返回删掉了几行");
    assert!(items
        .list_by_collection(collection)
        .await
        .unwrap()
        .is_empty());
    assert!(
        collections.find_by_id(collection).await.unwrap().is_some(),
        "合集本身应当还在"
    );
}

#[tokio::test]
async fn listing_and_searching_are_separate_queries_on_purpose() {
    // 刻意**不**做成一个带 `Option<String>` 的方法。
    //
    // `list` 的 `ORDER BY name` 与 `name` 的唯一索引一致，所以不需额外
    // 排序步骤；`search_by_name` 是 `ILIKE '%kw%'`，btree 索引用不上，
    // 走的是完全不同的代价结构。合成一个方法会让「传了 None 时走哪条路」
    // 成为调用方要记住的隐含约定，而那正是两条 SQL 悄悄分叉的起点 ——
    // 本仓库已经吃过一次：`count` 漏了 `AND plugin_key IS NOT NULL`
    // 而 `items` 有，于是 `total` 与 `items.len()` 描述的不是同一个集合。
    let db = TestDb::require().await;
    let repo = VideoCollectionRepository::new(db.pool().clone());
    let stamp = sm_db::common::time::now_utc().and_utc().timestamp();
    let tag = format!("srch{stamp}");

    repo.insert(&NewVideoCollection {
        name: format!("{tag}-alpha"),
        description: String::new(),
    })
    .await
    .unwrap();
    repo.insert(&NewVideoCollection {
        name: format!("{tag}-beta"),
        description: String::new(),
    })
    .await
    .unwrap();
    repo.insert(&NewVideoCollection {
        name: format!("unrelated-{stamp}"),
        description: String::new(),
    })
    .await
    .unwrap();

    let listed = repo.list(page()).await.unwrap();
    assert!(
        listed.total >= 3,
        "list 列出全部（库里还有其他合集），实际 {}",
        listed.total
    );

    let found = repo.search_by_name(&tag, page()).await.unwrap();
    assert_eq!(found.total, 2, "只该命中带 tag 的两个");
    assert_eq!(
        found.total as usize,
        found.items.len(),
        "total 与 items 必须是同一个集合"
    );
    let names: Vec<&str> = found.items.iter().map(|c| c.name.as_str()).collect();
    assert!(names.iter().all(|n| n.contains(&tag)), "{names:?}");

    // 空关键词匹配全部（`ILIKE '%%'`）—— 结果与 `list` 同集，但走的是
    // 另一条 SQL，所以必须显式调用 `search_by_name("")` 才拿得到。
    let all = repo.search_by_name("", page()).await.unwrap();
    assert_eq!(all.total, listed.total, "空关键词与 list 同集");
}
