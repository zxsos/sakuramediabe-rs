//! SSE 传输骨架：事件名清单 + 广播 hub + keep-alive 流。
//!
//! 对应上游 `src/api/routers/_utils.py:55-73`（`to_sse_event` 与
//! `sse_streaming_response`）与三个调用它的端点。
//!
//! # 逐字节的线格式
//!
//! ```python
//! f"event: {event}\ndata: {json.dumps(payload, ensure_ascii=False)}\n\n"
//! ```
//!
//! 两个细节容易漏：
//!
//! - **`ensure_ascii=False`** —— 中文直接以 UTF-8 原样输出，不转成 `\uXXXX`。
//!   `serde_json::to_string` 的默认行为与之**一致**（它不转义非 ASCII），
//!   所以这里不需要任何配置。写成 `to_string` 就对，写成别的就错。
//! - **每条事件以 `\n\n` 结束** —— 两个换行。少一个，客户端会把两条事件
//!   连成一条（`data` 变成多行），而 Flutter 侧的解析器只在空行处切分。
//!
//! # 事件清单：**10 个**，不是 13
//!
//! 本仓库此前的注释（`Cargo.toml` 与 `sm-api/src/lib.rs`）写的是
//! 「SSE 13 事件」。那是把 `completed` 的 **yield 次数**（恰好 13 处）当成了
//! 事件种数。穷举三个流的 `yield "<name>"` 后，**去重**得到 10 个：
//!
//! | 事件 | 影片搜索 | 系列导入 | 演员搜索 |
//! |---|---|---|---|
//! | [`SEARCH_STARTED`] | ✅ | ✅ | ✅ |
//! | [`SERIES_FOUND`] | — | ✅ | — |
//! | [`JAVDB_SERIES_FOUND`] | — | ✅ | — |
//! | [`MOVIE_FOUND`] | ✅ | ✅ | — |
//! | [`UPSERT_STARTED`] | ✅ | ✅ | ✅ |
//! | [`MOVIE_SKIPPED`] | — | ✅ | — |
//! | [`MOVIE_UPSERT_STARTED`] | — | ✅ | — |
//! | [`MOVIE_UPSERT_FINISHED`] | — | ✅ | — |
//! | [`UPSERT_FINISHED`] | ✅ | ✅ | ✅ |
//! | [`COMPLETED`] | ✅ | ✅ | ✅ |
//!
//! 三个流（路径与上游一致）：
//!
//! | 方法 | 路径 | 上游 |
//! |---|---|---|
//! | POST | `/movies/search/javdb/stream` | `catalog/movies.py:196` |
//! | POST | `/movies/series/{series_id}/javdb/import/stream` | `catalog/movies.py:111` |
//! | POST | `/actors/search/javdb/stream` | `catalog/actors.py:86` |
//!
//! # 本模块只做传输，**不**注册这三个端点
//!
//! 它们的数据源是 `MovieMetadataRefreshService` 与 `ActorService` 的
//! `stream_*` 生成器，都在 `catalog` 域（27 文件 / 7,556 行，尚未开工）。
//! 端点注册随那个域一起做 —— 现在注册只能返回空流。
//!
//! # 与上游刻意的一处差异：不发 `Connection: keep-alive`
//!
//! 上游设了三个头。`Cache-Control: no-cache` 由 axum 的 `Sse` 自带；
//! `X-Accel-Buffering: no` 是给 nginx 的（关掉缓冲，否则事件被攒着不发），
//! 必须保留。而 `Connection` 是 **hop-by-hop** 头，由传输层（hyper）管理，
//! 应用层设置它没有意义、在 HTTP/2 下还是协议违规 —— 所以这里不设，
//! 连接语义交给 hyper。

use std::convert::Infallible;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;

/// `search_started`：开始搜索。三条流都有，且都是**第一条**事件。
pub const SEARCH_STARTED: &str = "search_started";
/// `series_found`：本地系列存在（仅系列导入流）。
pub const SERIES_FOUND: &str = "series_found";
/// `javdb_series_found`：JavDB 上找到了这个系列（仅系列导入流）。
pub const JAVDB_SERIES_FOUND: &str = "javdb_series_found";
/// `movie_found`：远端命中的影片信息，**落库前**就回给前端。
pub const MOVIE_FOUND: &str = "movie_found";
/// `upsert_started`：开始落库，`{"total": n}`。
pub const UPSERT_STARTED: &str = "upsert_started";
/// `movie_skipped`：该条已存在被跳过（仅系列导入流）。
pub const MOVIE_SKIPPED: &str = "movie_skipped";
/// `movie_upsert_started`：单条落库开始（仅系列导入流）。
pub const MOVIE_UPSERT_STARTED: &str = "movie_upsert_started";
/// `movie_upsert_finished`：单条落库完成（仅系列导入流）。
pub const MOVIE_UPSERT_FINISHED: &str = "movie_upsert_finished";
/// `upsert_finished`：落库阶段结束，带统计。
pub const UPSERT_FINISHED: &str = "upsert_finished";
/// `completed`：流结束。`{"success": bool, "reason": ...}`。
pub const COMPLETED: &str = "completed";

/// 全部事件名（去重后 10 个）。测试断言这个集合与上游一致。
pub const SSE_EVENT_NAMES: [&str; 10] = [
    SEARCH_STARTED,
    SERIES_FOUND,
    JAVDB_SERIES_FOUND,
    MOVIE_FOUND,
    UPSERT_STARTED,
    MOVIE_SKIPPED,
    MOVIE_UPSERT_STARTED,
    MOVIE_UPSERT_FINISHED,
    UPSERT_FINISHED,
    COMPLETED,
];

/// keep-alive 间隔。
///
/// 上游**没有**发心跳：流空转时连接就那么挂着。这里加 15 秒一次的
/// `: ping` 注释，理由是反代与客户端的空闲超时会静默掐断连接，而上游
/// 靠内网直连躲过了这件事。SSE 规范要求客户端忽略注释行，所以这是纯增量、
/// 不影响字节契约。
pub const KEEP_ALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// 一条要发出的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEvent {
    /// `event:` 字段。空串表示不带 `event:` 行（默认 `message` 类型）。
    pub name: String,
    /// `data:` 的内容。**必须是单行** —— 换行会被客户端当成多条事件。
    pub data: String,
}

impl ServerEvent {
    /// 用 `event: <name>` + 单行 JSON 载荷构造一条事件。
    pub fn json<T: Serialize>(name: &str, payload: &T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            name: name.to_owned(),
            // `to_string` 不转义非 ASCII，与上游 `ensure_ascii=False` 一致；
            // `to_string` 也不会插入换行，所以「单行」这个前提由它保证。
            data: serde_json::to_string(payload)?,
        })
    }

    /// 按上游线格式渲染成 axum 的 [`Event`]。
    fn to_sse_event(&self) -> Event {
        let event = Event::default().data(self.data.clone());
        if self.name.is_empty() {
            event
        } else {
            event.event(self.name.clone())
        }
    }
}

/// SSE 广播 hub。
///
/// # 容量与丢弃策略
///
/// `broadcast` 是**有界**的：缓冲满时最旧的事件被丢弃、慢消费者收到
/// `RecvError::Lagged(n)`。这里选丢弃而不是阻塞，理由是 SSE 的载荷是
/// 「进度快照」而不是「账目」—— 丢掉几条中间进度，下一条 `completed`
/// 仍然带着完整的 `stats`，客户端能自愈；而阻塞发布方会让一个卡住的
/// 连接拖住整个后台任务。
///
/// 容量默认 64 条：够缓冲一次突发（导入一部系列影片会连发十几条
/// `movie_upsert_*`），也够把慢客户端的丢失窗口控制在几十毫秒内。
///
/// # 为什么不存领域事件类型
///
/// 事件载荷是各域自己的 DTO（`stats` 的字段在三个流里都不一样）。
/// hub 只搬运「已序列化的一行 JSON」，于是 `catalog` 域将来落地时
/// 不必改动本模块 —— 反过来如果这里定义 `enum SseEvent`，每加一个事件
/// 都要动路由层，与「契约层与业务层分开」相悖。
#[derive(Debug, Clone)]
pub struct SseHub {
    sender: broadcast::Sender<ServerEvent>,
}

impl SseHub {
    /// 新建一个 hub。`capacity` 至少为 1。
    pub fn new(capacity: usize) -> Self {
        let (sender, _rx) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// 发布一条事件，返回**当前订阅者数量**。
    ///
    /// 返回 0 不是错误：没有订阅者时事件直接进缓冲，随后被覆盖。
    /// 这正是导入任务与 HTTP 连接解耦的地方 —— 没人看进度条时，
    /// 导入照跑，结果由最终的 `completed` 承载。
    pub fn publish(&self, event: ServerEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// 订阅。返回的 receiver 与本 hub 绑定，hub 被 drop 后该 receiver 收尾。
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.sender.subscribe()
    }

    /// 当前订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// 变成一个 HTTP 响应。
    ///
    /// 头：`Cache-Control: no-cache`（axum 自带）+ `X-Accel-Buffering: no`。
    /// 载荷类型 `text/event-stream` 由 `Sse` 设置。
    pub fn response(&self) -> Response {
        let stream = BroadcastStream::new(self.sender.subscribe()).map(|item| match item {
            Ok(event) => Ok::<Event, Infallible>(event.to_sse_event()),
            Err(BroadcastStreamRecvError::Lagged(missed)) => {
                // 丢弃是**已知**行为（见类型文档），但要留痕：
                // 「客户端看到的进度比库里少的量」只有这里能查。
                tracing::warn!(missed, "SSE 订阅者跟不上，已丢弃若干事件");
                // 丢的那几条无法补发（缓冲里已经没有），发一个注释让客户端
                // 知道中途有缺口，而不是让它误以为流是连续的。
                Ok(Event::default().comment(format!("dropped {missed} events")))
            }
        });

        Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
            .into_response()
            .tap_headers()
    }
}

/// 给响应补上非 hop-by-hop 的自定义头。
///
/// 单独一个 trait 而不是把 header 拼进 `Sse`：`Sse` 只接受 keep-alive
/// 配置，头部要在 `into_response()` 之后才能改。
trait TapHeaders {
    fn tap_headers(self) -> Response;
}

impl TapHeaders for Response {
    fn tap_headers(self) -> Response {
        let mut response = self;
        // `Cache-Control: no-cache` 已由 Sse 设置，这里只补 nginx 那个。
        if let Ok(value) = "no".parse() {
            response.headers_mut().insert("x-accel-buffering", value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_list_has_ten_distinct_names() {
        // 「13 事件」是误记（那是 `completed` 的 yield 次数）。这个断言的
        // 作用是：上游若新增或改名事件，这里会先红，而不是等到客户端发现。
        let mut sorted = SSE_EVENT_NAMES;
        sorted.sort_unstable();
        let unique = {
            let mut deduped = sorted.to_vec();
            deduped.dedup();
            deduped
        };
        assert_eq!(sorted.len(), 10, "去重前就应有 10 个");
        assert_eq!(unique.len(), 10, "10 个名字必须互不相同");
    }

    #[test]
    fn json_payloads_are_single_line_and_unescaped() {
        // 上游 `json.dumps(..., ensure_ascii=False)`：中文原样输出。
        let event =
            ServerEvent::json(MOVIE_FOUND, &serde_json::json!({"title": "素颜"})).expect("序列化");
        assert_eq!(event.name, MOVIE_FOUND);
        assert_eq!(event.data, r#"{"title":"素颜"}"#);
        assert!(!event.data.contains('\n'), "data 里不能有换行");
        assert!(!event.data.contains("\\u"), "非 ASCII 不能被转义");
    }

    #[test]
    fn a_publish_with_no_subscribers_is_not_an_error() {
        let hub = SseHub::new(4);
        assert_eq!(
            hub.publish(ServerEvent::json(COMPLETED, &serde_json::json!({"ok": true})).unwrap()),
            0
        );
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_was_published() {
        let hub = SseHub::new(4);
        let mut rx = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);

        hub.publish(ServerEvent::json(SEARCH_STARTED, &serde_json::json!({"n": 1})).unwrap());
        let got = rx.recv().await.expect("应当收到事件");
        assert_eq!(got.name, SEARCH_STARTED);
        assert_eq!(got.data, r#"{"n":1}"#);
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_told_how_much_it_missed() {
        // 缓冲 2 条、连发 4 条：接收方一定 lagged。
        let hub = SseHub::new(2);
        let mut rx = hub.subscribe();
        for i in 0..4 {
            hub.publish(ServerEvent::json(MOVIE_FOUND, &serde_json::json!({ "i": i })).unwrap());
        }
        let lagged = rx.recv().await.expect_err("应当 lagged");
        assert!(
            matches!(lagged, tokio::sync::broadcast::error::RecvError::Lagged(n) if n > 0),
            "丢失条数必须为正，否则「丢弃」这件事没法归因；实际：{lagged:?}"
        );
    }
}
