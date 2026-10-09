//! 媒体技术信息回填的**端到端**测试：真库 + 可编程的假探测网关。
//!
//! # 为什么必须连库
//!
//! 这个服务的难点全在「**条件化三连写**」上：三列各有自己的 WHERE 守卫
//! （`video_info` 乐观并发、时长仍 ≤ 0、分辨率仍为空），任何一列落空都不
//! 算失败，而「updated 但仍缺」与「没写进但也已经不缺」必须分得开 ——
//! 这些只有真库才验得到。
//!
//! 上游出处：`playback/media_video_info_backfill_service.py`（233 行）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

/// `await_holding_lock` 的豁免见文件顶部 `SERIAL` 的说明。
use sm_db::testing::TestDb;
use sm_service::playback::media_video_info_backfill::MediaVideoInfoBackfillService;
use sm_service::playback::provider_helpers::{
    MediaHandle, ProviderFailure, StorageGateway, PROVIDER_UNSUPPORTED,
};

mod support;

/// 串行锁。理由与 `media_file_hash_backfill` 套件同一条：advisory lock 的键
/// 是 `(namespace, media_id)`，不区分 schema，而每个测试 schema 的
/// `media.id` 都从 1 开始 —— 并行会互相占锁。生产无此问题。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 可编程的探测桩：按 `media_id` 回探测字典。
struct ScriptedProbe {
    script: HashMap<i64, Result<Value, ()>>,
}

impl StorageGateway for ScriptedProbe {
    fn has_provider(&self, _provider_key: &str) -> bool {
        true
    }

    fn delete_media(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不删媒体")
    }

    fn generate_thumbnails(
        &self,
        _handle: &MediaHandle,
        _workspace: &std::path::Path,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        sm_service::playback::provider_helpers::ThumbnailJobResult,
                        ProviderFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        unimplemented!("本套件不生成缩略图")
    }

    fn compute_file_hash(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不算哈希")
    }

    fn scan_managed_media_ref_keys(
        &self,
        _library: &sm_service::playback::provider_helpers::LibraryHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不对账")
    }

    fn managed_media_ref_key(
        &self,
        _library: &sm_service::playback::provider_helpers::LibraryHandle,
        _media_ref: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不对账")
    }

    fn merged_playback_format(&self, _provider_key: &str) -> Option<String> {
        // 本套件不做合并播放。
        None
    }

    fn probe_video_info(
        &self,
        handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ProviderFailure>> + Send + '_>> {
        let outcome = self
            .script
            .get(&handle.media_id)
            .cloned()
            .unwrap_or_else(|| Ok(json!({})));
        Box::pin(async move {
            match outcome {
                Ok(info) => Ok(info),
                Err(()) => Err(ProviderFailure {
                    code: PROVIDER_UNSUPPORTED.to_owned(),
                    safe_message: "桩：不支持探测".to_owned(),
                    retryable: false,
                }),
            }
        })
    }
}

/// 一份「完整」的探测结果：时长 + 分辨率都有。
fn full_probe() -> Value {
    json!({
        "container": {"duration_seconds": 120, "format_name": "matroska"},
        "video": {"width": 1920, "height": 1080, "codec": "h264"}
    })
}

async fn set_duration_zero(db: &TestDb, media_id: i32) {
    sqlx::query("UPDATE media SET duration_seconds = 0 WHERE id = $1")
        .bind(media_id)
        .execute(db.pool())
        .await
        .expect("把时长清成 0");
}

async fn row_of(db: &TestDb, media_id: i32) -> (Option<String>, i32, Option<String>) {
    sqlx::query_as::<_, (Option<String>, i32, Option<String>)>(
        "SELECT video_info, duration_seconds, resolution FROM media WHERE id = $1",
    )
    .bind(media_id)
    .fetch_one(db.pool())
    .await
    .expect("读回 media 行")
}

async fn service(db: &TestDb, probe: ScriptedProbe) -> MediaVideoInfoBackfillService {
    // 两条连接：媒体锁占一条，库操作走另一条（见哈希回填套件的注释）。
    let pool = db.pool_with_max_connections(2).await;
    MediaVideoInfoBackfillService::new(&pool, Arc::new(probe))
}

/// ★ 完整探测结果：三列都落库；第二轮候选为空。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_full_probe_backfills_all_three_columns() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("PROBE-FULL")).await;
    set_duration_zero(&db, media_id).await;

    let script = HashMap::from([(i64::from(media_id), Ok(full_probe()))]);
    let stats = service(&db, ScriptedProbe { script })
        .await
        .backfill_missing_video_infos(None)
        .await
        .expect("跑完");

    assert_eq!(stats.examined, 1);
    assert_eq!(stats.updated, 1);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.incomplete_media, 0, "三列都补齐了");
    let (video_info, duration, resolution) = row_of(&db, media_id).await;
    assert_eq!(duration, 120, "时长来自 container.duration_seconds");
    assert_eq!(resolution.as_deref(), Some("1920x1080"));
    let info: Value =
        serde_json::from_str(video_info.as_deref().expect("video_info 已写")).expect("是 JSON");
    assert_eq!(info["video"]["codec"], "h264", "整份透传");
}

/// ★ provider 不支持探测：`skipped_unsupported`，不是失败，也不重试出死循环。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_unsupported_provider_is_skipped_not_failed() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("PROBE-UNSUPPORTED")).await;

    let script = HashMap::from([(i64::from(media_id), Err(()))]);
    let stats = service(&db, ScriptedProbe { script })
        .await
        .backfill_missing_video_infos(None)
        .await
        .expect("跑完");

    assert_eq!(stats.skipped_unsupported, 1, "合法状态，不是失败");
    assert_eq!(stats.failed, 0);
    let (video_info, _, _) = row_of(&db, media_id).await;
    assert!(video_info.is_none(), "不支持探测时不能写脏数据");
}

/// ★ 探测结果为空对象：`failed`（上游 `ValueError("provider returned no
/// valid media video info")`），且不写任何列。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_empty_probe_result_is_a_failure() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("PROBE-EMPTY")).await;

    let script = HashMap::from([(i64::from(media_id), Ok(json!({})))]);
    let stats = service(&db, ScriptedProbe { script })
        .await
        .backfill_missing_video_infos(None)
        .await
        .expect("跑完");

    assert_eq!(stats.failed, 1);
    assert_eq!(stats.updated, 0);
    let (video_info, _, _) = row_of(&db, media_id).await;
    assert!(video_info.is_none());
}

/// ★ 残缺的探测结果：能写多少写多少（`updated`），但仍缺 → `incomplete`。
///
/// 时长被清成 0、探测结果里没有 `container.duration_seconds` → 那一列的
/// WHERE（`duration_seconds <= 0`）仍命中但没有新值可写 → 行 remains 缺。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_partial_probe_updates_what_it_can_and_counts_as_incomplete() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("PROBE-PARTIAL")).await;
    set_duration_zero(&db, media_id).await;

    let script = HashMap::from([(
        i64::from(media_id),
        Ok(json!({"video": {"width": 3840, "height": 2160}})),
    )]);
    let stats = service(&db, ScriptedProbe { script })
        .await
        .backfill_missing_video_infos(None)
        .await
        .expect("跑完");

    assert_eq!(stats.updated, 1, "video_info 与 resolution 写进去了");
    assert_eq!(stats.incomplete_media, 1, "时长仍缺");
    let (video_info, duration, resolution) = row_of(&db, media_id).await;
    assert_eq!(duration, 0, "探测结果里没有时长，不该编一个");
    assert_eq!(resolution.as_deref(), Some("3840x2160"));
    assert!(
        video_info.is_some(),
        "残缺的结果也值得存（更全才覆盖的判据）"
    );
}

/// ★ 已有探测结果、新的**不更全**：不覆盖（`info_leaves` 判据），也不算失败。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_thinner_probe_does_not_overwrite_a_richer_one() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("PROBE-THINNER")).await;
    set_duration_zero(&db, media_id).await;
    let rich = json!({
        "container": {"duration_seconds": 120},
        "video": {"width": 1920, "height": 1080}
    });
    sqlx::query("UPDATE media SET video_info = $1, duration_seconds = 120, resolution = '1920x1080' WHERE id = $2")
        .bind(serde_json::to_string(&rich).expect("序列化"))
        .bind(media_id)
        .execute(db.pool())
        .await
        .expect("预置完整探测结果");

    let script = HashMap::from([(i64::from(media_id), Ok(json!({"container": {}})))]);
    let stats = service(&db, ScriptedProbe { script })
        .await
        .backfill_missing_video_infos(None)
        .await
        .expect("跑完");

    // 注意：duration/resolution 都不缺、video_info 非 NULL → **候选里就不该有它**。
    assert_eq!(stats.examined, 0, "三段条件全不命中，连探测都不该发生");
    assert_eq!(stats.updated, 0);
}
