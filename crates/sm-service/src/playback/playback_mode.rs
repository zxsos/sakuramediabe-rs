//! 「这次播放**实际**用了哪种投递方式」的登记表。
//!
//! 复刻上游 `_PlaybackModeResults`（`api/routers/playback/media.py:58-109`）。
//!
//! # 上游就是**进程内、有界、短命**的字典，不是数据库表
//!
//! 第一反应通常是「建一张表」。上游是模块级单例 + 120 秒 TTL + 1024 条 LRU 上限 +
//! 惰性清理，查询端点只做一次 `get`。
//!
//! 代价是**多实例部署下会读不到**（登记落在 A 实例、查询打到 B 实例）。这里照抄
//! 同样的取舍 —— 换成跨实例存储是另一种设计，不该顺手改掉上游语义。
//!
//! # 为什么这么短命、这么小
//!
//! 它只服务「刚起播那几秒，前端来问一次」。前端为此重试 3 次、每次隔 1 秒
//! （`sakuramedia/lib/widgets/domain/media/media_playback_info_button.dart:84-86`，
//! 注释：playlist 通知可能先于网关响应）。所以两个常量都不是可有可无的：
//!
//! - **TTL 120 秒**。写长（比如按签名窗口取 6 小时）只会让一个**过时的答案**还能
//!   被问到 —— 而客户端拿它当「现在这条流的实际模式」。
//! - **上限 1024 条**。attempt id 由**客户端生成**（16 字节 base64url），不设上限
//!   就是一条内存增长路径。
//!
//! # 记录的条件很窄
//!
//! 上游只在 `playback_attempt_id is not None and not normalized_path` 时记录
//! （`media.py:309-313`），即**只记主文件**：字幕、封面这类子资源路径非空，
//! 不进这张表。这不是遗漏 —— 播放器上报的模式是针对整条流的。
//!
//! # ★ 存的是 `mode`，不是 `delivery`
//!
//! `direct` / `proxy` 与 `redirect` / `proxy` **不是同一套字面量**。上游在**写入
//! 时**就折了（`media.py:83`）：
//!
//! ```python
//! mode="direct" if delivery == "redirect" else "proxy"
//! ```
//!
//! 所以 [`PlaybackModeResults::record`] 收 delivery、[`PlaybackModeResults::get`]
//! 给 mode。直接把 `redirect` 透出去，前端会落到 `_ => '未确认'`
//! —— **静默显示「未确认」，不报错**。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 登记存活时长。上游 `_PLAYBACK_MODE_RESULT_TTL_SECONDS = 120.0`
/// （`media.py:54`）。
const TTL: Duration = Duration::from_secs(120);

/// 最多记多少条。上游 `_PLAYBACK_MODE_RESULT_MAX_ENTRIES = 1024`
/// （`media.py:55`）。
const MAX_ENTRIES: usize = 1024;

/// delivery → mode。上游 `media.py:83`。
///
/// 注意这里是 `if/else` 而**不是白名单**：`redirect` 之外的任何值都算 `proxy`。
/// 照抄这个形状 —— 改成白名单（未知值报错）会让插件将来多一种投递方式时，
/// 宿主先一步把合法的登记丢掉。
fn mode_of(delivery: &str) -> &'static str {
    if delivery == "redirect" {
        "direct"
    } else {
        "proxy"
    }
}

/// 进程内的播放模式登记表。
pub struct PlaybackModeResults {
    ttl: Duration,
    max_entries: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    entries: HashMap<String, Entry>,
    /// 单调递增的「最近使用」序号。
    ///
    /// 用它而不是 `Instant` 排 LRU：同一毫秒内的多次操作靠时间排不出先后，
    /// 淘汰顺序就成了随机的。
    tick: u64,
}

struct Entry {
    mode: &'static str,
    recorded_at: Instant,
    used_at: u64,
}

impl PlaybackModeResults {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                tick: 0,
            }),
        }
    }

    /// 记下「这个 attempt 实际用了哪种投递方式」。
    ///
    /// 入参是 **delivery**（`redirect` / `proxy`），存下来的是 **mode**
    /// （`direct` / `proxy`）—— 与上游同构，映射只在 `mode_of` 一处。
    pub fn record(&self, attempt_id: &str, delivery: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner.prune(now, self.ttl);
        let used_at = inner.next_tick();
        inner.entries.insert(
            attempt_id.to_owned(),
            Entry {
                mode: mode_of(delivery),
                recorded_at: now,
                used_at,
            },
        );
        inner.evict_down_to(self.max_entries);
    }

    /// 查「这个 attempt 用了哪种模式」。`direct` / `proxy`。
    ///
    /// `None` = 没登记或已过期。**调用方该回 200 + `null`**，不是 404：
    /// 「还没登记」是正常中间态（前端会重试 3 次）。
    ///
    /// 读也 touch LRU（上游 `media.py:96` 的 `move_to_end`）：这个表的目的就是
    /// 「最近问过的留着」，只按写入时间淘汰会把正在被问的那条先踢掉。
    pub fn get(&self, attempt_id: &str) -> Option<&'static str> {
        let mut inner = self.lock();
        let now = Instant::now();
        // 与上游一致：读之前也清一遍过期项（`get` 里第一件事就是
        // `_discard_expired`）。只在写入时清会让「只读不写」的场景一直留着过期项。
        let used_at = inner.next_tick();
        inner.prune(now, self.ttl);
        inner.entries.get_mut(attempt_id).map(|entry| {
            entry.used_at = used_at;
            entry.mode
        })
    }

    /// 锁中毒时**不复用**：这里的值只是「一个字符串」，没有不变式要保护，
    /// 让整个服务因为一次无关的 panic 永久 500 是不成比例的。
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn prune(&mut self, now: Instant, ttl: Duration) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.recorded_at) < ttl);
    }

    /// 超出上限就淘汰**最久没用过**的那条（上游 `popitem(last=False)`）。
    fn evict_down_to(&mut self, max_entries: usize) {
        while self.entries.len() > max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

/// 进程级共享实例。
///
/// 与上游的模块级 `_PLAYBACK_MODE_RESULTS` 同构：写入端（`play_media`）与读取端
/// （`get_playback_attempt_mode`）必须看到**同一份**登记表，所以它不能是每请求新建
/// 的值，也没有理由挂在 `AppState` 上走一遍注入。
pub fn shared() -> &'static PlaybackModeResults {
    static SHARED: std::sync::OnceLock<PlaybackModeResults> = std::sync::OnceLock::new();
    SHARED.get_or_init(|| PlaybackModeResults::new(TTL, MAX_ENTRIES))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_ttl() -> Duration {
        Duration::from_secs(60)
    }

    /// 记过就能查到，且取回的是原来那个值（**proxy 那半边**）。
    #[test]
    fn a_recorded_proxy_delivery_is_visible_as_proxy() {
        let results = PlaybackModeResults::new(short_ttl(), MAX_ENTRIES);
        assert_eq!(results.get("a"), None, "没记过就是 None");
        results.record("a", "proxy");
        assert_eq!(results.get("a"), Some("proxy"));
    }

    /// ★ `record("redirect")` 取回的是 **`direct`**，不是 `redirect`。
    ///
    /// 这套字面量是前端在匹配的（`media_playback_info_button.dart:94`：
    /// `switch (response['mode']) { 'direct' => '直连', ... }`）。透传 `redirect`
    /// 不会报错 —— 前端落到 `_ => '未确认'`，静默显示「未确认」。
    #[test]
    fn a_redirect_delivery_is_stored_as_direct_not_redirect() {
        let results = PlaybackModeResults::new(short_ttl(), MAX_ENTRIES);
        results.record("a", "redirect");
        assert_eq!(results.get("a"), Some("direct"));
        assert_ne!(
            results.get("a"),
            Some("redirect"),
            "透传会让前端静默显示未确认"
        );
    }

    /// `redirect` 之外的任何值都算 `proxy`（上游是 `if/else`，不是白名单）。
    #[test]
    fn anything_but_redirect_is_proxy() {
        let results = PlaybackModeResults::new(short_ttl(), MAX_ENTRIES);
        for delivery in ["proxy", "", "weird"] {
            results.record(delivery, delivery);
            assert_eq!(
                results.get(delivery),
                Some("proxy"),
                "delivery={delivery:?}"
            );
        }
    }

    /// TTL 过后查不到 —— **不是**返回一个过期的旧值。
    ///
    /// 客户端拿这个值当「现在这条流的实际模式」，返回过时答案比返回「不知道」更糟
    /// （它会显示一个错的模式，而且看起来是确定的）。
    #[test]
    fn an_expired_record_is_not_returned() {
        let results = PlaybackModeResults::new(Duration::ZERO, MAX_ENTRIES);
        results.record("a", "proxy");
        assert_eq!(results.get("a"), None, "零 TTL 下立刻过期");
    }

    /// 超出上限淘汰**最久没用过**的（LRU），不是最早写入的。
    ///
    /// 这两者在「只写不读」时重合，在「读了旧条目」时不同 —— 下面这条测的就是后者：
    /// `a` 写得最早，但在 `b` 之前被读过，所以该淘汰的是 `b`。
    #[test]
    fn eviction_drops_the_least_recently_used_not_the_oldest() {
        let results = PlaybackModeResults::new(short_ttl(), 2);
        results.record("a", "proxy");
        results.record("b", "proxy");
        // 读一下 `a`：此后它的「最近使用」比 `b` 新。
        assert_eq!(results.get("a"), Some("proxy"));
        // 插第三条，触发淘汰。
        results.record("c", "proxy");

        assert_eq!(results.get("a"), Some("proxy"), "刚被读过的不该淘汰");
        assert_eq!(results.get("b"), None, "最久没用过的是 b");
        assert_eq!(results.get("c"), Some("proxy"));
    }

    /// 重复记录同一个 id 取**最后一次**。
    #[test]
    fn the_last_record_wins() {
        let results = PlaybackModeResults::new(short_ttl(), MAX_ENTRIES);
        results.record("a", "proxy");
        results.record("a", "redirect");
        assert_eq!(results.get("a"), Some("direct"));
    }
}
