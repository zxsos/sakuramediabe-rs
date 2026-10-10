//! 缩略图任务队列的**计数与重置**端到端测试。
//!
//! # 为什么必须端到端
//!
//! 这几个数是**运维看板与人工重试的输入**，而它们的条件是三条的并集 + 一个
//! `NOT EXISTS`，光看代码很容易读成「状态 = pending 的数量」——骨架就写错了。
//! 更麻烦的是**错的方向是静默的**：
//!
//! | 条件 | 漏了会怎样 |
//! |---|---|
//! | 候选集含 `succeeded` | 「状态说做完了但产物不在」的媒体**永远修不回来** |
//! | 候选要求「无缩略图」 | 已经有图的媒体每轮被重做一遍（白烧 provider） |
//! | 重置要求「无缩略图」 | 人工重试会把已有产物**删了重做** |
//! | 重置要求 `state = terminal` | 正在退避的媒体被白送一次重试额度 |
//!
//! `TestDb` 每个用例一个独立 schema（见 `sm_db::testing::db` 的文档），所以下面
//! 的计数可以直接断言**绝对值**。
//!
//! 上游出处：`playback/thumbnails/task_service.py`。

mod support;

use chrono::Duration;
use sm_db::playback::media::thumbnail_state;
use sm_db::testing::TestDb;
use sm_service::playback::thumbnails::task_service::MediaThumbnailTaskService;
use support::{seed_image, seed_media, seed_thumbnail};

/// 建一条媒体并把状态机列写成指定值。
async fn seed_media_in_state(
    db: &TestDb,
    state: &str,
    next_retry_at: Option<chrono::NaiveDateTime>,
    valid: bool,
) -> i32 {
    let media_id = seed_media(db, Some(&format!("TTC-{:06}", support::n()))).await;
    sqlx::query(
        "UPDATE media SET thumbnail_generation_state = $1, thumbnail_next_retry_at = $2, \
             thumbnail_attempt_count = 2, thumbnail_deferred_count = 1, valid = $3 \
         WHERE id = $4",
    )
    .bind(state)
    .bind(next_retry_at)
    .bind(valid)
    .bind(media_id)
    .execute(db.pool())
    .await
    .expect("改状态机列");
    media_id
}

fn now() -> chrono::NaiveDateTime {
    sm_db::common::time::now_utc()
}

/// ★ 候选集**包含**「状态说成功、产物却不在」的媒体。
///
/// 这是修复路径：包被删 / 磁盘换了 / 写库成功而落盘失败之后，那个媒体必须能被
/// 重新扫到 —— 否则它永久停在 `succeeded` 而永远没有图。
///
/// 反过来，「已经有缩略图」的媒体**不能**再进候选，否则每轮都白做一遍。
#[tokio::test]
async fn the_candidate_set_includes_succeeded_media_without_artifacts() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());
    assert_eq!(service.count_pending_media().await.expect("基线"), 0);

    // 1. pending、无产物 → 候选
    seed_media_in_state(&db, thumbnail_state::PENDING, None, true).await;
    // 2. succeeded、无产物 → **仍是候选**（修复路径）
    let repaired = seed_media_in_state(&db, thumbnail_state::SUCCEEDED, None, true).await;
    assert_eq!(service.count_pending_media().await.expect("计数"), 2);

    // 3. succeeded、**有产物** → 不再是候选
    let image_id = seed_image(&db, &format!("tasks/{}.webp", support::n())).await;
    seed_thumbnail(&db, repaired, image_id, 0).await;
    assert_eq!(
        service.count_pending_media().await.expect("计数"),
        1,
        "有产物的媒体不该被反复重做"
    );
}

/// 候选集看 `valid`，也看退避窗口是否到期。
#[tokio::test]
async fn the_candidate_set_respects_the_retry_window_and_validity() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());

    // 退避未到 → 不是候选
    seed_media_in_state(
        &db,
        thumbnail_state::RETRY_WAIT,
        Some(now() + Duration::hours(1)),
        true,
    )
    .await;
    // 退避已过 → 是候选
    seed_media_in_state(
        &db,
        thumbnail_state::RETRY_WAIT,
        Some(now() - Duration::minutes(1)),
        true,
    )
    .await;
    // 退避中但 `next_retry_at` 为 NULL → 按「立即可试」算，是候选
    seed_media_in_state(&db, thumbnail_state::RETRY_WAIT, None, true).await;
    // 无效媒体 → 不是候选
    seed_media_in_state(&db, thumbnail_state::PENDING, None, false).await;
    // 终态 → 不是候选（要人工重置）
    seed_media_in_state(&db, thumbnail_state::TERMINAL, None, true).await;

    assert_eq!(
        service.count_pending_media().await.expect("候选计数"),
        2,
        "只有「退避已过」与「next_retry_at 为 NULL」这两条进候选"
    );

    // 状态计数**不加 valid 过滤**（上游也没有）：队列深度要能看见卡住的无效媒体。
    assert_eq!(service.count_retry_wait_media().await.expect("退避中"), 3);
    assert_eq!(
        service.count_terminal_failed_media().await.expect("终态"),
        1
    );
}

/// 状态计数同样要求「无产物」—— 已经有图的媒体不该出现在待办看板上。
#[tokio::test]
async fn the_state_counts_only_cover_media_without_artifacts() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());

    let with_artifacts = seed_media_in_state(
        &db,
        thumbnail_state::RETRY_WAIT,
        Some(now() - Duration::minutes(1)),
        true,
    )
    .await;
    let image_id = seed_image(&db, &format!("tasks/{}.webp", support::n())).await;
    seed_thumbnail(&db, with_artifacts, image_id, 0).await;

    assert_eq!(
        service.count_retry_wait_media().await.expect("退避中"),
        0,
        "已经有产物的媒体不该占着待办看板"
    );
}

/// ★ 重置只碰「终态 + 有效 + 无产物」三类条件都满足的行，且**计数清零**。
#[tokio::test]
async fn reset_only_touches_terminal_valid_media_without_artifacts() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());

    let terminal = seed_media_in_state(&db, thumbnail_state::TERMINAL, None, true).await;
    let retrying = seed_media_in_state(&db, thumbnail_state::RETRY_WAIT, None, true).await;
    let invalid = seed_media_in_state(&db, thumbnail_state::TERMINAL, None, false).await;
    let has_artifacts = seed_media_in_state(&db, thumbnail_state::TERMINAL, None, true).await;
    let image_id = seed_image(&db, &format!("tasks/{}.webp", support::n())).await;
    seed_thumbnail(&db, has_artifacts, image_id, 0).await;

    let affected = service
        .reset_terminal_media(&[terminal, retrying, invalid, has_artifacts])
        .await
        .expect("重置");
    assert_eq!(
        affected, 1,
        "只有「终态 + 有效 + 无产物」那一条该被重置 —— 返回的是**真正改动的行数**"
    );

    let row: (String, i32, i32, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        "SELECT thumbnail_generation_state, thumbnail_attempt_count, \
                thumbnail_deferred_count, thumbnail_next_retry_at \
         FROM media WHERE id = $1",
    )
    .bind(terminal)
    .fetch_one(db.pool())
    .await
    .expect("复查");
    assert_eq!(row.0, thumbnail_state::PENDING, "回到待处理");
    assert_eq!(row.1, 0, "尝试计数必须清零");
    assert_eq!(row.2, 0, "延迟计数必须清零");
    assert!(row.3.is_none(), "退避时刻要清掉");

    // 不该动的三条原样不动。
    for (id, expected_state) in [
        (retrying, thumbnail_state::RETRY_WAIT),
        (invalid, thumbnail_state::TERMINAL),
        (has_artifacts, thumbnail_state::TERMINAL),
    ] {
        let (state,): (String,) =
            sqlx::query_as("SELECT thumbnail_generation_state FROM media WHERE id = $1")
                .bind(id)
                .fetch_one(db.pool())
                .await
                .expect("复查");
        assert_eq!(state, expected_state, "媒体 {id} 不该被重置");
    }
}

/// 重置之后它就**重新进候选**了 —— 这是「人工重试」的完整闭环。
#[tokio::test]
async fn a_reset_media_becomes_a_candidate_again() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());

    let media_id = seed_media_in_state(&db, thumbnail_state::TERMINAL, None, true).await;
    assert_eq!(service.count_pending_media().await.expect("重置前"), 0);

    assert_eq!(
        service
            .reset_terminal_media(&[media_id])
            .await
            .expect("重置"),
        1
    );
    assert_eq!(
        service.count_pending_media().await.expect("重置后"),
        1,
        "重置的意义就在于此：它要能被下一轮扫到"
    );
}

/// 空输入直接返回 0，不发查询（也避免 `ANY('{}')` 这类边界）。
#[tokio::test]
async fn resetting_nothing_is_a_no_op() {
    let db = TestDb::require().await;
    let service = MediaThumbnailTaskService::new(db.pool());
    assert_eq!(service.reset_terminal_media(&[]).await.expect("空输入"), 0);
}
