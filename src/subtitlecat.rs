//! SubtitleCat 搜索与中文字幕下载。
//!
//! # 上游对应：`subtitlecat.py`
//!
//! 逐条照搬：`_LinkParser` / `_collect_links` / `normalize_movie_number` /
//! `_numbers_equal` / `_extract_movie_numbers` / `_subtitle_bytes` /
//! `SubtitleCatClient.fetch_chinese_subtitles`。
//!
//! # 与上游不同的地方
//!
//! 1. **用 `reqwest`（async）替代 `httpx`（sync）**：插件是 async gRPC 服务，
//!    阻塞客户端会卡住 tokio 运行时。
//! 2. **重试退避不用 `time.sleep`**：用 `tokio::time::sleep`。
//! 3. **配置从宿主写的文件读**，不是 `context.settings`。

use std::time::Duration;

use regex::Regex;
use reqwest::{Client, Url};

use crate::html::collect_links;
use crate::settings::Settings;

/// 上游 `USER_AGENT`。
const USER_AGENT: &str = "SakuraMedia-SubtitleCat/0.1";

/// 中文字幕下载锚点的 `id`（上游 `CHINESE_DOWNLOAD_ANCHOR_ID`）。
const CHINESE_DOWNLOAD_ANCHOR_ID: &str = "download_zh-CN";

/// 搜索结果列表的祖先 class（上游 `ancestor_class="subtitles"`）。
const SEARCH_RESULT_CLASS: &str = "subtitles";

/// 上游 `_MOVIE_NUMBER_PATTERN`。
///
/// 原版用了 look-around（`(?<!…)` / `(?!…)`），Rust 的 `regex` 不支持，改写为
/// 无需 look-around 的版本：匹配核心部分，边界由调用方按需处理。
/// `(?i)`：上游 `re.IGNORECASE`。
const MOVIE_NUMBER_PATTERN: &str = r"(?i)(?:FC2[-_ ]?PPV[-_ ]?|[A-Z]{2,6}[-_ ]?)\d{2,6}";

/// 抓取失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleCatError {
    /// HTTP 请求失败（网络、超时、5xx 重试耗尽）。
    Request(String),
    /// 站点返回了非字幕内容。
    InvalidSubtitle(String),
    /// 4xx：上游直接抛，不重试。
    ClientError(String),
}

impl std::fmt::Display for SubtitleCatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(msg) => write!(f, "SubtitleCat 请求失败: {msg}"),
            Self::InvalidSubtitle(msg) => {
                write!(f, "SubtitleCat 返回内容不是有效的 SRT 字幕: {msg}")
            }
            Self::ClientError(msg) => write!(f, "SubtitleCat 请求失败: {msg}"),
        }
    }
}

impl std::error::Error for SubtitleCatError {}

/// 统一人工输入和链接中的番号分隔符（上游 `normalize_movie_number`）。
pub fn normalize_movie_number(value: &str) -> String {
    let re = Regex::new(r"[-_\s]+").expect("常量正则必合法");
    let normalized = re
        .replace_all(value.trim().to_uppercase().as_str(), "-")
        .into_owned();
    if let Some(suffix) = normalized.strip_prefix("FC2PPV") {
        let suffix = suffix.trim_start_matches('-');
        return if suffix.is_empty() {
            "FC2-PPV".to_owned()
        } else {
            format!("FC2-PPV-{suffix}")
        };
    }
    normalized
}

/// 番号相等（忽略 `-`，上游 `_numbers_equal`）。
fn numbers_equal(left: &str, right: &str) -> bool {
    left == right || left.replace('-', "") == right.replace('-', "")
}

/// 从文本中提取番号（上游 `_extract_movie_numbers`）。
fn extract_movie_numbers(value: &str) -> Vec<String> {
    let re = Regex::new(MOVIE_NUMBER_PATTERN).expect("常量正则必合法");
    re.find_iter(value)
        .map(|m| normalize_movie_number(m.as_str()))
        .collect()
}

/// SubtitleCat 单部影片客户端。
pub struct SubtitleCatClient {
    settings: Settings,
    client: Client,
    number_re: Regex,
}

impl SubtitleCatClient {
    pub fn new(settings: &Settings) -> Result<Self, SubtitleCatError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs_f64(settings.request_timeout_seconds))
            // 上游 `follow_redirects=True`。
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|e| SubtitleCatError::Request(e.to_string()))?;
        Ok(Self {
            settings: settings.clone(),
            client,
            number_re: Regex::new(MOVIE_NUMBER_PATTERN).expect("常量正则必合法"),
        })
    }

    /// 抓取一部影片所有搜索结果中的简体中文字幕（上游 `fetch_chinese_subtitles`）。
    ///
    /// 返回 SRT 字节列表（已校验是有效字幕）。
    pub async fn fetch_chinese_subtitles(
        &self,
        movie_number: &str,
    ) -> Result<Vec<Vec<u8>>, SubtitleCatError> {
        let number = normalize_movie_number(movie_number);
        let search_url = self
            .settings
            .base_url
            .join("index.php")
            .map_err(|e| SubtitleCatError::Request(e.to_string()))?;

        let search_html = self
            .get_text(search_url.clone(), &[("search", number.as_str())])
            .await?;

        // 搜索结果页：只收 `subtitles` class 内的链接，且链接里的番号要对上。
        let detail_links = collect_links(&search_html, Some(SEARCH_RESULT_CLASS), None);
        let mut matching_detail_links = Vec::new();
        for link in detail_links {
            let absolute = self.resolve(&search_url, &link)?;
            if extract_movie_numbers(&link)
                .iter()
                .any(|candidate| numbers_equal(candidate, &number))
            {
                matching_detail_links.push(absolute);
            }
        }

        let mut subtitles = Vec::new();
        let mut seen_download_links = std::collections::HashSet::new();
        for detail_link in matching_detail_links {
            let detail_html = self.get_text(detail_link.clone(), &[]).await?;
            let download_links =
                collect_links(&detail_html, None, Some(CHINESE_DOWNLOAD_ANCHOR_ID));
            for download_link in download_links {
                let absolute = self.resolve(&detail_link, &download_link)?;
                let key = absolute.as_str().to_owned();
                if !seen_download_links.insert(key) {
                    continue;
                }
                let bytes = self.get_bytes(absolute).await?;
                subtitles.push(subtitle_bytes(&bytes)?);
            }
        }
        Ok(subtitles)
    }

    /// GET 文本（带重试）。
    async fn get_text(
        &self,
        url: Url,
        params: &[(&str, &str)],
    ) -> Result<String, SubtitleCatError> {
        let bytes = self.get(url, params).await?;
        String::from_utf8(bytes).map_err(|e| SubtitleCatError::Request(e.to_string()))
    }

    /// GET 字节（带重试）。
    async fn get_bytes(&self, url: Url) -> Result<Vec<u8>, SubtitleCatError> {
        self.get(url, &[]).await
    }

    /// 带重试的 GET（上游 `_get`：4xx 直接抛，5xx/网络错误退避重试）。
    async fn get(&self, url: Url, params: &[(&str, &str)]) -> Result<Vec<u8>, SubtitleCatError> {
        let attempts = self.settings.request_retries as usize + 1;
        let mut last_error: Option<SubtitleCatError> = None;

        // 手动拼 query（避免 reqwest feature 组合问题）。
        let mut url = url;
        if !params.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in params {
                pairs.append_pair(k, v);
            }
            drop(pairs);
        }

        for attempt in 0..attempts {
            let request = self.client.get(url.clone());
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return response
                            .bytes()
                            .await
                            .map(|b| b.to_vec())
                            .map_err(|e| SubtitleCatError::Request(e.to_string()));
                    }
                    // 上游：4xx 直接抛，不重试。
                    if status.is_client_error() {
                        return Err(SubtitleCatError::ClientError(format!(
                            "HTTP {}",
                            status.as_u16()
                        )));
                    }
                    last_error = Some(SubtitleCatError::Request(format!(
                        "HTTP {}",
                        status.as_u16()
                    )));
                }
                Err(e) => {
                    // 超时也算可重试（上游 `httpx.RequestError` 分支）。
                    if e.is_status() {
                        if let Some(status) = e.status() {
                            if status.is_client_error() {
                                return Err(SubtitleCatError::ClientError(format!(
                                    "HTTP {}",
                                    status.as_u16()
                                )));
                            }
                        }
                    }
                    last_error = Some(SubtitleCatError::Request(e.to_string()));
                }
            }

            // 退避：上游 `time.sleep(0.5 * (2**attempt))`。
            if attempt + 1 < attempts {
                let delay = Duration::from_secs_f64(0.5 * (2_u32.pow(attempt as u32) as f64));
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.unwrap_or_else(|| SubtitleCatError::Request(format!("请求失败: {url}"))))
    }

    /// 相对 URL 按基址解析（上游 `urljoin`）。
    fn resolve(&self, base: &Url, href: &str) -> Result<Url, SubtitleCatError> {
        base.join(href)
            .map_err(|e| SubtitleCatError::Request(e.to_string()))
    }

    #[allow(dead_code)]
    fn _use_number_re(&self) -> &Regex {
        &self.number_re
    }
}

/// 将响应转为 UTF-8，并拒绝明显不是字幕的页面（上游 `_subtitle_bytes`）。
fn subtitle_bytes(bytes: &[u8]) -> Result<Vec<u8>, SubtitleCatError> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    if text.trim().is_empty() || !text.contains("-->") {
        return Err(SubtitleCatError::InvalidSubtitle(
            "内容中没有 SRT 时间轴标记".to_owned(),
        ));
    }
    Ok(text.as_bytes().to_vec())
}

// 兼容上游命名：测试里直接用。
#[allow(dead_code)]
fn _numbers_equal(left: &str, right: &str) -> bool {
    numbers_equal(left, right)
}

#[allow(dead_code)]
fn _extract_movie_numbers(value: &str) -> Vec<String> {
    extract_movie_numbers(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_like_upstream() {
        assert_eq!(normalize_movie_number("ssni 888"), "SSNI-888");
        assert_eq!(normalize_movie_number("ssni_888"), "SSNI-888");
        assert_eq!(normalize_movie_number("SSNI-888"), "SSNI-888");
        assert_eq!(normalize_movie_number("fc2ppv 123456"), "FC2-PPV-123456");
        assert_eq!(normalize_movie_number("FC2-PPV-123456"), "FC2-PPV-123456");
    }

    #[test]
    fn numbers_equal_ignores_dashes() {
        assert!(numbers_equal("SSNI-888", "SSNI888"));
        assert!(numbers_equal("SSNI-888", "SSNI-888"));
        assert!(!numbers_equal("SSNI-888", "SSNI-889"));
    }

    #[test]
    fn extracts_movie_numbers_from_text() {
        let numbers = extract_movie_numbers("watch SSNI-888 and abc-123 online");
        assert!(numbers.contains(&"SSNI-888".to_owned()));
        assert!(numbers.contains(&"ABC-123".to_owned()));
    }

    #[test]
    fn rejects_non_srt_content() {
        assert!(subtitle_bytes(b"<html>not a subtitle</html>").is_err());
        assert!(subtitle_bytes(b"").is_err());
        assert!(subtitle_bytes(b"   ").is_err());
    }

    #[test]
    fn accepts_valid_srt() {
        let srt = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n";
        assert!(subtitle_bytes(srt).is_ok());
    }

    #[test]
    fn strips_bom_before_validation() {
        let mut srt = b"\xef\xbb\xbf".to_vec();
        srt.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n");
        let out = subtitle_bytes(&srt).expect("BOM 不该影响校验");
        assert!(!out.starts_with(b"\xef\xbb\xbf"));
    }

    // 抑制未使用警告：number_re 在当前实现里暂未直接使用，
    // 但保留它以便后续按上游 `_MOVIE_NUMBER_PATTERN` 做更复杂的匹配。
    #[test]
    fn _number_re_is_compiled() {
        let settings = Settings::default();
        let client = SubtitleCatClient::new(&settings).expect("客户端必能建");
        assert!(client._use_number_re().is_match("SSNI-888"));
    }
}
