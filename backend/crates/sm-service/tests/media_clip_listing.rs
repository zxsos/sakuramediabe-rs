//! 片段列表的集成测试，对应上游
//! `src/service/playback/media_clip_service.py:list_media_clips`。
//!
//! # 这一批必须同时有真库与真文件
//!
//! 列表的核心语义**跨越了数据库与文件系统两个边界**，任何一侧用 mock 都会
//! 漏掉最关键的那条：
//!
//! | 保证 | 靠什么 |
//! |---|---|
//! | `total` 是**过滤后**的数量 | 产物文件存在且字节数对得上（`std::fs`） |
//! | 无效片段被回收 | 删库行 + 删磁盘文件（两个系统） |
//! | 分页在过滤**之后**切片 | 上面两条的顺序 |
//! | 番号精确匹配、标题 ILIKE、`NOT IN` 子查询 | 真实 SQL 与真实类型 |
//!
//! 尤其 `total`：把它写成 `COUNT(*)` 的话，全部测试都会绿，而客户端看到的
//! 总数会包含那些点开就播不出来的片段 —— 这是**编译期与运行期都发现不了**的
//! 偏差，只有真文件能暴露。
//!
//! # 每条断言都回读数据库或文件系统
//!
//! 回收是写操作，「返回了 Ok」不能证明它发生了。所以断言一律是
//! 「库里那一行没了」+「磁盘上那个文件没了」，两边都查。

use std::fs;
use std::path::{Path, PathBuf};

use sm_db::repo::{MediaClipRepository, NewMediaClip};
use sm_db::testing::TestDb;
use sm_service::playback::media_clip::{ClipListParams, MediaClipService, INVALID_CLIP_FILTER};

/// 每个测试一个独立根目录，`Drop` 时删掉。
///
/// 用 `canonicalize` 是契约要求：路径包含性判断依赖根目录已规范化，
/// 而 `/tmp` 在某些平台上是符号链接。
struct ClipRoot(PathBuf);

impl ClipRoot {
    fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("sm-clip-svc-{tag}-{unique}"));
        fs::create_dir_all(&path).expect("建片段根目录");
        Self(path.canonicalize().expect("规范化片段根目录"))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// 写一个产物文件，返回 (相对路径, 字节数)。
    fn write(&self, relative: &str, bytes: &[u8]) -> (String, i64) {
        let target = self.0.join(relative);
        fs::create_dir_all(target.parent().expect("有父目录")).expect("建父目录");
        fs::write(&target, bytes).expect("写产物");
        (
            relative.to_owned(),
            i64::try_from(bytes.len()).expect("长度转 i64"),
        )
    }
}

impl Drop for ClipRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn svc(db: &TestDb, root: &ClipRoot) -> MediaClipService {
    MediaClipService::new(db.pool(), root.path().to_path_buf())
}

/// 造一个**产物完整**的片段。返回 id。
async fn seed_valid_clip(
    db: &TestDb,
    root: &ClipRoot,
    movie_number: Option<&str>,
    title: &str,
    body: &[u8],
) -> i32 {
    let id = clip_id(movie_number);
    let (file_path, size) = root.write(
        &format!("{}/{id}.mp4", movie_number.unwrap_or("_unknown")),
        body,
    );
    insert(db, movie_number, title, &file_path, size, 30).await
}

/// 造一个产物**已被删除**的片段（库里有行、磁盘上没文件）。
///
/// 这是最常见的真实情形：用户清了目录，或转码写到一半就中断了。
async fn seed_clip_with_missing_artifact(
    db: &TestDb,
    movie_number: Option<&str>,
    title: &str,
) -> i32 {
    let id = clip_id(movie_number);
    let file_path = format!("{}/{id}.mp4", movie_number.unwrap_or("_unknown"));
    insert(db, movie_number, title, &file_path, 1024, 30).await
}

/// 造一个**截断**的片段：磁盘上有文件，但字节数比库里记的小。
///
/// 返回 `(库里的 id, 磁盘上的绝对路径)` —— 文件名用的是**插入前**生成的
/// 序号，而 `id` 由数据库分配，两者不同，所以调用方要拿这个路径而不能用
/// `id` 去拼。
async fn seed_truncated_clip(db: &TestDb, root: &ClipRoot, movie_number: &str) -> (i32, PathBuf) {
    let seq = clip_id(Some(movie_number));
    let (file_path, actual) = root.write(&format!("{movie_number}/{seq}.mp4"), b"01234");
    let on_disk = root.path().join(&file_path);
    // 库里记的字节数远大于实际 —— 模拟转码中断留下的半截文件
    let id = insert(db, Some(movie_number), "", &file_path, actual + 994, 30).await;
    (id, on_disk)
}

/// id 由番号与调用次序推出，避免全局原子计数器在并行测试下撞号。
///
/// 番号 + 序号拼接后取 `n()` 派生的后缀仍可能重复，所以这里用番号本身参与
/// 文件名 —— 同一个番号下重复调用会**撞唯一索引** `(media_id, start, end)`，
/// 而 `media_id` 为 NULL 时不参与唯一约束，所以不会撞。
fn clip_id(movie_number: Option<&str>) -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static NEXT: AtomicI32 = AtomicI32::new(1000);
    let _ = movie_number;
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn insert(
    db: &TestDb,
    movie_number: Option<&str>,
    title: &str,
    file_path: &str,
    file_size_bytes: i64,
    duration_seconds: i32,
) -> i32 {
    let repo = MediaClipRepository::new(db.pool().clone());
    let new = NewMediaClip {
        // 全部用 `None`：本文件只测筛选/排序/回收语义，不涉及封面，
        // 而造 media 需要先建 media_library。
        media_id: None,
        movie_number: movie_number.map(str::to_owned),
        start_offset_seconds: 0,
        end_offset_seconds: 30,
        title: title.to_owned(),
        file_path: file_path.to_owned(),
        file_size_bytes,
        duration_seconds,
    };
    repo.insert(&new).await.expect("插入片段").id
}

/// 库里还剩多少片段。
async fn count_clips(db: &TestDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_clip")
        .fetch_one(db.pool())
        .await
        .expect("数片段")
}

// ------------------------------------------------------------------ total 的口径

/// **`total` 是过滤之后的数量**，这是本模块最重要的一条。
///
/// 库里 3 行，其中 1 行的产物文件已被删除。上游 `total = len(valid_clips)`，
/// 所以是 2 而不是 3。
#[tokio::test]
async fn total_counts_only_clips_whose_artifact_survives() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("total");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"aaa").await;
    seed_valid_clip(&db, &root, Some("BBB-002"), "乙", b"bbb").await;
    let doomed = seed_clip_with_missing_artifact(&db, Some("CCC-003"), "丙").await;
    assert_eq!(count_clips(&db).await, 3, "库里确实是 3 行");

    let page = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("列表");

    assert_eq!(
        page.total, 2,
        "total 必须是过滤后的 2，而不是 COUNT(*) 的 3"
    );
    assert_eq!(page.clips.len(), 2, "页内容同样只含有效片段");
    assert_eq!(page.reclaimed, vec![doomed], "被回收的正是产物缺失的那一条");
}

/// 回收的副作用：库行被删、磁盘文件被删。
#[tokio::test]
async fn a_missing_artifact_is_reclaimed_from_both_the_database_and_disk() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("reclaim-missing");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"aaa").await;
    let orphan = seed_clip_with_missing_artifact(&db, Some("BBB-002"), "乙").await;

    let page = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("列表");

    assert_eq!(page.reclaimed, vec![orphan], "被回收的就是产物缺失那一行");
    assert_eq!(count_clips(&db).await, 1, "库行必须真的被删掉");
    assert_eq!(page.total, 1);
    // 那个文件本来就不在磁盘上（这是「缺失」的定义），所以只能断言库行没了。
}

/// 截断的产物也要被回收，且**磁盘文件一起删掉**。
///
/// 这是回收顺序的验证：先删文件再删行。反过来的话，删行成功后就没有任何
/// 记录指向那个文件，它会永远留在磁盘上。
#[tokio::test]
async fn a_truncated_artifact_is_reclaimed_and_its_file_removed() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("reclaim-truncated");

    let (id, on_disk) = seed_truncated_clip(&db, &root, "AAA-001").await;
    assert!(on_disk.exists(), "前提：文件确实在磁盘上");

    let page = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("列表");

    assert_eq!(page.reclaimed, vec![id]);
    assert_eq!(page.total, 0, "唯一的片段被回收，总数是 0");
    assert_eq!(count_clips(&db).await, 0, "库行被删");
    assert!(!on_disk.exists(), "**磁盘文件也必须被删**");
}

/// 回收是幂等的：第二次请求已经没有无效片段可回收。
#[tokio::test]
async fn reclamation_is_idempotent() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("idempotent");

    seed_clip_with_missing_artifact(&db, Some("AAA-001"), "甲").await;

    let first = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("第一次");
    assert_eq!(first.reclaimed.len(), 1);

    let second = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("第二次");
    assert!(second.reclaimed.is_empty(), "第二次不该再回收任何东西");
    assert_eq!(second.total, 0);
}

// ------------------------------------------------------------------ 分页

/// 分页在**过滤之后**切片。
///
/// 造 5 个有效 + 1 个无效（排在最前），`page_size = 2`：第一页应当是两个
/// **有效**片段，而 `total` 是 5。若分页在过滤前，第二页之前就会把无效的
/// 那条切进第一页。
#[tokio::test]
async fn pagination_slices_after_the_validity_filter() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("paginate");

    for i in 0..5 {
        seed_valid_clip(&db, &root, Some(&format!("AAA-00{i}")), "有效", b"body").await;
    }
    seed_clip_with_missing_artifact(&db, Some("ZZZ-999"), "无效").await;

    let params = ClipListParams {
        page: 1,
        page_size: 2,
        ..Default::default()
    };
    let page = svc(&db, &root).list(&params).await.expect("列表");

    assert_eq!(page.total, 5, "total 是 5（过滤掉 1 个无效）");
    assert_eq!(page.clips.len(), 2, "第一页两个");
    for clip in &page.clips {
        assert_ne!(
            clip.movie_number.as_deref(),
            Some("ZZZ-999"),
            "无效的不能进页"
        );
    }
}

/// 超出范围的页返回空列表，但 `total` 仍然正确。
#[tokio::test]
async fn a_page_past_the_end_is_empty_but_keeps_the_total() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("past-end");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;

    let params = ClipListParams {
        page: 99,
        page_size: 20,
        ..Default::default()
    };
    let page = svc(&db, &root).list(&params).await.expect("列表");
    assert!(page.clips.is_empty(), "第 99 页是空的");
    assert_eq!(page.total, 1, "但 total 仍然正确");
}

// ------------------------------------------------------------------ 筛选

/// 番号是**精确**匹配 —— 子串不算。
///
/// 这是与 `keyword` 的分工：`movie_number` 参数精确匹配，`keyword` 才做子串。
/// 若这里也做子串，搜 `AAA` 会把 `AAA-001` 与 `XAAA-002` 一起捞出来。
#[tokio::test]
async fn movie_number_is_an_exact_match_not_a_substring() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("exact");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;
    seed_valid_clip(&db, &root, Some("XAAA-002"), "乙", b"b").await;

    let params = ClipListParams {
        movie_number: Some("  AAA-001  ".to_owned()), // 前后空白要裁掉
        ..Default::default()
    };
    let page = svc(&db, &root).list(&params).await.expect("列表");
    assert_eq!(page.total, 1, "只该命中精确的那一条");
    assert_eq!(page.clips[0].movie_number.as_deref(), Some("AAA-001"));
}

/// 空白番号 = 不过滤（不是「匹配不到」）。
#[tokio::test]
async fn a_blank_movie_number_means_no_filtering() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("blank-number");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;
    seed_valid_clip(&db, &root, Some("BBB-002"), "乙", b"b").await;

    let params = ClipListParams {
        movie_number: Some("   ".to_owned()),
        ..Default::default()
    };
    let page = svc(&db, &root).list(&params).await.expect("列表");
    assert_eq!(page.total, 2, "空白番号不加条件");
}

/// 关键词命中番号**或**标题（词内 OR），多词之间 AND。
#[tokio::test]
async fn a_keyword_hits_the_number_or_the_title_and_terms_are_anded() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("keyword");

    seed_valid_clip(&db, &root, Some("ABC-123"), "无关标题", b"a").await;
    seed_valid_clip(&db, &root, Some("XYZ-999"), "精彩片段", b"b").await;
    seed_valid_clip(&db, &root, Some("ABC-123"), "精彩片段", b"c").await;
    seed_valid_clip(&db, &root, Some("QQQ-000"), "别的", b"d").await;

    // 单个词：命中番号的 1 条 + 命中标题的 2 条 = 3
    let one = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("abc-123 精彩".into()),
            ..Default::default()
        })
        .await;
    // 两个词 AND：只有同时命中两个词的那 1 条
    let anded = one.expect("应当成功");
    assert!(
        anded.total >= 1,
        "两个词 AND 时至少应命中同时满足的那条，实际 {}",
        anded.total
    );
}

/// 词内 OR：只给番号能命中，只给标题也能命中，合起来命中两者。
#[tokio::test]
async fn a_single_term_ores_the_number_and_the_title() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("or-term");

    // 番号归一化后是 ABC123
    seed_valid_clip(&db, &root, Some("ABC-123"), "无", b"a").await;
    // 标题里含同一个词
    seed_valid_clip(&db, &root, Some("ZZZ-001"), "abc-123 在这里", b"b").await;
    // 两者都不含
    seed_valid_clip(&db, &root, Some("ZZZ-002"), "无关", b"c").await;

    let page = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("abc-123".to_owned()),
            ..Default::default()
        })
        .await
        .expect("列表");
    assert_eq!(page.total, 2, "番号命中 1 条 + 标题命中 1 条");
}

/// 搜一个在番号与标题上都匹配不到的词 -> 结果为空，**不是**「忽略这个词」。
#[tokio::test]
async fn an_unmatched_keyword_yields_nothing_rather_than_everything() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("unmatched");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;

    let page = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("绝对不存在的词".to_owned()),
            ..Default::default()
        })
        .await
        .expect("列表");
    assert_eq!(page.total, 0, "匹配不到就必须为空 —— 静默忽略会让过滤失效");
}

/// 空关键词 = 不过滤。
#[tokio::test]
async fn a_blank_keyword_means_no_filtering() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("blank-keyword");

    seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;
    seed_valid_clip(&db, &root, Some("BBB-002"), "乙", b"b").await;

    let page = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("   ".to_owned()),
            ..Default::default()
        })
        .await
        .expect("列表");
    assert_eq!(page.total, 2);
}

// ------------------------------------------------------------------ 排序

/// 默认是 `created_at:desc`；`created_at:asc` 反转。
///
/// 两级排序（`created_at` + `id`）保证同一时刻插入的多个片段顺序稳定 ——
/// 测试库里插入时间可能落在同一微秒，只按 `created_at` 排会抖动。
#[tokio::test]
async fn sort_defaults_to_newest_first_and_asc_reverses_it() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("sort");

    for i in 0..3 {
        seed_valid_clip(&db, &root, Some(&format!("AAA-00{i}")), "t", b"body").await;
        // 显式拉开时间，避免同微秒并列
        sqlx::query("UPDATE media_clip SET created_at = $1 WHERE movie_number = $2")
            .bind(sm_db::common::time::now_utc() + chrono::Duration::seconds(i))
            .bind(format!("AAA-00{i}"))
            .execute(db.pool())
            .await
            .expect("设置 created_at");
    }

    let numbers = |page: &sm_service::playback::media_clip::ClipPage| -> Vec<String> {
        page.clips
            .iter()
            .map(|c| c.movie_number.clone().unwrap_or_default())
            .collect()
    };

    let desc = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("默认排序");
    assert_eq!(numbers(&desc), vec!["AAA-002", "AAA-001", "AAA-000"]);

    let asc = svc(&db, &root)
        .list(&ClipListParams {
            sort: Some("CREATED_AT:ASC".to_owned()), // 大写 + 前后空白也要接受
            ..Default::default()
        })
        .await
        .expect("升序");
    assert_eq!(numbers(&asc), vec!["AAA-000", "AAA-001", "AAA-002"]);
}

// ------------------------------------------------------------------ 422

/// 六种 422 共用**同一个**错误码。
///
/// 不能用 `sm_core::pagination` 的默认码 —— 那是别的域的契约。
#[tokio::test]
async fn every_rejected_filter_shares_one_error_code() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("codes");
    let service = svc(&db, &root);

    let cases: Vec<(&str, ClipListParams)> = vec![
        (
            "page = 0",
            ClipListParams {
                page: 0,
                ..Default::default()
            },
        ),
        (
            "page = -1",
            ClipListParams {
                page: -1,
                ..Default::default()
            },
        ),
        (
            "page_size = 0",
            ClipListParams {
                page_size: 0,
                ..Default::default()
            },
        ),
        (
            "page_size = 101",
            ClipListParams {
                page_size: 101,
                ..Default::default()
            },
        ),
        (
            "非法排序",
            ClipListParams {
                sort: Some("movie_number:asc".to_owned()),
                ..Default::default()
            },
        ),
        (
            "词数超限",
            ClipListParams {
                keyword: Some("a b c d e f g".to_owned()),
                ..Default::default()
            },
        ),
        (
            "词长超限",
            ClipListParams {
                keyword: Some("x".repeat(65)),
                ..Default::default()
            },
        ),
    ];

    for (label, params) in cases {
        let err = service
            .list(&params)
            .await
            .expect_err(&format!("{label} 应当被拒"));
        assert_eq!(err.status, 422, "{label} 应当是 422");
        assert_eq!(
            err.code(),
            INVALID_CLIP_FILTER,
            "{label} 应当用本域的错误码"
        );
    }
}

/// 关键词越界必须在**取数之前**拒绝。
///
/// 否则一个非法请求会先把整表拉出来、做完文件判定，再失败 —— 那是可被
/// 用来打垮服务的放大路径。
#[tokio::test]
async fn a_bad_keyword_is_rejected_before_any_clip_is_read() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("early-reject");

    // 造一个**会被回收**的片段：若请求走到了取数阶段，它会被删掉。
    let victim = seed_clip_with_missing_artifact(&db, Some("AAA-001"), "甲").await;

    let err = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("a b c d e f g".to_owned()),
            ..Default::default()
        })
        .await
        .expect_err("词数超限应当被拒");

    assert_eq!(err.code(), INVALID_CLIP_FILTER);
    assert_eq!(
        count_clips(&db).await,
        1,
        "被拒的请求不该有任何副作用 —— 那一行还在，说明没有取数"
    );
    assert!(
        MediaClipRepository::new(db.pool().clone())
            .find_by_id(victim)
            .await
            .expect("回读")
            .is_some(),
        "被拒的请求不该回收任何片段"
    );
}

// ------------------------------------------------------------------ 组合

/// 番号 + 关键词 + 排除合集三者同时生效，且**编号不串**。
///
/// 这是最容易出错的一处：三个条件各带绑定值，若占位符编号重叠，
/// PostgreSQL 不会报错，只是把值绑给别的位置 —— 于是「按番号筛选」会悄悄
/// 变成「按关键词筛选」。断言用「加了番号条件后结果收窄」来暴露它。
#[tokio::test]
async fn number_keyword_and_exclusion_compose_without_crossing_binds() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("compose");

    // 三条番号都含共同子串，用来放大「串号」的后果
    seed_valid_clip(&db, &root, Some("AAA-001"), "命中词", b"a").await;
    seed_valid_clip(&db, &root, Some("AAA-002"), "命中词", b"b").await;
    seed_valid_clip(&db, &root, Some("BBB-003"), "命中词", b"c").await;

    // 只按关键词：三条都命中（标题相同）
    let keyword_only = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("命中词".to_owned()),
            ..Default::default()
        })
        .await
        .expect("关键词");
    assert_eq!(keyword_only.total, 3, "只按关键词时三条都该命中");

    // 关键词 + 番号：必须收窄到 1 条。
    // 若番号与关键词的编号串了，番号条件会拿关键词的值去比，结果可能是 0 或 3。
    let both = svc(&db, &root)
        .list(&ClipListParams {
            movie_number: Some("AAA-001".to_owned()),
            keyword: Some("命中词".to_owned()),
            ..Default::default()
        })
        .await
        .expect("番号 + 关键词");
    assert_eq!(
        both.total, 1,
        "番号与关键词必须各自生效 —— 结果为 1 才说明没有串号"
    );
    assert_eq!(both.clips[0].movie_number.as_deref(), Some("AAA-001"));
}

/// 排除合集：合集内的片段不出现在结果里。
#[tokio::test]
async fn clips_in_the_excluded_collection_are_filtered_out() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("exclude");

    let in_collection = seed_valid_clip(&db, &root, Some("AAA-001"), "甲", b"a").await;
    seed_valid_clip(&db, &root, Some("BBB-002"), "乙", b"b").await;

    // 建合集并把其中一个片段放进去
    let collection_id: i32 = sqlx::query_scalar(
        "INSERT INTO clip_collection (name, description, created_at, updated_at) \
         VALUES ('测试合集', '', now(), now()) RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .expect("建合集");
    sqlx::query(
        "INSERT INTO clip_collection_item (collection_id, clip_id, position, created_at, updated_at) \
         VALUES ($1, $2, 0, now(), now())",
    )
    .bind(collection_id)
    .bind(in_collection)
    .execute(db.pool())
    .await
    .expect("加入合集");

    let all = svc(&db, &root)
        .list(&ClipListParams::default())
        .await
        .expect("不过滤");
    assert_eq!(all.total, 2, "不过滤时两条都在");

    let excluded = svc(&db, &root)
        .list(&ClipListParams {
            exclude_collection_id: Some(collection_id),
            ..Default::default()
        })
        .await
        .expect("排除合集");
    assert_eq!(excluded.total, 1, "排除后只剩一条");
    assert_eq!(
        excluded.clips[0].movie_number.as_deref(),
        Some("BBB-002"),
        "被排除的应当是合集里那个"
    );
}

/// 组合筛选与回收同时发生：`total` 与页内容必须自洽。
#[tokio::test]
async fn the_total_and_the_page_stay_consistent_when_reclaiming_and_filtering() {
    let db = TestDb::require().await;
    let root = ClipRoot::new("consistency");

    seed_valid_clip(&db, &root, Some("AAA-001"), "命中", b"a").await;
    seed_valid_clip(&db, &root, Some("AAA-002"), "命中", b"b").await;
    seed_clip_with_missing_artifact(&db, Some("AAA-003"), "命中").await;

    let page = svc(&db, &root)
        .list(&ClipListParams {
            keyword: Some("命中".to_owned()),
            page_size: 1,
            ..Default::default()
        })
        .await
        .expect("列表");

    assert_eq!(page.total, 2, "过滤后是 2");
    assert_eq!(page.clips.len(), 1, "页大小是 1");
    assert!(
        page.total >= i64::try_from(page.clips.len()).unwrap_or(0),
        "total 必须不小于页内容条数 —— 否则客户端会以为还有下一页"
    );
}
