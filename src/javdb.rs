//! JavDB 榜单抓取：HTTP 拉榜单页，解析出番号列表（顺序即排名）。
//!
//! # 榜单与 URL
//!
//! | board_key | 路径 | 周期 |
//! |---|---|---|
//! | `hot` | `/` | 无（首页热播） |
//! | `top_rated` | `/rankings/movies` | daily/weekly/monthly |
//! | `censored` | `/rankings/movies?m=censored` | daily/weekly/monthly |
//! | `uncensored` | `/rankings/movies?m=uncensored` | daily/weekly/monthly |
//! | `fc2` | `/rankings/movies?m=fc2` | daily/weekly/monthly |
//! | `top250` | `/rankings/movies/top250` | 动态（年份） |
//!
//! # 解析
//!
//! 榜单页的影片链接形如 `<a href="/v/ABC-123">`。按文档顺序提取 `href`
//! 里 `/v/` 后面的番号，去重但保序 —— 顺序即排名，不能打乱。

use std::time::Duration;

use thiserror::Error;
use url::Url;

use crate::settings::Settings;

/// 榜单定义。
#[derive(Debug, Clone)]
pub struct Board {
    /// 榜单 key（如 `hot`）。
    pub key: &'static str,
    /// 显示名。
    pub display_name: &'static str,
    /// 静态周期；空表示无周期概念。
    pub periods: &'static [&'static str],
    /// 默认周期。
    pub default_period: &'static str,
    /// 是否动态周期（TOP250 按年份）。
    pub dynamic_periods: bool,
}

/// 全部榜单。
pub const BOARDS: &[Board] = &[
    Board {
        key: "hot",
        display_name: "热播",
        periods: &[],
        default_period: "all",
        dynamic_periods: false,
    },
    Board {
        key: "top_rated",
        display_name: "高评分",
        periods: &["daily", "weekly", "monthly"],
        default_period: "weekly",
        dynamic_periods: false,
    },
    Board {
        key: "censored",
        display_name: "有码",
        periods: &["daily", "weekly", "monthly"],
        default_period: "weekly",
        dynamic_periods: false,
    },
    Board {
        key: "uncensored",
        display_name: "无码",
        periods: &["daily", "weekly", "monthly"],
        default_period: "weekly",
        dynamic_periods: false,
    },
    Board {
        key: "fc2",
        display_name: "FC2",
        periods: &["daily", "weekly", "monthly"],
        default_period: "weekly",
        dynamic_periods: false,
    },
    Board {
        key: "top250",
        display_name: "TOP250",
        periods: &[],
        default_period: "all",
        dynamic_periods: true,
    },
];

/// 按 key 找榜单。
pub fn find_board(key: &str) -> Option<&'static Board> {
    BOARDS.iter().find(|b| b.key == key)
}

/// 抓取失败。
#[derive(Debug, Error)]
pub enum FetchError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("榜单不存在: {0}")]
    UnknownBoard(String),
    #[error("周期不支持: board={0} period={1}")]
    UnsupportedPeriod(String, String),
}

/// JavDB 榜单抓取器。
pub struct JavDbSource {
    client: reqwest::Client,
    base_url: Url,
}

impl JavDbSource {
    pub fn new(settings: &Settings) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.timeout_secs))
            .user_agent("Mozilla/5.0 (compatible; SakuraMedia/1.0)")
            .build()?;
        let base_url = Url::parse(settings.base_url.trim_end_matches('/'))?;
        Ok(Self { client, base_url })
    }

    #[cfg(test)]
    fn new_for_test(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: Url::parse(base_url).unwrap(),
        }
    }

    /// 榜单页 URL。
    fn board_url(&self, board: &Board, period: &str) -> Result<Url, FetchError> {
        let mut url = match board.key {
            "hot" => self.base_url.join("/")?,
            "top_rated" => self.base_url.join("/rankings/movies")?,
            "censored" => self.base_url.join("/rankings/movies?m=censored")?,
            "uncensored" => self.base_url.join("/rankings/movies?m=uncensored")?,
            "fc2" => self.base_url.join("/rankings/movies?m=fc2")?,
            "top250" => self.base_url.join("/rankings/movies/top250")?,
            _ => return Err(FetchError::UnknownBoard(board.key.to_owned())),
        };
        // 静态周期的榜单把 period 作为查询参数；hot/top250 不需要。
        if !board.periods.is_empty() && !period.is_empty() && period != "all" {
            if !board.periods.contains(&period) {
                return Err(FetchError::UnsupportedPeriod(
                    board.key.to_owned(),
                    period.to_owned(),
                ));
            }
            url.query_pairs_mut().append_pair("p", period);
        }
        Ok(url)
    }

    /// 抓取榜单，返回番号列表（顺序即排名）。
    pub async fn fetch_ranking(
        &self,
        board_key: &str,
        period: &str,
    ) -> Result<Vec<String>, FetchError> {
        let board =
            find_board(board_key).ok_or_else(|| FetchError::UnknownBoard(board_key.to_owned()))?;
        let url = self.board_url(board, period)?;
        let html = self.client.get(url).send().await?.text().await?;
        Ok(parse_ranking_page(&html))
    }
}

/// 从榜单页 HTML 提取番号列表（顺序即排名，去重保序）。
///
/// 影片链接形如 `<a href="/v/ABC-123">` 或 `<a href="/v/ABC-123/">`。
pub fn parse_ranking_page(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut rest = html;
    while let Some(a_pos) = find_tag_start(rest, "a") {
        let tag = &rest[a_pos..];
        let tag_end = tag.find('>').map(|i| a_pos + i).unwrap_or(rest.len());
        let tag_text = &rest[a_pos..tag_end];
        if let Some(href) = attr_value(tag_text, "href") {
            if let Some(number) = movie_number_from_href(&href) {
                if seen.insert(number.clone()) {
                    out.push(number);
                }
            }
        }
        rest = &rest[tag_end.min(rest.len())..];
        if rest.is_empty() {
            break;
        }
        // 前进一步，避免死循环。
        rest = &rest[1.min(rest.len())..];
    }
    out
}

/// 找 `<tag` 的起始位置（大小写不敏感）。
fn find_tag_start(html: &str, tag: &str) -> Option<usize> {
    let lower = html.to_lowercase();
    let needle = format!("<{tag}");
    lower.find(&needle)
}

/// 取标签内属性值（处理单引号/双引号/无引号）。
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let needle = format!("{name}=");
    let pos = lower.find(&needle)?;
    let mut v = tag[pos + needle.len()..].trim_start().to_string();
    if v.starts_with('"') {
        v.remove(0);
        Some(v.split('"').next().unwrap_or("").to_owned())
    } else if v.starts_with('\'') {
        v.remove(0);
        Some(v.split('\'').next().unwrap_or("").to_owned())
    } else {
        Some(
            v.split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
                .to_owned(),
        )
    }
}

/// 从 href 提取番号：`/v/ABC-123` 或 `/v/ABC-123/` → `ABC-123`。
fn movie_number_from_href(href: &str) -> Option<String> {
    // 去掉查询参数与 fragment。
    let path = href.split(['?', '#']).next()?;
    let rest = path.strip_prefix("/v/")?;
    let number = rest.trim_end_matches('/').trim();
    if number.is_empty() || number.contains('/') {
        return None;
    }
    Some(number.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boards_have_expected_keys() {
        let keys: Vec<_> = BOARDS.iter().map(|b| b.key).collect();
        assert_eq!(
            keys,
            vec![
                "hot",
                "top_rated",
                "censored",
                "uncensored",
                "fc2",
                "top250"
            ]
        );
    }

    #[test]
    fn top250_is_dynamic() {
        let b = find_board("top250").unwrap();
        assert!(b.dynamic_periods);
        assert!(b.periods.is_empty());
    }

    #[test]
    fn parse_ranking_page_extracts_numbers_in_order() {
        let html = r#"
            <div class="movie-list">
              <a href="/v/ABC-123"><img></a>
              <a href="/v/DEF-456/"><img></a>
              <a href="/actors/1">演员</a>
              <a href="/v/ABC-123"><img></a>
              <A HREF="/v/GHI-789?x=1"><img></A>
            </div>"#;
        assert_eq!(
            parse_ranking_page(html),
            vec!["ABC-123", "DEF-456", "GHI-789"]
        );
    }

    #[test]
    fn parse_ranking_page_ignores_non_movie_links() {
        let html = r#"<a href="/">首页</a><a href="/rankings">排行</a>"#;
        assert!(parse_ranking_page(html).is_empty());
    }

    #[test]
    fn board_url_for_censored_weekly() {
        let src = JavDbSource::new_for_test("https://javdb.com");
        let board = find_board("censored").unwrap();
        let url = src.board_url(board, "weekly").unwrap();
        assert!(url.as_str().contains("m=censored"));
        assert!(url.as_str().contains("p=weekly"));
    }

    #[test]
    fn board_url_rejects_bad_period() {
        let src = JavDbSource::new_for_test("https://javdb.com");
        let board = find_board("censored").unwrap();
        assert!(matches!(
            src.board_url(board, "yearly"),
            Err(FetchError::UnsupportedPeriod(_, _))
        ));
    }

    #[test]
    fn unknown_board_is_error() {
        assert!(find_board("nope").is_none());
    }

    #[tokio::test]
    async fn fetch_ranking_uses_mock_server() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/v/AAA-001"></a><a href="/v/BBB-002/"></a>"#),
            )
            .mount(&server)
            .await;
        let src = JavDbSource::new_for_test(&server.uri());
        let numbers = src.fetch_ranking("hot", "all").await.unwrap();
        assert_eq!(numbers, vec!["AAA-001", "BBB-002"]);
    }
}
