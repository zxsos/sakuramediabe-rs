//! JavLibrary 排行榜 HTML 适配。
//!
//! # 上游对应：`sakuramedia_more_rank_movies/javlibrary.py`
//!
//! | 上游 | 这里 |
//! |---|----|
//! | `_VideoIdParser` / `parse_javlibrary_ranking` | [`parse_ranking`] |
//! | `JavLibraryClient.get_bestrated_numbers` | [`JavLibraryClient::get_bestrated_numbers`] |
//! | `JavLibraryClient.get_mostwanted_numbers` | [`JavLibraryClient::get_mostwanted_numbers`] |
//! | `JAVLIBRARY_MODE_BY_PERIOD` | [`mode_for_period`] |

use std::collections::HashSet;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::html::{events, Event};

pub const JAVLIBRARY_BASE: &str = "https://www.f101w.com";
pub const JAVLIBRARY_HOST: &str = "www.f101w.com";
const BESTRATED_PATH: &str = "vl_bestrated.php";
const MOSTWANTED_PATH: &str = "vl_mostwanted.php";

/// 上游 `JAVLIBRARY_MAX_PAGES`。
pub const MAX_PAGES: u32 = 25;

/// 上游 `JAVLIBRARY_MODE_BY_PERIOD`：上个月=1、全部=2。
pub fn mode_for_period(period: &str) -> Option<&'static str> {
    match period.trim().to_ascii_lowercase().as_str() {
        "monthly" => Some("1"),
        "all" => Some("2"),
        _ => None,
    }
}

/// 上游 `JAVLIBRARY_PERIODS`。
pub const PERIODS: [&str; 2] = ["monthly", "all"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavLibraryError {
    /// 上游 `JavLibraryRankingError`。
    Ranking(String),
    /// 上游 `ValueError`（不支持的周期）。
    BadPeriod(String),
    /// HTTP 失败。
    Http(String),
}

impl std::fmt::Display for JavLibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ranking(d) => write!(f, "JavLibrary 榜单错误: {d}"),
            Self::BadPeriod(d) => write!(f, "不支持的 JavLibrary 榜单周期: {d}"),
            Self::Http(d) => write!(f, "JavLibrary 请求失败: {d}"),
        }
    }
}

impl std::error::Error for JavLibraryError {}

/// 按榜单顺序解析番号（上游 `parse_javlibrary_ranking`）。
///
/// 只提取每个 `div.video` 内的 `div.id` 番号；无 `div.video` 或全部缺番号时
/// 返回空列表（调用方据此停翻页）。
pub fn parse_ranking(content: &str) -> Vec<String> {
    let code_re = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{1,127}$").unwrap();
    let mut items = Vec::new();
    let mut video_depth: usize = 0;
    let mut id_captured = false;
    let mut id_depth: usize = 0;
    let mut buffer = String::new();

    let finish_id = |items: &mut Vec<String>, buffer: &mut String, id_depth: &mut usize| {
        let code: String = buffer.split_whitespace().collect::<Vec<_>>().join(" ");
        if !code.is_empty() && code_re.is_match(&code) {
            items.push(code);
        }
        *id_depth = 0;
        buffer.clear();
    };

    for event in events(content) {
        match event {
            Event::Start(tag) => {
                if !tag.is("div") {
                    continue;
                }
                if video_depth == 0 {
                    if tag.has_class("video") {
                        video_depth = 1;
                        id_captured = false;
                    }
                    continue;
                }
                video_depth += 1;
                if tag.has_class("id") && !id_captured {
                    id_captured = true;
                    id_depth = video_depth;
                    buffer.clear();
                }
            }
            Event::End(name) => {
                if !name.eq_ignore_ascii_case("div") || video_depth == 0 {
                    continue;
                }
                if id_depth == video_depth {
                    finish_id(&mut items, &mut buffer, &mut id_depth);
                }
                video_depth -= 1;
            }
            Event::Text(text) => {
                if id_depth > 0 {
                    buffer.push_str(&text);
                }
            }
        }
    }
    items
}

/// JavLibrary 榜单客户端（上游 `JavLibraryClient`）。
pub struct JavLibraryClient {
    client: reqwest::Client,
    request_interval: Duration,
    max_pages: u32,
    last_request_at: Option<Instant>,
}

impl JavLibraryClient {
    pub fn new(
        timeout: Duration,
        request_interval: Duration,
        max_pages: u32,
    ) -> Result<Self, JavLibraryError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/120.0.0.0 Safari/537.36",
            )
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    reqwest::header::ACCEPT_LANGUAGE,
                    "zh-CN,zh;q=0.9".parse().unwrap(),
                );
                h.insert(
                    reqwest::header::REFERER,
                    format!("{JAVLIBRARY_BASE}/cn/").parse().unwrap(),
                );
                h
            })
            .build()
            .map_err(|e| JavLibraryError::Http(format!("构建 HTTP 客户端失败: {e}")))?;
        Ok(Self {
            client,
            request_interval,
            max_pages,
            last_request_at: None,
        })
    }

    async fn request_page(
        &mut self,
        url: &str,
        page_name: &str,
    ) -> Result<String, JavLibraryError> {
        if let Some(last) = self.last_request_at {
            let elapsed = last.elapsed();
            if elapsed < self.request_interval {
                tokio::time::sleep(self.request_interval - elapsed).await;
            }
        }
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| JavLibraryError::Http(format!("{page_name}请求失败: {e}")))?;
        self.last_request_at = Some(Instant::now());
        if response.status() != reqwest::StatusCode::OK {
            return Err(JavLibraryError::Http(format!(
                "{page_name}返回 HTTP {}",
                response.status()
            )));
        }
        // 上游校验「发生未知跳转」
        let final_url = response.url().clone();
        if final_url.host_str().unwrap_or("").to_ascii_lowercase() != JAVLIBRARY_HOST {
            return Err(JavLibraryError::Ranking(format!("{page_name}发生未知跳转")));
        }
        response
            .text()
            .await
            .map_err(|e| JavLibraryError::Http(format!("{page_name}读取正文失败: {e}")))
    }

    async fn rank_numbers(
        &mut self,
        path: &str,
        period: &str,
    ) -> Result<Vec<String>, JavLibraryError> {
        let mode =
            mode_for_period(period).ok_or_else(|| JavLibraryError::BadPeriod(period.to_owned()))?;
        let mut numbers = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=self.max_pages {
            let url = format!("{JAVLIBRARY_BASE}/cn/{path}?&mode={mode}&page={page}");
            let html = self
                .request_page(&url, &format!("榜单第 {page} 页"))
                .await?;
            let items = parse_ranking(&html);
            if items.is_empty() {
                break;
            }
            for code in items {
                if seen.insert(code.clone()) {
                    numbers.push(code);
                }
            }
        }
        if numbers.is_empty() {
            return Err(JavLibraryError::Ranking("榜单没有可用番号".to_owned()));
        }
        Ok(numbers)
    }

    /// 高评价榜（上游 `get_bestrated_numbers`）。
    pub async fn get_bestrated_numbers(
        &mut self,
        period: &str,
    ) -> Result<Vec<String>, JavLibraryError> {
        self.rank_numbers(BESTRATED_PATH, period).await
    }

    /// 最想要榜（上游 `get_mostwanted_numbers`）。
    pub async fn get_mostwanted_numbers(
        &mut self,
        period: &str,
    ) -> Result<Vec<String>, JavLibraryError> {
        self.rank_numbers(MOSTWANTED_PATH, period).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANKING_HTML: &str = r#"
<html><body>
<div class="video"><div class="id">SSIS-001</div></div>
<div class="video"><div class="id">ssis-002</div></div>
<div class="video"><div class="title">没有番号的条目</div></div>
</body></html>"#;

    #[test]
    fn parse_ranking_ok() {
        let items = parse_ranking(RANKING_HTML);
        // 注意：上游不对番号做大小写归一（minnano 那边才 upper）
        assert_eq!(items, vec!["SSIS-001", "ssis-002"]);
    }

    #[test]
    fn parse_ranking_empty() {
        assert!(parse_ranking("<html></html>").is_empty());
    }

    #[test]
    fn mode_for_period_ok() {
        assert_eq!(mode_for_period("monthly"), Some("1"));
        assert_eq!(mode_for_period("all"), Some("2"));
        assert_eq!(mode_for_period("daily"), None);
    }
}
