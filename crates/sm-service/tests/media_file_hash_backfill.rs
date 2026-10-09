//! 媒体文件哈希回填的**端到端**测试：真库 + 可编程的假网关。
//!
//! # 为什么必须连库
//!
//! 这个服务的正确性一半在**库状态机**上：候选是「`IS NULL` 或空串」、
//! 每条处理前要**重读**（快照可能过期）、锁被占是跳过。光看代码看不出
//! 「重读时发现已被补上」与「锁被占」为什么都落进 `skipped_media` 却
//! 走完全不同的分支。
//!
//! 上游出处：`playback/media_file_hash_backfill_service.py`（113 行）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use sm_db::testing::TestDb;
use sm_service::playback::media_file_hash_backfill::MediaFileHashBackfillService;
use sm_service::playback::provider_helpers::{MediaHandle, ProviderFailure, StorageGateway};

mod support;

/// 本文件的用例**串行**跑。
///
/// # 为什么必须串行（而别的套件不用）
///
/// advisory lock 的键是 `(namespace, media_id)`，**不区分 schema** ——
/// `pg_try_advisory_lock` 是数据库级的。每个用例的 `media.id` 都从 1 开始
/// （每个 schema 自己的序列），并行时 A 用例持有的媒体 1 的锁会把 B 用例的
/// 媒体 1 挡住，B 拿到 `Ok(None)` 走「锁被占 → skipped_media」分支，于是
/// `hashed == 0`，表现为**随机的**断言失败（`backfills_only` 单跑三次全绿、
/// 并行必炸的那种）。
///
/// 生产没有这个问题：一个部署一个 schema，media.id 全库唯一。
/// 所以串行是**夹具的事**，不是实现的事 —— 别因此去给锁键加 schema 前缀
/// （那要改 namespace 常量，影响真实部署）。
///
/// # `await_holding_lock` 的豁免是**故意的**
///
/// 这把锁的目的就是**跨整个用例（含所有 await 点）**互斥；在每个 await 前
/// 放掉再拿回等于没串行。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 可编程的哈希桩：按 `media_id` 给答案，没写进剧本的回一个**合法**哈希。
struct ScriptedGateway {
    /// `Err(())` = provider 失败；`Ok("")` = 回了非法哈希。
    script: HashMap<i64, Result<String, ()>>,
    /// 收到的 media_id，按调用顺序。
    calls: Mutex<Vec<i64>>,
}

impl ScriptedGateway {
    fn new(script: HashMap<i64, Result<String, ()>>) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl StorageGateway for ScriptedGateway {
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

    fn probe_video_info(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ProviderFailure>> + Send + '_>> {
        // 信息回填有自己的桩，本套件不探测。
        unimplemented!("ScriptedGateway 不探测")
    }

    fn scan_managed_media_ref_keys(
        &self,
        _library: &sm_service::playback::provider_helpers::LibraryHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProviderFailure>> + Send + '_>> {
        unimplemented!("ScriptedGateway 不对账")
    }

    fn managed_media_ref_key(
        &self,
        _library: &sm_service::playback::provider_helpers::LibraryHandle,
        _media_ref: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        unimplemented!("ScriptedGateway 不对账")
    }

    fn merged_playback_format(&self, _provider_key: &str) -> Option<String> {
        // 本套件不做合并播放。
        None
    }

    fn compute_file_hash(
        &self,
        handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        self.calls.lock().expect("锁").push(handle.media_id);
        let outcome = self
            .script
            .get(&handle.media_id)
            .cloned()
            .unwrap_or_else(|| Ok(format!("media-file-hash-v1:{:040x}", handle.media_id)));
        Box::pin(async move {
            match outcome {
                Ok(hash) => Ok(hash),
                Err(()) => Err(ProviderFailure {
                    code: "unavailable".to_owned(),
                    safe_message: "桩：读不到".to_owned(),
                    retryable: true,
                }),
            }
        })
    }
}

async fn set_hash(db: &TestDb, media_id: i32, hash: &str) {
    sqlx::query("UPDATE media SET file_hash = $1 WHERE id = $2")
        .bind(hash)
        .bind(media_id)
        .execute(db.pool())
        .await
        .expect("写 file_hash");
}

async fn hash_of(db: &TestDb, media_id: i32) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT file_hash FROM media WHERE id = $1")
        .bind(media_id)
        .fetch_one(db.pool())
        .await
        .expect("读 file_hash")
}

/// 两条连接：回填会取一条**会话级** advisory lock 并持有它做后续的库操作
/// —— 锁占一条连接，池里（默认 1 条）就一条不剩了。见
/// `support` 里删除类用例的同款注释与 `sm_db::common::advisory_lock` 的文档。
async fn service(db: &TestDb, gateway: ScriptedGateway) -> MediaFileHashBackfillService {
    let pool = db.pool_with_max_connections(2).await;
    MediaFileHashBackfillService::new(&pool, Arc::new(gateway))
}

/// ★ 只补缺的：已有哈希的行**不在候选里**，更不会被覆盖。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn backfills_only_the_missing_ones() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let filled = support::seed_media(&db, Some("HASH-FILLED")).await;
    let missing = support::seed_media(&db, Some("HASH-MISSING")).await;
    set_hash(&db, filled, "existing-hash").await;

    let service = service(&db, ScriptedGateway::new(HashMap::new())).await;
    let stats = service
        .backfill_missing_file_hashes(None)
        .await
        .expect("跑完");

    assert_eq!(stats.examined, 1, "候选只有缺哈希的那条");
    assert_eq!(stats.hashed, 1);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.skipped_media, 0);
    let hash = hash_of(&db, missing).await.expect("行在");
    assert!(
        hash.starts_with("media-file-hash-v1:"),
        "写进去的是 provider 给的哈希：{hash}"
    );
    assert_eq!(
        hash_of(&db, filled).await.as_deref(),
        Some("existing-hash"),
        "已有哈希的行不能被动"
    );
}

/// ★ provider 回**空白哈希**：记 `invalid_hash`，**不写库**（否则那行永远
/// 不会再被回填），也不是 `failed`。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_blank_hash_is_invalid_and_not_written() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("HASH-BLANK")).await;

    let script = HashMap::from([(i64::from(media_id), Ok(String::new()))]);
    let service = service(&db, ScriptedGateway::new(script)).await;
    let stats = service
        .backfill_missing_file_hashes(None)
        .await
        .expect("跑完");

    assert_eq!(
        stats.invalid_hash, 1,
        "与 failed 分开：这是 provider 的 bug"
    );
    assert_eq!(stats.hashed, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(hash_of(&db, media_id).await, None, "空哈希不能落库");
}

/// ★ provider 失败：记 `failed`，单条失败**不中断**整批。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_provider_failure_does_not_stop_the_batch() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let broken = support::seed_media(&db, Some("HASH-BROKEN")).await;
    let good = support::seed_media(&db, Some("HASH-GOOD")).await;

    let script = HashMap::from([(i64::from(broken), Err(()))]);
    let service = service(&db, ScriptedGateway::new(script)).await;
    let stats = service
        .backfill_missing_file_hashes(None)
        .await
        .expect("跑完");

    assert_eq!(stats.examined, 2);
    assert_eq!(stats.hashed, 1, "失败之后的那条照常处理");
    assert_eq!(stats.failed, 1);
    assert_eq!(hash_of(&db, broken).await, None);
    assert!(hash_of(&db, good)
        .await
        .expect("成功那条有哈希")
        .starts_with("media-file-hash-v1:"));
}

/// ★ 跑完一轮之后**再跑一轮**：候选为空，provider 一次都不被调。
///
/// # 为什么不测「候选快照过期」那条分支
///
/// 服务的重读逻辑（`process_one` 里的 `filter`）确实存在 —— 上游在锁内用
/// 带条件的查询重读（`:74-79`）。但它的触发前提是「候选列表生成**之后**、
/// 轮到这条**之前**，别人把哈希补上了」，而服务每次跑都**重算候选**
/// （上游也是 `_candidate_ids()` 每轮现查）—— 从公共 API 上构造不出这个
/// 窗口，除非真起并发写者。假造一个（先补哈希再跑）只会测到「候选为空」，
/// 与本用例重复还更绕。所以这里锁的是**可观察**的契约：第二轮不再碰 provider。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_second_run_after_a_full_backfill_examines_nothing() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("HASH-RACE")).await;

    let gateway = Arc::new(ScriptedGateway::new(HashMap::new()));
    let pool = db.pool_with_max_connections(2).await;
    let service = MediaFileHashBackfillService::new(&pool, gateway.clone());

    let first = service
        .backfill_missing_file_hashes(None)
        .await
        .expect("第一轮");
    assert_eq!(first.hashed, 1);
    assert_eq!(
        first.examined, 1,
        "第一轮的 examined 是候选数，不是处理数的一半"
    );

    let second = service
        .backfill_missing_file_hashes(None)
        .await
        .expect("第二轮");
    assert_eq!(second.examined, 0, "都补上了，没有候选");
    assert_eq!(second.hashed, 0);
    assert_eq!(
        gateway.calls.lock().expect("锁").len(),
        1,
        "provider 只在第一轮被调过一次 —— 第二轮连问都不问"
    );
    assert!(hash_of(&db, media_id)
        .await
        .expect("哈希在")
        .starts_with("media-file-hash-v1:"));
}
