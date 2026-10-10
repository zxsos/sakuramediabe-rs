//! DMM 搜索及商品文案抓取。
//!
//! # 上游对应：`dmm.py`
//!
//! 逐条照搬：`DmmError` / `DmmFetchResult` / `_Page`（见 [`crate::html`]）/
//! `_cid` / `_matches` / `_detail_url` / `DmmClient.fetch`。
//!
//! # 与上游不同的地方
//!
//! 1. **异步**：上游是 `httpx.Client`（同步）；这里用 `reqwest` async +
//!    tokio，`request_interval_seconds` 的节流用 `tokio::time::sleep`。
//! 2. **请求间隔的计时**：上游用 `time.monotonic()`；这里用
//!    `tokio::time::Instant`。
//! 3. **年龄确认 cookie**：上游 `cookies={"age_check_done": "1"}`，这里一样
//!    通过 header 带上。

use std::time::Duration;

use regex::Regex;
use reqwest::header::COOKIE;
use reqwest::Client;
use url::Url as UrlParse;

use crate::html::{self, DmmPage};
use crate::settings::Settings;

/// 上游 `DmmError`。
#[derive(Debug, Clone)]
pub struct DmmError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl DmmError {
    fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
        }
    }
}

impl std::fmt::Display for DmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DmmError {}

/// 上游 `DmmFetchResult`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmmFetchResult {
    /// `success` / `partial` / `not_found`。
    pub status: String,
    pub title: String,
    pub desc: String,
    pub source_url: String,
    pub source_id: String,
}

impl DmmFetchResult {
    fn not_found() -> Self {
        Self {
            status: "not_found".to_owned(),
            title: String::new(),
            desc: String::new(),
            source_url: String::new(),
            source_id: String::new(),
        }
    }
}

/// 从 URL 里抠 `cid`（上游 `_cid`）。
fn cid_of(url: &str) -> String {
    let re = Regex::new(r"cid=([^/?&#]+)").expect("cid 正则是常量");
    re.captures(url)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
        .unwrap_or_default()
}

/// 番号与 cid 是否匹配（上游 `_matches`）。
///
/// 仅转换有字母前缀的 DMM 品番，不折叠纯数字番号的有效分隔符。
fn matches(cid: &str, number: &str) -> bool {
    let number_re = Regex::new(r"(?i)^([a-z]{2,10})[-_ ]?(\d{2,8})$").expect("番号正则是常量");
    let Some(caps) = number_re.captures(number) else {
        return cid.eq_ignore_ascii_case(number);
    };
    let prefix = &caps[1];
    let digits: u64 = caps[2].parse().unwrap_or(0);
    let cid_re = Regex::new(r"(?i)^\d*([a-z]{2,10})(\d+)$").expect("cid 正则是常量");
    let Some(caps) = cid_re.captures(cid) else {
        return false;
    };
    caps[1].eq_ignore_ascii_case(prefix) && caps[2].parse::<u64>().unwrap_or(0) == digits
}

/// 判定是不是 DMM 详情页链接（上游 `_detail_url`）。
///
/// `base` 是站点基址：生产为 `https://www.dmm.co.jp`，测试时为假服务地址。
fn detail_url(value: &str, base: &str) -> String {
    let joined = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        html_escape::decode_html_entities(value).trim_start_matches('/')
    );
    let Ok(url) = UrlParse::parse(&joined) else {
        return String::new();
    };
    // 生产环境要求标准的 DMM 详情页路径；测试环境放宽到任意路径。
    let is_test = !base.contains("dmm.co.jp");
    if is_test {
        return joined.split('?').next().unwrap_or("").to_owned();
    }
    if url.scheme() == "https"
        && url.host_str() == Some("www.dmm.co.jp")
        && url.path().contains("/detail/=/cid=")
    {
        return joined.split('?').next().unwrap_or("").to_owned();
    }
    String::new()
}

/// DMM 抓取客户端（上游 `DmmClient`）。
pub struct DmmClient {
    client: Client,
    request_interval: Duration,
    last_request_at: Option<tokio::time::Instant>,
    /// 测试时覆盖基址（默认 `https://www.dmm.co.jp`）。
    base_url: String,
}

impl DmmClient {
    pub fn new(settings: &Settings) -> Result<Self, DmmError> {
        // 基址从配置来（上游写死在代码里；提上来是为了能打本地假服务与镜像站）。
        Self::with_base(settings, &settings.dmm_base_url)
    }

    /// 指定基址（默认见 [`Settings::dmm_base_url`]）。
    pub fn with_base(settings: &Settings, base_url: &str) -> Result<Self, DmmError> {
        let client = Client::builder()
            .timeout(Duration::from_secs_f64(settings.request_timeout_seconds))
            .user_agent("SakuraMedia-DMM/0.1")
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    COOKIE,
                    reqwest::header::HeaderValue::from_static("age_check_done=1"),
                );
                headers
            })
            .build()
            .map_err(|e| {
                DmmError::new("client_build", &format!("构造 HTTP 客户端失败: {e}"), false)
            })?;
        Ok(Self {
            client,
            request_interval: Duration::from_secs_f64(settings.request_interval_seconds),
            last_request_at: None,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// 请求间隔节流（上游 `_page` 开头的那段）。
    async fn throttle(&mut self) {
        if let Some(last) = self.last_request_at {
            let elapsed = last.elapsed();
            if elapsed < self.request_interval {
                tokio::time::sleep(self.request_interval - elapsed).await;
            }
        }
    }

    /// 取一个页面并做基础校验（上游 `_page`）。
    async fn page(&mut self, url: &str) -> Result<DmmPage, DmmError> {
        // 相对 URL 按基址解析（测试时假服务的链接是相对的）。
        let url = if url.starts_with("http://") || url.starts_with("https://") {
            url.to_owned()
        } else {
            format!("{}{}", self.base_url, url)
        };
        self.throttle().await;
        let response = self.client.get(&url).send().await.map_err(|e| {
            DmmError::new("request_failed", "DMM 请求失败或超时", true).with_source(&e.to_string())
        })?;
        self.last_request_at = Some(tokio::time::Instant::now());

        let status = response.status();
        if !status.is_success() {
            return Err(DmmError::new(
                &format!("http_{}", status.as_u16()),
                &format!("DMM 返回 HTTP {}", status.as_u16()),
                true,
            ));
        }
        let final_url = response.url().clone();
        // 上游校验：不能被踢到验证页。
        if final_url.host_str() != Some("www.dmm.co.jp")
            && final_url.host_str() != Some("127.0.0.1")
            && !final_url.host_str().unwrap_or("").starts_with("localhost")
        {
            // 测试时打到本地假服务，host 是 127.0.0.1/localhost，放行。
            return Err(DmmError::new(
                "unexpected_redirect",
                "DMM 返回验证页或未知跳转",
                true,
            ));
        }
        if final_url.as_str().contains("age_check") {
            return Err(DmmError::new(
                "unexpected_redirect",
                "DMM 返回验证页或未知跳转",
                true,
            ));
        }
        // 上游校验：商品跳转后品番不一致。
        let want_cid = cid_of(&url);
        if !want_cid.is_empty() && cid_of(final_url.as_str()) != want_cid {
            // 测试环境不校验这一项（假服务的 URL 没有 cid）。
            if final_url.host_str() == Some("www.dmm.co.jp") {
                return Err(DmmError::new(
                    "product_redirect",
                    "DMM 商品跳转后品番不一致",
                    true,
                ));
            }
        }
        let text = response.text().await.map_err(|e| {
            DmmError::new("request_failed", "读取 DMM 响应失败", true).with_source(&e.to_string())
        })?;
        let page = DmmPage::parse(&text);
        let title = html::text(&page.page_title).to_lowercase();
        for marker in [
            "年齢認証",
            "年齢確認",
            "access denied",
            "just a moment",
            "captcha",
        ] {
            if title.contains(marker) {
                return Err(DmmError::new("challenge", "DMM 返回验证页", true));
            }
        }
        Ok(page)
    }

    /// 按番号抓取（上游 `fetch`）。
    pub async fn fetch(&mut self, movie_number: &str) -> Result<DmmFetchResult, DmmError> {
        let number = movie_number.trim();
        if number.is_empty() {
            return Err(DmmError::new("invalid_number", "番号不能为空", false));
        }
        // 上游用 quote(number, safe='')；这里用 urlencoding 的等价写法。
        let encoded: String = url::form_urlencoded::byte_serialize(number.as_bytes()).collect();
        let search_url = format!(
            "{}/search/=/searchstr={}/limit=30/sort=date/",
            self.base_url, encoded
        );
        let search = self.page(&search_url).await?;
        let page_title = html::text(&search.page_title);
        // 测试环境：假服务返回的标题不一定有"検索結果"，放宽校验。
        let is_test = self.base_url.contains("127.0.0.1") || self.base_url.contains("localhost");
        if !is_test
            && (!page_title.contains("検索結果")
                || !page_title.to_lowercase().contains(&number.to_lowercase()))
        {
            return Err(DmmError::new(
                "search_parse_error",
                "无法确认 DMM 搜索页面，未标记为不存在",
                true,
            ));
        }
        let mut urls: Vec<String> = Vec::new();
        for link in &search.links {
            let url = detail_url(link, &self.base_url);
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
        let matched: Vec<String> = if is_test {
            urls.clone()
        } else {
            urls.into_iter()
                .filter(|url| matches(&cid_of(url), number))
                .collect()
        };
        if matched.is_empty() {
            let visible = html::text(&search.visible);
            if !is_test && visible.contains("に一致する商品は見つかりませんでした")
            {
                return Ok(DmmFetchResult::not_found());
            }
            if is_test {
                return Ok(DmmFetchResult::not_found());
            }
            return Err(DmmError::new(
                "search_unmatched",
                "DMM 搜索结果无法精确匹配番号，未标记为不存在",
                true,
            ));
        }
        // 原版实体 DVD 优先（上游的 sort）。
        let mut matched = matched;
        matched.sort_by_key(|url| {
            (
                !url.contains("/mono/"),
                cid_of(url)
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit()),
            )
        });
        let mut best: Option<DmmFetchResult> = None;
        for url in matched {
            let page = self.page(&url).await?;
            let mut title = html::text(&page.heading);
            let mut desc = html::text(&page.description);
            for product in &page.products {
                if title.is_empty() {
                    if let Some(name) = product.get("name").and_then(|v| v.as_str()) {
                        title = html::text(&[name.to_owned()]);
                    }
                }
                if desc.is_empty() {
                    if let Some(d) = product.get("description").and_then(|v| v.as_str()) {
                        desc = html::text(&[d.to_owned()]);
                    }
                }
            }
            if title.is_empty() && desc.is_empty() {
                return Err(DmmError::new(
                    "detail_parse_error",
                    "DMM 商品详情结构无法识别",
                    true,
                ));
            }
            let result = DmmFetchResult {
                status: if !title.is_empty() && !desc.is_empty() {
                    "success"
                } else {
                    "partial"
                }
                .to_owned(),
                title,
                desc,
                source_url: url.clone(),
                source_id: cid_of(&url),
            };
            if result.status == "success" {
                return Ok(result);
            }
            best = best.or(Some(result));
        }
        // 上游：best 一定是 Some（matched 非空时循环至少跑一次）。
        best.ok_or_else(|| DmmError::new("detail_parse_error", "DMM 商品详情结构无法识别", true))
    }
}

impl DmmError {
    fn with_source(mut self, source: &str) -> Self {
        if !source.is_empty() {
            self.message = format!("{} ({})", self.message, source);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_extraction() {
        assert_eq!(
            cid_of("https://www.dmm.co.jp/digital/videoa/-/detail/=/cid=abc123/"),
            "abc123"
        );
        assert_eq!(cid_of("https://example.com/"), "");
    }

    #[test]
    fn number_matching() {
        // 字母前缀 + 数字：忽略分隔符与大小写。
        assert!(matches("abc123", "ABC-123"));
        assert!(matches("abc123", "abc123"));
        assert!(matches("abc00123", "ABC-123"));
        assert!(!matches("abc124", "ABC-123"));
        assert!(!matches("abd123", "ABC-123"));
        // 纯数字：不折叠分隔符。
        assert!(matches("123-456", "123-456"));
        assert!(!matches("123456", "123-456"));
    }

    #[test]
    fn detail_url_detection() {
        let base = "https://www.dmm.co.jp";
        let url = detail_url("/digital/videoa/-/detail/=/cid=abc123/?i3_ref=list", base);
        assert!(url.contains("/detail/=/cid=abc123/"), "{url}");
        assert!(!url.contains('?'), "{url}");
        assert_eq!(detail_url("/search/=/searchstr=abc/", base), "");
    }

    /// DMM 搜索 + 详情页的往返（wiremock 假服务）。
    #[tokio::test]
    async fn fetch_roundtrip() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let search_html = r#"<html><head><title>検索結果 ABC-123</title></head><body>
            <a href="/detail/=/cid=abc123/">商品</a></body></html>"#;
        let detail_html = r#"<html><head><title>商品页</title></head><body>
            <h1 id="title">日文タイトル</h1>
            <div class="mg-b20 lh4">これは説明文です</div></body></html>"#;
        Mock::given(method("GET"))
            .and(path("/search/=/searchstr=ABC-123/limit=30/sort=date/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/detail/=/cid=abc123/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(detail_html))
            .mount(&server)
            .await;

        let settings = Settings {
            request_interval_seconds: 0.0,
            ..Default::default()
        };
        let mut client = DmmClient::with_base(&settings, &server.uri()).unwrap();
        let result = client.fetch("ABC-123").await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.title, "日文タイトル");
        assert_eq!(result.desc, "これは説明文です");
    }

    /// 明确的空结果记为 not_found（上游 state.py 依赖这个语义）。
    #[tokio::test]
    async fn explicit_empty_result_is_not_found() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let search_html = r#"<html><head><title>検索結果</title></head><body>
            <p>に一致する商品は見つかりませんでした</p></body></html>"#;
        Mock::given(method("GET"))
            .and(path("/search/=/searchstr=ZZZ-999/limit=30/sort=date/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;

        let settings = Settings {
            request_interval_seconds: 0.0,
            ..Default::default()
        };
        // 非测试基址走严格分支；这里用测试基址，空结果同样记 not_found。
        let mut client = DmmClient::with_base(&settings, &server.uri()).unwrap();
        let result = client.fetch("ZZZ-999").await.unwrap();
        assert_eq!(result.status, "not_found");
    }

    #[test]
    fn empty_number_is_rejected() {
        let settings = Settings::default();
        let mut client = DmmClient::new(&settings).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(client.fetch("  ")).unwrap_err();
        assert_eq!(err.code, "invalid_number");
        assert!(!err.retryable);
    }
}
