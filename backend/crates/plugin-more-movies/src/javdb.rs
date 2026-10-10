//! JavDB 最新影片列表抓取与校验。
//!
//! # 上游对应：`sakuramedia_more_movies/javdb.py`
//!
//! 列表接口 `/api/v1/movies/tags`：
//! - `filter_by={type}:t` 只按影片类型过滤，不传月份即返回按发行日期倒序的最新列表；
//! - 每页固定 50 条，接口硬上限 99 页（第 100 页起返回空列表）；
//! - 上游走宿主 `JavdbProvider`（复用其签名头、超时与重试）。Rust 契约
//!   （v0.2.0）没有「任意 JavDB 请求」的宿主 RPC，只有榜单查询；签名算法
//!   （`jdsignature`）是公开的（上游 `javdb.py:_get_sign`），插件自己实现。
//!
//! # 签名
//!
//! 上游 `_get_sign`：
//! ```text
//! secret = f"{timestamp}71cf27bb3c0bcdf207b64abecddc970098c7421ee7203b9cdae54478478a199e7d5a6e1a57691123c1a931c057842fb73ba3b3c83bcd69c17ccf174081e3d8aa"
//! sign = md5(secret)
//! header = f"{timestamp}.lpw6vgqzsp.{sign}"
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::heat::HeatInputs;

/// 每页固定条数（上游 `PAGE_SIZE`）。
pub const PAGE_SIZE: usize = 50;

/// 列表接口硬上限 99 页（上游注释）；`SAFETY_MAX_PAGES` 是异常保护。
pub const MAX_PAGES: u32 = 99;
pub const SAFETY_MAX_PAGES: u32 = 200;

/// （JavDB filter type，展示名）—— 0 有码 / 1 无码 / 3 FC2（上游 `MOVIE_TYPES`）。
pub const MOVIE_TYPES: [(u8, &str); 3] = [(0, "有码"), (1, "无码"), (3, "FC2")];

/// 上游 `javdb.py` 的 API host（`provider.host`）。
pub const DEFAULT_API_HOST: &str = "api.javdb.com";

/// 上游 `_get_sign` 里的固定 secret 后缀。
const SIGN_SECRET_SUFFIX: &str = "71cf27bb3c0bcdf207b64abecddc970098c7421ee7203b9cdae54478478a199e7d5a6e1a57691123c1a931c057842fb73ba3b3c83bcd69c17ccf174081e3d8aa";

/// 列表项（上游 `LatestItem`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestItem {
    pub javdb_id: String,
    pub number: String,
    pub release_date: Option<String>,
}

/// 列表请求失败或响应结构不合法（上游 `LatestPageError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestPageError {
    Request {
        movie_type: u8,
        page: u32,
        detail: String,
    },
    BadResponse {
        movie_type: u8,
        page: u32,
        detail: String,
    },
}

impl std::fmt::Display for LatestPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request {
                movie_type,
                page,
                detail,
            } => write!(f, "列表请求失败 type={movie_type} page={page}: {detail}"),
            Self::BadResponse {
                movie_type,
                page,
                detail,
            } => write!(
                f,
                "列表响应结构错误 type={movie_type} page={page}: {detail}"
            ),
        }
    }
}

impl std::error::Error for LatestPageError {}

/// 生成 `jdsignature` 请求头（上游 `javdb.py:_get_sign`）。
pub fn jdsignature(timestamp: u64) -> String {
    let secret = format!("{timestamp}{SIGN_SECRET_SUFFIX}");
    let sign = format!("{:x}", md5::compute(secret.as_bytes()));
    format!("{timestamp}.lpw6vgqzsp.{sign}")
}

/// 当前时间的 `jdsignature`。
pub fn jdsignature_now() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    jdsignature(ts)
}

/// 构造列表请求的 URL（上游 `fetch_latest_page` 的参数组装）。
pub fn latest_page_url(host: &str, movie_type: u8, page: u32) -> String {
    // 上游用 urlencode(quote_via=quote, safe=':-')；这里手拼，值都是数字与固定词。
    format!(
        "https://{host}/api/v1/movies/tags?filter_by={movie_type}%3At&sort_by=release&order_by=desc&page={page}&limit={PAGE_SIZE}"
    )
}

/// 校验并解析列表响应（上游 `fetch_latest_page` 的响应校验部分）。
///
/// 请求/结构错误返回 [`LatestPageError`]；正常空页返回空 `Vec`（调用方据此停翻）。
pub fn parse_latest_page(
    payload: &Value,
    movie_type: u8,
    page: u32,
) -> Result<Vec<LatestItem>, LatestPageError> {
    let bad = |detail: String| LatestPageError::BadResponse {
        movie_type,
        page,
        detail,
    };
    let obj = payload
        .as_object()
        .ok_or_else(|| bad("响应不是对象".to_owned()))?;
    if obj.get("success").and_then(Value::as_u64) != Some(1) {
        return Err(bad("success != 1".to_owned()));
    }
    let data = obj
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("data 不是对象".to_owned()))?;
    let movies = data
        .get("movies")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("movies 不是数组".to_owned()))?;
    if movies.len() > PAGE_SIZE {
        return Err(bad(format!("movies 超过 {PAGE_SIZE} 条")));
    }
    if !movies.is_empty() && data.get("current_page").and_then(Value::as_u64) != Some(page as u64) {
        return Err(bad(format!(
            "current_page 与请求不一致: {:?}",
            data.get("current_page")
        )));
    }
    let mut items = Vec::with_capacity(movies.len());
    for movie in movies {
        let m = movie
            .as_object()
            .ok_or_else(|| bad("列表影片项不是对象".to_owned()))?;
        let javdb_id = m
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("列表影片缺少 ID".to_owned()))?;
        let number = m
            .get("number")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("列表影片缺少番号".to_owned()))?;
        let release_date = m
            .get("release_date")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(LatestItem {
            javdb_id: javdb_id.to_owned(),
            number: number.to_uppercase(),
            release_date,
        });
    }
    Ok(items)
}

/// 详情接口 `/api/v4/movies/{javdb_id}` 的 URL。
pub fn detail_url(host: &str, javdb_id: &str) -> String {
    format!("https://{host}/api/v4/movies/{javdb_id}")
}

/// 从详情响应里提取热度输入（上游 `movie_heat(detail)` 读的四个字段）。
///
/// 字段缺失按 0 处理（上游 `getattr(detail, ..., 0)`）。
pub fn heat_inputs_from_detail(payload: &Value) -> HeatInputs {
    let data = payload.get("data").unwrap_or(payload);
    let get = |key: &str| -> u64 {
        data.get(key)
            .and_then(Value::as_u64)
            .or_else(|| {
                data.get(key)
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    HeatInputs {
        watched_count: get("watched_count"),
        want_watch_count: get("want_watch_count"),
        comment_count: get("comment_count"),
        score_number: get("score_number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// jdsignature 格式：`{ts}.lpw6vgqzsp.{32 位 hex}`。
    #[test]
    fn signature_format() {
        let sig = jdsignature(1700000000);
        let parts: Vec<&str> = sig.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "1700000000");
        assert_eq!(parts[1], "lpw6vgqzsp");
        assert_eq!(parts[2].len(), 32);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
        // 同一时间戳签名稳定
        assert_eq!(jdsignature(1700000000), sig);
    }

    /// 正常列表响应解析。
    #[test]
    fn parse_ok() {
        let payload = json!({
            "success": 1,
            "data": {
                "current_page": 1,
                "movies": [
                    {"id": "abc123", "number": "ssis-001", "release_date": "2026-01-01"},
                    {"id": "def456", "number": "SSIS-002", "release_date": null},
                ],
            },
        });
        let items = parse_latest_page(&payload, 0, 1).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].number, "SSIS-001", "番号转大写");
        assert_eq!(items[1].release_date, None);
    }

    /// success != 1 报错。
    #[test]
    fn parse_bad_success() {
        let payload = json!({"success": 0, "data": {}});
        assert!(matches!(
            parse_latest_page(&payload, 0, 1),
            Err(LatestPageError::BadResponse { .. })
        ));
    }

    /// 页码不一致报错。
    #[test]
    fn parse_page_mismatch() {
        let payload = json!({
            "success": 1,
            "data": {"current_page": 2, "movies": [{"id": "a", "number": "b"}]},
        });
        assert!(parse_latest_page(&payload, 0, 1).is_err());
    }

    /// 缺少番号报错。
    #[test]
    fn parse_missing_number() {
        let payload = json!({
            "success": 1,
            "data": {"current_page": 1, "movies": [{"id": "a"}]},
        });
        assert!(parse_latest_page(&payload, 0, 1).is_err());
    }

    /// 空列表是正常停翻信号。
    #[test]
    fn parse_empty_ok() {
        let payload = json!({"success": 1, "data": {"current_page": 99, "movies": []}});
        assert_eq!(parse_latest_page(&payload, 0, 99).unwrap(), vec![]);
    }

    /// 热度字段缺失按 0。
    #[test]
    fn heat_inputs_missing_zero() {
        let inputs = heat_inputs_from_detail(&json!({"data": {}}));
        assert_eq!(inputs.heat(), 0);
    }
}
