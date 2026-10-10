//! 缩略图任务的心跳（上游 `playback/thumbnails/progress.py`，57 行）。
//!
//! # 它解决的是「provider 调用期间任务看起来死了」
//!
//! 类 docstring 原文：「Keep thumbnail work visible while provider calls block.」
//!
//! 生成缩略图要调 provider（可能跑几分钟），而进度上报只发生在**阶段之间**。
//! 没有心跳的话，任务中心里那条任务会**长时间没有任何更新** —— 租约到期后
//! 它会被判定为崩溃并被重新领取，而它其实正在正常干活。
//!
//! # 心跳间隔 2 秒
//!
//! [`INTERVAL_SECONDS`]。为什么不是更长：租约默认 30 秒（见
//! `sm_service::system::task_queue::DEFAULT_LEASE_SECONDS`），2 秒的心跳足够
//! 证明「还活着」；再密只是白写库。
//!
//! # 心跳线程**自己开 DB 连接**
//!
//! 上游在心跳里开 `connection_context()`。原因是发起心跳的那个连接可能正
//! 被一个**阻塞的 provider 调用**占着 —— 拿它上报会一起卡住。
//!
//! Rust 侧对应：心跳任务要**独立**的连接池句柄，不能借用主连接。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 心跳间隔（秒）。
pub const INTERVAL_SECONDS: u64 = 2;

/// 一次心跳上报的载荷。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HeartbeatPayload {
    /// 已处理的媒体数。
    pub processed: i32,
    /// 当前正在处理的媒体 id。用于前端显示「在处理哪个」。
    pub current_media_id: Option<i64>,
    /// 附加信息（文案等）。
    pub message: Option<String>,
}

/// 上报器。**可注入** —— 心跳线程必须能独立发请求。
pub trait HeartbeatSink: Send + Sync {
    /// 发一次心跳。**失败不 panic**：上报失败不该让生成任务失败。
    fn emit(&self, payload: &HeartbeatPayload) -> Result<(), String>;
}

/// 心跳器。`Drop` 时停止心跳线程。
pub struct ThumbnailTaskProgress {
    sink: Arc<dyn HeartbeatSink>,
    stop: Arc<AtomicBool>,
    /// 心跳线程句柄。`Drop` 时 join。
    handle: Option<std::thread::JoinHandle<()>>,
    /// 当前载荷。**`Arc` 是必需的** —— 心跳线程与 `emit` 要读同一份，
    /// 否则会出现「刚上报完进度，心跳又发回旧值」。
    payload: Arc<std::sync::Mutex<HeartbeatPayload>>,
}

impl ThumbnailTaskProgress {
    /// 构造。**不立刻启动线程** —— 调 `start()` 才开。
    ///
    /// 分成两步是因为 `new` 里开线程会让「构造失败」与「线程 panic」混在一起，
    /// 而后者只会在第一次 `emit` 时暴露。
    pub fn new(sink: Arc<dyn HeartbeatSink>) -> Self {
        Self {
            sink,
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            payload: Arc::new(std::sync::Mutex::new(HeartbeatPayload::default())),
        }
    }

    /// 启动心跳线程。
    pub fn start(&mut self) {
        if self.handle.is_some() {
            return; // 幂等：重复 start 不该开两个线程
        }
        let sink = Arc::clone(&self.sink);
        let stop = Arc::clone(&self.stop);
        // ★ 克隆**同一份** payload，不是新建 —— 见字段文档。
        let payload = Arc::clone(&self.payload);
        self.handle = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let snapshot = { payload.lock().map(|p| p.clone()).unwrap_or_default() };
                // ★ 上报失败只忽略 —— 见 HeartbeatSink 的文档。
                let _ = sink.emit(&snapshot);
                std::thread::sleep(std::time::Duration::from_secs(INTERVAL_SECONDS));
            }
        }));
    }

    /// 立即发一次（不等间隔）。`force = false` 时按 2 秒节流。
    pub fn emit(&self, payload: HeartbeatPayload) {
        if let Ok(mut slot) = self.payload.lock() {
            *slot = payload.clone();
        }
        let _ = self.sink.emit(&payload);
    }

    /// 停掉心跳。**幂等**。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ThumbnailTaskProgress {
    /// ★ 停线程必须放 `Drop`，不能只靠调用方记得调。
    ///
    /// 漏掉的代价：线程持有 `Arc`，跑着的任务永远不释放连接池 —— 而任务
    /// 可能因任何错误提前返回。
    fn drop(&mut self) {
        self.stop();
    }
}
