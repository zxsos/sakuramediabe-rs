//! HTTP `Range` 请求的解析与响应构造。
//!
//! # 为什么需要它
//!
//! 视频播放器**必须**能拖动进度条。拖动就是浏览器发一个
//! `Range: bytes=1048576-` 请求第 1MB 之后的内容；服务器不认 `Range`
//! 只有两个下场：要么每次都发全量（一个 900 秒的片段可能几十 MB，拖一次
//! 进度条就是一次全量下载），要么播放器直接不能用。
//!
//! 上游用 `fastapi-range-responses` 这个第三方库实现，本仓库手写 ——
//! `tokio-util` 与它带来的 `StreamReader` 都不在依赖里，而手写换来的是
//! 「不引新依赖 + 可单测」：这个模块的每条规则都能在 `cargo test --lib`
//! 里验掉，不需要起服务器也不需要真实文件。
//!
//! # 遵循 RFC 7233
//!
//! | 请求 | 响应 |
//! |---|---|
//! | 无 `Range` | 200 + 全量 + `Accept-Ranges: bytes` |
//! | `bytes=0-499` | 206 + `Content-Range: bytes 0-499/1000` |
//! | `bytes=500-` | 206 + `Content-Range: bytes 500-999/1000` |
//! | `bytes=-500` | 206 + 最后 500 字节 |
//! | 起点越界 / `bytes=-0` | 416 + `Content-Range: bytes */1000` |
//! | 语法无法解析 | **200 全量**（RFC 要求忽略） |
//! | 多区间 `bytes=0-1,5-6` | **200 全量**（见下） |
//!
//! # 两处刻意的简化
//!
//! **多区间请求返回 200 全量。** RFC 7233 允许服务端忽略多区间（回 200 或
//! 206 单区间）。多区间是 `curl` 之类的手工请求，播放器不会发，而支持它要
//! 做 `multipart/byteranges` 响应 —— 那是一整套 multipart 编码，客户端
//! 兼容性最难验证。为一个没人用而高风险的方向不值得。
//!
//! **无法解析的 `Range` 忽略而不是 416。** 这是 RFC 的明确要求（"An origin
//! server MUST ignore a Range header field that contains a range unit it does
//! not understand"）。把它当 416 会让某些代理发来的正常请求失败。

use std::ops::RangeInclusive;

/// 单个字节区间，**闭区间**（与 HTTP 的 `bytes=a-b` 语义一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    /// 闭区间上界。
    pub end: u64,
}

impl ByteRange {
    /// 区间字节数。`end < start`（不该发生）时算 0。
    #[must_use]
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// 恒为 `false` —— 零长度区间不是合法结果（`bytes=-0` 走
    /// [`RangeRequest::Unsatisfiable`]）。保留它是为了让调用点用
    /// `is_empty()` 表达意图，而不是让人以为空区间可能出现。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// 闭区间转标准库的 `RangeInclusive`。
    #[must_use]
    pub fn to_inclusive(&self) -> RangeInclusive<u64> {
        self.start..=self.end
    }

    /// `Content-Range` 的值，形如 `bytes 0-499/1000`。
    #[must_use]
    pub fn content_range_header(&self, total: u64) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, total)
    }
}

/// 解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// 不做范围请求：回 200 + 全量。
    ///
    /// 包含三种情况：没有 `Range` 头、`Range` 语法无法解析、`Range` 是多区间。
    /// 后两者按 RFC 忽略。
    None,
    /// 单区间，回 206。
    One(ByteRange),
    /// 不可满足，回 416 + `Content-Range: bytes */total`。
    Unsatisfiable,
}

impl RangeRequest {
    /// 解析一个 `Range` 头值。`total` 是资源的完整字节数。
    ///
    /// `total == 0`（空文件）时，任何区间都不可满足 —— 没有字节可以给。
    #[must_use]
    pub fn parse(header: Option<&str>, total: u64) -> Self {
        let Some(header) = header else {
            return Self::None;
        };
        let header = header.trim();

        // `Range = range-unit "=" range-set`。只认 `bytes`。
        // 等号两侧允许空白：严格按 RFC 7233 的 ABNF 这里没有 OWS，但宽松
        // 解析的代价是零 —— 最坏结果是从「200 全量」变成「206 区间」，
        // 两者都是合法响应，而某些代理确实会发 `bytes = 0-499`。
        let Some((unit, spec)) = header.split_once('=') else {
            return Self::None;
        };
        if unit.trim() != "bytes" {
            return Self::None;
        }
        let spec = spec.trim();

        // 多区间：忽略（见类型文档）。
        if spec.contains(',') {
            return Self::None;
        }

        // 区间里必然有一个 `-`。没有就是语法错误。
        let Some((first, last)) = spec.split_once('-') else {
            return Self::None;
        };
        let (first, last) = (first.trim(), last.trim());

        // `bytes=-N`：第一个字段为空 -> 后缀式。
        if first.is_empty() {
            return match last.parse::<u64>() {
                Ok(suffix) => Self::suffix(suffix, total),
                Err(_) => Self::None,
            };
        }
        let Ok(start) = first.parse::<u64>() else {
            return Self::None;
        };
        // `bytes=N-`：第二个字段为空 -> 开区间。
        if last.is_empty() {
            return Self::open(start, total);
        }
        match last.parse::<u64>() {
            Ok(end) => Self::closed(start, end, total),
            Err(_) => Self::None,
        }
    }

    /// `bytes=-N` —— 最后 N 字节。
    fn suffix(length: u64, total: u64) -> Self {
        if total == 0 {
            return Self::Unsatisfiable;
        }
        // 零字节后缀不可满足（RFC 7233 §2.1：「A suffix-length of zero」）。
        if length == 0 {
            return Self::Unsatisfiable;
        }
        let last_byte = total - 1;
        Self::One(ByteRange {
            // `length` 大于文件长度时 saturating_sub 会落到 0，即整个文件。
            start: last_byte.saturating_sub(length - 1),
            end: last_byte,
        })
    }

    /// `bytes=start-` —— 从 `start` 到文件末尾。
    fn open(start: u64, total: u64) -> Self {
        if total == 0 {
            return Self::Unsatisfiable;
        }
        let last_byte = total - 1;
        if start > last_byte {
            return Self::Unsatisfiable;
        }
        Self::One(ByteRange {
            start,
            end: last_byte,
        })
    }

    /// `bytes=start-end` —— 闭区间，终点夹到文件末尾。
    fn closed(start: u64, end: u64, total: u64) -> Self {
        if total == 0 {
            return Self::Unsatisfiable;
        }
        let last_byte = total - 1;
        // 起点越界：整个区间都在文件之外。
        if start > last_byte {
            return Self::Unsatisfiable;
        }
        // 终点夹到末尾 —— `bytes=0-99999` 对 1000 字节的文件是合法请求。
        let end = end.min(last_byte);
        // 终点在起点之前（`bytes=500-100`）：不可满足。
        if end < start {
            return Self::Unsatisfiable;
        }
        Self::One(ByteRange { start, end })
    }

    /// 416 响应的 `Content-Range` 值。
    #[must_use]
    pub fn unsatisfied_header(total: u64) -> String {
        format!("bytes */{total}")
    }
}

/// 单次读取的块大小。
///
/// 64 KiB 是权衡：太小则系统调用次数多，太大则并发请求时内存占用高。
/// 片段最长 900 秒（配置 `media.media_clip_max_duration_seconds`），产物可能
/// 几十 MB，播放器并发 6 条连接是常见的 —— 64 KiB x 6 约 384 KiB 每客户端。
pub const CHUNK_SIZE: usize = 64 * 1024;

/// 以正确的状态码与响应头返回 `path` 的内容，遵循请求的 `Range`。
///
/// 状态码由 [`RangeRequest`] 决定：206（区间）、200（全量）、416（不可满足）。
///
/// # `Accept-Ranges` 三种情况都要带
///
/// 少了它播放器**不会**尝试拖动进度条，这个端点等于不支持 seek；416 少了它，
/// 客户端就不知道这个资源本来支持区间。
///
/// # 为什么先算出三元组再一次性建 Response
///
/// `Builder::body()` 会**吃掉** builder 并返回 `Response`，所以不能
/// `builder.status(..).body(..)` 之后再拿同一个 builder 继续加头。
pub fn serve_file(
    path: &std::path::Path,
    range_header: Option<&str>,
) -> Result<axum::response::Response, std::io::Error> {
    use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE};
    use axum::http::{HeaderName, StatusCode};

    let total = std::fs::metadata(path)?.len();

    // (状态码, 额外响应头, 响应体)
    let (status, extra, body): (StatusCode, Vec<(HeaderName, String)>, _) =
        match RangeRequest::parse(range_header, total) {
            RangeRequest::Unsatisfiable => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                vec![(CONTENT_RANGE, RangeRequest::unsatisfied_header(total))],
                axum::body::Body::empty(),
            ),
            RangeRequest::None => (
                StatusCode::OK,
                vec![(CONTENT_LENGTH, total.to_string())],
                body_for(path, whole_file(total))?,
            ),
            RangeRequest::One(range) => (
                StatusCode::PARTIAL_CONTENT,
                vec![
                    (CONTENT_RANGE, range.content_range_header(total)),
                    // **区间**长度，不是文件长度 —— 写错会让播放器进度条错乱。
                    (CONTENT_LENGTH, range.len().to_string()),
                ],
                body_for(path, range)?,
            ),
        };

    let mut response = axum::response::Response::builder()
        .status(status)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_TYPE, "video/mp4");
    for (name, value) in extra {
        response = response.header(name, value);
    }
    Ok(response
        .body(body)
        .expect("头名称来自固定字面量，值已转成字符串"))
}

/// 空文件（`total == 0`）的「整个文件」区间。
fn whole_file(total: u64) -> ByteRange {
    ByteRange {
        start: 0,
        // 0 字节文件给一个空区间，`len()` 为 0，流立刻结束。
        end: total.saturating_sub(1),
    }
}

/// 流式读取的状态：文件路径、当前位置、剩余字节数。
///
/// **刻意不持有 `File` 句柄。** 早先的写法是把句柄在 `spawn_blocking` 前后
/// 换来换去，那需要一个占位句柄来应付所有权转移，而占位句柄本身就是一处
/// 会出错的地方（打开失败、平台差异）。
///
/// 改成每块按路径重新打开：`open` 是微秒级操作，而一次 64 KiB 的读是它的
/// 数量级以上，所以代价可以忽略，换来的是状态里只有一个 `PathBuf`。
type Cursor = (std::path::PathBuf, u64, u64);

/// 打开文件并把 `range` 变成一个按块产出的流。
fn body_for(path: &std::path::Path, range: ByteRange) -> Result<axum::body::Body, std::io::Error> {
    let initial: Cursor = (path.to_path_buf(), range.start, range.len());
    // `try_unfold` 的 future 输出是 `Option<(Item, State)>` —— **item 在前，
    // state 在后**。写反了的话 Item 会被推成 `Cursor`，而流产出的是
    // `Result<Item, _>`，于是类型不匹配。
    let stream = futures::stream::try_unfold(initial, move |cursor: Cursor| async move {
        let next: Option<(axum::body::Bytes, Cursor)> = read_chunk(cursor).await?;
        Ok::<_, std::io::Error>(next)
    });
    Ok(axum::body::Body::from_stream(stream))
}

/// 读一块。`None` 表示流结束。
///
/// 返回 `(字节块, 下一个状态)` —— 顺序与 `try_unfold` 一致，见上面的调用点。
async fn read_chunk(cursor: Cursor) -> Result<Option<(axum::body::Bytes, Cursor)>, std::io::Error> {
    use axum::body::Bytes;
    use std::io::{Read, Seek, SeekFrom};

    let (path, position, remaining) = cursor;
    if remaining == 0 {
        return Ok(None);
    }
    let want = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);

    // 路径要留给下一个状态，所以给阻塞任务一份克隆。`PathBuf` 的克隆是
    // 一次小分配，相对一次 64 KiB 的读可以忽略。
    let for_task = path.clone();

    // 标准库的 `File` 是同步的：在 async 上下文里直接 read 会卡住整个 worker
    // 线程，所以整块读取放进阻塞池。没有用 `tokio-util` 的 `ReaderStream`
    // 是因为那个 crate 不在依赖里，而这是唯一需要它的地方。
    let (mut buffer, filled) = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&for_task)?;
        let mut buffer = vec![0u8; want];
        file.seek(SeekFrom::Start(position))?;
        let mut filled = 0usize;
        // `read` 允许短读，所以循环填满；返回 0 表示 EOF，此时保留已读到的部分。
        while filled < want {
            match file.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        Ok::<_, std::io::Error>((buffer, filled))
    })
    .await
    .map_err(std::io::Error::other)??;

    if filled == 0 {
        // 还没读完请求的区间就 EOF：文件比库里记的 `file_size_bytes` 短。
        // 提前结束而不是报错 —— 客户端拿到短一个 body，比 500 好排查。
        return Ok(None);
    }
    let filled = u64::try_from(filled).unwrap_or(u64::MAX);
    buffer.truncate(usize::try_from(filled).unwrap_or(usize::MAX));
    Ok(Some((
        Bytes::from(buffer),
        (path, position + filled, remaining - filled),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL: u64 = 1000;

    fn one(range: &str) -> ByteRange {
        match RangeRequest::parse(Some(range), TOTAL) {
            RangeRequest::One(r) => r,
            other => panic!("{range} 应当是可满足的单区间，实际 {other:?}"),
        }
    }

    fn unsat(range: &str) {
        assert_eq!(
            RangeRequest::parse(Some(range), TOTAL),
            RangeRequest::Unsatisfiable,
            "{range} 应当不可满足"
        );
    }

    fn ignored(range: &str) {
        assert_eq!(
            RangeRequest::parse(Some(range), TOTAL),
            RangeRequest::None,
            "{range} 应当被忽略（回 200 全量）"
        );
    }

    #[test]
    fn no_header_means_no_range() {
        assert_eq!(RangeRequest::parse(None, TOTAL), RangeRequest::None);
        // 空白不是合法 Range
        assert_eq!(RangeRequest::parse(Some("   "), TOTAL), RangeRequest::None);
    }

    #[test]
    fn a_closed_range_is_exact() {
        let r = one("bytes=0-499");
        assert_eq!((r.start, r.end, r.len()), (0, 499, 500));
        assert_eq!(r.content_range_header(TOTAL), "bytes 0-499/1000");
    }

    #[test]
    fn an_open_ended_range_runs_to_the_last_byte() {
        let r = one("bytes=500-");
        assert_eq!((r.start, r.end, r.len()), (500, 999, 500));
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        let r = one("bytes=-500");
        assert_eq!((r.start, r.end), (500, 999));
        // 后缀比文件还长 -> 取整个文件
        let whole = one("bytes=-5000");
        assert_eq!((whole.start, whole.end), (0, 999));
    }

    /// 终点越界要**夹**到文件末尾，而不是不可满足 ——
    /// `bytes=0-99999` 对 1000 字节的文件是完全正常的请求。
    #[test]
    fn an_end_past_the_file_is_clamped_not_rejected() {
        let r = one("bytes=0-99999");
        assert_eq!((r.start, r.end), (0, 999));
    }

    /// 单字节区间。闭区间的边界，容易写成 `end - start` 而少 1。
    #[test]
    fn a_single_byte_range_has_length_one() {
        let r = one("bytes=0-0");
        assert_eq!(r.len(), 1, "bytes=0-0 是一个字节，不是零个");
        let last = one("bytes=999-999");
        assert_eq!((last.start, last.end, last.len()), (999, 999, 1));
    }

    #[test]
    fn a_start_at_the_last_byte_is_satisfiable() {
        let r = one("bytes=999-");
        assert_eq!((r.start, r.end), (999, 999));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn unsatisfiable_ranges() {
        unsat("bytes=1000-"); // 起点正好在末尾之后
        unsat("bytes=1000-1005");
        unsat("bytes=5000-6000");
        unsat("bytes=-0"); // 零字节后缀
        unsat("bytes=500-100"); // 终点在起点之前
    }

    /// 空文件对任何区间都不可满足 —— 没有字节可以给。
    #[test]
    fn an_empty_file_satisfies_no_range() {
        for header in ["bytes=0-", "bytes=0-0", "bytes=-1", "bytes=0-999"] {
            assert_eq!(
                RangeRequest::parse(Some(header), 0),
                RangeRequest::Unsatisfiable,
                "{header} 对空文件应当不可满足"
            );
        }
        // 但「无 Range」在空文件上仍然是 200 + 空体
        assert_eq!(RangeRequest::parse(None, 0), RangeRequest::None);
    }

    /// 语法错误与多区间按 RFC 忽略，不是 416。
    ///
    /// 这条容易被写成 416 —— 那样会让某些代理发来的正常请求失败。
    #[test]
    fn unparsable_and_multi_ranges_are_ignored() {
        ignored("items=0-10"); // 未知 range-unit
        ignored("bytes=abc-def"); // 非数字
        ignored("bytes="); // 空
        ignored("bytes=-"); // 两端都空
        ignored("bytes=0-1,5-6"); // 多区间
        ignored("0-499"); // 缺 range-unit
    }

    /// `bytes=-N` 与 `bytes=N-` 长得像但语义完全不同 ——
    /// 一个从末尾数，一个从起点数。混淆它们会让后缀请求返回错误的一段。
    #[test]
    fn suffix_and_open_ranges_are_not_confused() {
        let suffix = one("bytes=-100");
        let open = one("bytes=900-");
        assert_eq!((suffix.start, suffix.end), (900, 999));
        assert_eq!((open.start, open.end), (900, 999));
        // 但换个数字就完全不同
        assert_eq!((one("bytes=-900").start, one("bytes=-900").end), (100, 999));
        assert_eq!((one("bytes=100-").start, one("bytes=100-").end), (100, 999));
    }

    /// 前后空白是合法的（HTTP 头值常被 trim）。
    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let r = one("  bytes=0-499  ");
        assert_eq!((r.start, r.end), (0, 499));
        assert_eq!(one("bytes = 0-499").end, 499, "等号两侧也可以有空白");
    }

    /// 覆盖整个文件的区间与「无 Range」等价，但走 206。
    #[test]
    fn a_range_covering_the_whole_file_is_still_partial_content() {
        let r = one("bytes=0-999");
        assert_eq!((r.start, r.end, r.len()), (0, 999, TOTAL));
    }

    /// `to_inclusive` 交给 `Read::take` 用，所以上界必须是闭区间。
    #[test]
    fn to_inclusive_is_a_closed_range() {
        let r = one("bytes=100-199");
        let inclusive = r.to_inclusive();
        assert_eq!(*inclusive.start(), 100);
        assert_eq!(*inclusive.end(), 199);
        assert_eq!(inclusive.count(), 100, "100..=199 是 100 个");
    }
}
