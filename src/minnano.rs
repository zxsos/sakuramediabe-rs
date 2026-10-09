//! みんなのAV 排行榜 HTML 适配。
//!
//! # 上游对应：`sakuramedia_more_rank_movies/minnano.py`
//!
//! | 上游 | 这里 |
//! |---|----|
//! | `_RankingTableParser` | [`parse_ranking_entries`]（事件扫描） |
//! | `_extract_dmm_cid` | [`extract_dmm_cid`] |
//! | `_extract_minnano_detail_url` | [`extract_minnano_detail_url`] |
//! | `_ProductCodeParser` / `parse_minnano_product_code` | [`parse_product_code`] |
//! | `MinnanoAvClient.get_rank_numbers` | [`MinnanoAvClient::get_rank_numbers`] |
//! | `MINNANO_RANKING_URLS` | [`ranking_url`] |

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::html::{Event, events};

pub const MINNANO_AV: &str = "https://www.minnano-av.com";
pub const MINNANO_HOST: &str = "www.minnano-av.com";

/// 上游 `MINNANO_RANKING_URLS`：daily / weekly / monthly。
pub fn ranking_url(period: &str) -> Option<&'static str> {
    match period.trim().to_ascii_lowercase().as_str() {
        "daily" => Some("https://www.minnano-av.com/ranking_av.php?daily"),
        "weekly" => Some("https://www.minnano-av.com/ranking_av.php"),
        "monthly" => Some("https://www.minnano-av.com/ranking_av.php?monthly"),
        _ => None,
    }
}

/// 上游 `MINNANO_PERIODS`。
pub const PERIODS: [&str; 3] = ["daily", "weekly", "monthly"];

/// 榜单条目：DMM CID 只用于溯源，正式番号来自作品详情页（上游 `MinnanoRankingEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingEntry {
    pub detail_url: String,
    pub dmm_cid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinnanoError {
    /// 上游 `MinnanoRankingError`。
    Ranking(String),
    /// 上游 `MinnanoProductCodeMissingError`。
    ProductCodeMissing(String),
    /// 上游 `ValueError`（不支持的周期）。
    BadPeriod(String),
    /// HTTP 失败。
    Http(String),
}

impl std::fmt::Display for MinnanoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ranking(d) => write!(f, "Minnano AV 榜单错误: {d}"),
            Self::ProductCodeMissing(d) => write!(f, "Minnano AV 作品详情页没有正式番号: {d}"),
            Self::BadPeriod(d) => write!(f, "不支持的 Minnano AV 榜单周期: {d}"),
            Self::Http(d) => write!(f, "Minnano AV 请求失败: {d}"),
        }
    }
}

impl std::error::Error for MinnanoError {}

#[derive(Debug, Clone)]
struct Link {
    href: String,
    text: String,
    is_title: bool,
}

/// 解析榜单表格，提取条目（上游 `_parse_ranking_entries`）。
pub fn parse_ranking_entries(content: &str) -> Result<Vec<RankingEntry>, MinnanoError> {
    // —— 第一遍：按上游 `_RankingTableParser` 的状态机攒出行 ——
    let mut table_found = false;
    let mut table_depth: usize = 0;
    let mut rows: Vec<Vec<Link>> = Vec::new();
    let mut row: Option<Vec<Link>> = None;
    let mut title_depth: usize = 0;
    let mut anchor: Option<(String, String, bool)> = None; // href, text, is_title

    let finish_anchor = |row: &mut Option<Vec<Link>>, anchor: &mut Option<(String, String, bool)>| {
        if let (Some(r), Some((href, text, is_title))) = (row.as_mut(), anchor.take()) {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            r.push(Link {
                href,
                text,
                is_title,
            });
        }
    };

    for event in events(content) {
        match event {
            Event::Start(tag) => {
                if tag.is("table") {
                    let classes: HashSet<&str> =
                        tag.attr("class").unwrap_or("").split_whitespace().collect();
                    if table_depth == 0
                        && classes.contains("tbllist")
                        && classes.contains("av")
                    {
                        table_found = true;
                        table_depth = 1;
                    } else if table_depth > 0 {
                        table_depth += 1;
                    }
                    continue;
                }
                if table_depth == 0 {
                    continue;
                }
                if tag.is("tr") {
                    finish_anchor(&mut row, &mut anchor);
                    if let Some(r) = row.take() {
                        if !r.is_empty() {
                            rows.push(r);
                        }
                    }
                    row = Some(Vec::new());
                    continue;
                }
                if row.is_none() {
                    continue;
                }
                if tag.is("h4") && tag.has_class("ttl") {
                    title_depth += 1;
                }
                if tag.is("a") {
                    if let Some(href) = tag.attr("href") {
                        if anchor.is_none() {
                            anchor = Some((href.to_owned(), String::new(), title_depth > 0));
                        }
                    }
                }
            }
            Event::End(name) => {
                if table_depth == 0 {
                    continue;
                }
                if name.eq_ignore_ascii_case("a") && anchor.is_some() {
                    finish_anchor(&mut row, &mut anchor);
                    continue;
                }
                if name.eq_ignore_ascii_case("h4") && title_depth > 0 {
                    title_depth -= 1;
                    continue;
                }
                if name.eq_ignore_ascii_case("tr") {
                    finish_anchor(&mut row, &mut anchor);
                    if let Some(r) = row.take() {
                        if !r.is_empty() {
                            rows.push(r);
                        }
                    }
                    title_depth = 0;
                    continue;
                }
                if name.eq_ignore_ascii_case("table") {
                    finish_anchor(&mut row, &mut anchor);
                    if let Some(r) = row.take() {
                        if !r.is_empty() {
                            rows.push(r);
                        }
                    }
                    title_depth = 0;
                    table_depth = table_depth.saturating_sub(1);
                }
            }
            Event::Text(text) => {
                if let Some((_, buf, _)) = anchor.as_mut() {
                    buf.push_str(&text);
                }
            }
        }
    }

    if !table_found {
        return Err(MinnanoError::Ranking("榜单表格不存在".to_owned()));
    }

    // —— 第二遍：按上游逻辑从行里挑标题链接与「動画を見る」链接 ——
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for links in &rows {
        let title_link = links.iter().find(|l| l.is_title);
        let Some(title_link) = title_link else { continue };
        let video_link = links.iter().find(|l| l.text.contains("動画を見る"));
        let dmm_cid = video_link
            .map(|l| extract_dmm_cid(&l.href))
            .unwrap_or_default();
        let detail_url = extract_minnano_detail_url(&title_link.href);
        if !dmm_cid.is_empty() && !detail_url.is_empty() && seen.insert(dmm_cid.clone()) {
            entries.push(RankingEntry {
                detail_url,
                dmm_cid,
            });
        }
    }
    if entries.is_empty() {
        return Err(MinnanoError::Ranking("榜单没有可用 DMM 番号".to_owned()));
    }
    Ok(entries)
}

/// 从 Minnano 的 DMM 联盟链接或直接 DMM 链接中提取 CID（上游 `_extract_dmm_cid`）。
pub fn extract_dmm_cid(href: &str) -> String {
    let unescaped = html_unescape(href);
    let absolute = join_url(&format!("{MINNANO_AV}/"), &unescaped);
    let Ok(url) = url::Url::parse(&absolute) else {
        return String::new();
    };
    if url.scheme() != "https" {
        return String::new();
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host != "al.dmm.co.jp" && host != "www.dmm.co.jp" {
        return String::new();
    }
    // candidates: lurl 查询参数优先，其次整个 URL
    let mut candidates = Vec::new();
    for (k, v) in url.query_pairs() {
        if k == "lurl" {
            candidates.push(v.into_owned());
        }
    }
    candidates.push(html_unescape(&unescaped));
    let cid_re = Regex::new(r"(?:^|[/?&])cid=([^/?&#]+)").unwrap();
    let code_re = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{1,127}$").unwrap();
    for candidate in candidates {
        let decoded = urlencoding_decode(&html_unescape(&candidate));
        if let Some(cap) = cid_re.captures(&decoded) {
            let cid = cap[1].trim().to_owned();
            if code_re.is_match(&cid) {
                return cid;
            }
        }
    }
    String::new()
}

/// 提取 Minnano 作品详情页 URL（上游 `_extract_minnano_detail_url`）。
pub fn extract_minnano_detail_url(href: &str) -> String {
    let absolute = join_url(&format!("{MINNANO_AV}/"), &html_unescape(href));
    let Ok(url) = url::Url::parse(&absolute) else {
        return String::new();
    };
    if url.scheme() != "https" {
        return String::new();
    }
    if url.host_str().unwrap_or("").to_ascii_lowercase() != MINNANO_HOST {
        return String::new();
    }
    absolute
}

/// 从作品详情页的「品番」行提取正式番号（上游 `parse_minnano_product_code`）。
pub fn parse_product_code(content: &str) -> Result<String, MinnanoError> {
    let mut product_code = String::new();
    let mut row: Option<Vec<Vec<String>>> = None;
    let mut cell: Option<Vec<String>> = None;
    let mut data_code = String::new();
    let code_re = Regex::new(r"[A-Za-z0-9][A-Za-z0-9_-]{1,127}").unwrap();

    let mut finish_row = |row: &mut Option<Vec<Vec<String>>>,
                          cell: &mut Option<Vec<String>>,
                          data_code: &mut String| {
        if let Some(c) = cell.take() {
            if let Some(r) = row.as_mut() {
                r.push(c);
            }
        }
        if let Some(r) = row.take() {
            let cells: Vec<String> = r
                .iter()
                .map(|c| c.join(" ").split_whitespace().collect::<Vec<_>>().join(" "))
                .collect();
            if cells.first().map(String::as_str) == Some("品番") {
                let candidate = if !data_code.is_empty() {
                    data_code.clone()
                } else {
                    cells.get(1).cloned().unwrap_or_default()
                };
                if let Some(m) = code_re.find(&candidate) {
                    product_code = m.as_str().to_uppercase();
                }
            }
        }
        data_code.clear();
    };

    for event in events(content) {
        match event {
            Event::Start(tag) => {
                if tag.is("tr") {
                    finish_row(&mut row, &mut cell, &mut data_code);
                    row = Some(Vec::new());
                    data_code.clear();
                } else if tag.is("td") && row.is_some() {
                    cell = Some(Vec::new());
                } else if tag.is("span") && cell.is_some() && tag.has_class("product-code-copy") {
                    data_code = tag.attr("data-code").unwrap_or("").to_owned();
                }
            }
            Event::End(name) => {
                if name.eq_ignore_ascii_case("td") && row.is_some() && cell.is_some() {
                    let c = cell.take().unwrap();
                    row.as_mut().unwrap().push(c);
                } else if name.eq_ignore_ascii_case("tr") {
                    finish_row(&mut row, &mut cell, &mut data_code);
                }
            }
            Event::Text(text) => {
                if let Some(c) = cell.as_mut() {
                    c.push(text.into_owned());
                }
            }
        }
    }

    if product_code.is_empty() {
        return Err(MinnanoError::ProductCodeMissing(
            "作品详情页没有可用正式番号".to_owned(),
        ));
    }
    Ok(product_code)
}

/// Minnano AV 榜单客户端（上游 `MinnanoAvClient`）。
pub struct MinnanoAvClient {
    client: reqwest::Client,
    request_interval: Duration,
    last_request_at: Option<Instant>,
    product_code_cache: HashMap<String, String>,
}

impl MinnanoAvClient {
    pub fn new(timeout: Duration, request_interval: Duration) -> Result<Self, MinnanoError> {
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
                    "ja-JP,ja;q=0.9,en;q=0.7".parse().unwrap(),
                );
                h.insert(
                    reqwest::header::REFERER,
                    format!("{MINNANO_AV}/").parse().unwrap(),
                );
                h
            })
            .build()
            .map_err(|e| MinnanoError::Http(format!("构建 HTTP 客户端失败: {e}")))?;
        Ok(Self {
            client,
            request_interval,
            last_request_at: None,
            product_code_cache: HashMap::new(),
        })
    }

    async fn request_page(&mut self, url: &str, page_name: &str) -> Result<String, MinnanoError> {
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
            .map_err(|e| MinnanoError::Http(format!("{page_name}请求失败: {e}")))?;
        self.last_request_at = Some(Instant::now());
        if response.status() != reqwest::StatusCode::OK {
            return Err(MinnanoError::Http(format!(
                "{page_name}返回 HTTP {}",
                response.status()
            )));
        }
        // 上游校验「发生未知跳转」：最终 host 必须是 minnano
        let final_url = response.url().clone();
        if final_url.host_str().unwrap_or("").to_ascii_lowercase() != MINNANO_HOST {
            return Err(MinnanoError::Ranking(format!("{page_name}发生未知跳转")));
        }
        response
            .text()
            .await
            .map_err(|e| MinnanoError::Http(format!("{page_name}读取正文失败: {e}")))
    }

    async fn product_code(&mut self, detail_url: &str) -> Result<String, MinnanoError> {
        if let Some(cached) = self.product_code_cache.get(detail_url) {
            return Ok(cached.clone());
        }
        let html = self.request_page(detail_url, "作品详情页").await?;
        let code = parse_product_code(&html)?;
        self.product_code_cache
            .insert(detail_url.to_owned(), code.clone());
        Ok(code)
    }

    /// 读取某周期榜单的正式番号（上游 `get_rank_numbers`）。
    pub async fn get_rank_numbers(&mut self, period: &str) -> Result<Vec<String>, MinnanoError> {
        let url = ranking_url(period)
            .ok_or_else(|| MinnanoError::BadPeriod(period.to_owned()))?;
        let html = self.request_page(url, "榜单").await?;
        let entries = parse_ranking_entries(&html)?;
        let mut numbers = Vec::new();
        let mut seen = HashSet::new();
        for entry in entries {
            match self.product_code(&entry.detail_url).await {
                Ok(code) => {
                    if seen.insert(code.clone()) {
                        numbers.push(code);
                    }
                }
                Err(MinnanoError::ProductCodeMissing(_)) => {
                    // 上游 warn 后跳过
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        if numbers.is_empty() {
            return Err(MinnanoError::Ranking("榜单没有可用正式番号".to_owned()));
        }
        Ok(numbers)
    }
}

/// 简单的 HTML 反转义（`html.unescape` 的子集：够 `href` 用）。
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// 简单的 URL join（`urllib.parse.urljoin` 的子集）。
fn join_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }
    let base = base.trim_end_matches('/');
    if let Some(path) = href.strip_prefix('/') {
        // 取 base 的 scheme+host
        if let Ok(url) = url::Url::parse(base) {
            if let Some(host) = url.host_str() {
                return format!("{}://{}/{}", url.scheme(), host, path.trim_start_matches('/'));
            }
        }
        return format!("{base}/{path}");
    }
    format!("{base}/{href}")
}

/// 百分号解码（`urllib.parse.unquote` 的子集）。
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                hex_val(bytes[i + 1]),
                hex_val(bytes[i + 2]),
            ) {
                out.push((h << 4 | l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANKING_HTML: &str = r#"
<html><body>
<table class="tbllist av">
<tr>
  <td><h4 class="ttl"><a href="/av/abc123/">作品标题一</a></h4></td>
  <td><a href="https://al.dmm.co.jp/?lurl=https%3A%2F%2Fwww.dmm.co.jp%2Fdigital%2Fvideoa%2F-%2Fdetail%2F%3D%2Fcid%3Dssis001%2F">動画を見る</a></td>
</tr>
<tr>
  <td><h4 class="ttl"><a href="/av/def456/">作品标题二</a></h4></td>
  <td><a href="https://al.dmm.co.jp/?lurl=https%3A%2F%2Fwww.dmm.co.jp%2Fdigital%2Fvideoa%2F-%2Fdetail%2F%3D%2Fcid%3Dssis002%2F">動画を見る</a></td>
</tr>
</table>
</body></html>"#;

    #[test]
    fn parse_ranking_entries_ok() {
        let entries = parse_ranking_entries(RANKING_HTML).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].dmm_cid, "ssis001");
        assert_eq!(
            entries[0].detail_url,
            "https://www.minnano-av.com/av/abc123/"
        );
    }

    #[test]
    fn parse_ranking_no_table() {
        assert!(matches!(
            parse_ranking_entries("<html></html>"),
            Err(MinnanoError::Ranking(_))
        ));
    }

    #[test]
    fn ranking_url_periods() {
        assert!(ranking_url("daily").is_some());
        assert!(ranking_url("weekly").is_some());
        assert!(ranking_url("monthly").is_some());
        assert!(ranking_url("yearly").is_none());
    }

    const DETAIL_HTML: &str = r#"
<html><body><table>
<tr><td>品番</td><td><span class="product-code-copy" data-code="SSIS-001">SSIS-001</span></td></tr>
<tr><td>発売日</td><td>2026-01-01</td></tr>
</table></body></html>"#;

    #[test]
    fn parse_product_code_ok() {
        assert_eq!(parse_product_code(DETAIL_HTML).unwrap(), "SSIS-001");
    }

    #[test]
    fn parse_product_code_missing() {
        assert!(matches!(
            parse_product_code("<html><body>nope</body></html>"),
            Err(MinnanoError::ProductCodeMissing(_))
        ));
    }

    #[test]
    fn extract_dmm_cid_non_dmm() {
        assert_eq!(extract_dmm_cid("https://example.com/?cid=x"), "");
    }
}
