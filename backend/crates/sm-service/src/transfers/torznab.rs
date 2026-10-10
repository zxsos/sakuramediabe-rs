//! Torznab 客户端，对应上游
//! `src/service/transfers/downloads/clients/torznab.py`（296 行）。
//!
//! # 两件事：连通性计数 + 搜索候选
//!
//! | 用途 | 入口 | 上游调用方 |
//! |---|---|---|
//! | 数命中条数（不建候选） | [`TorznabClient::search`] | `indexer_settings.test_connection` |
//! | 完整候选（提交下载用） | 同上，返回 [`TorznabCandidate`] | `downloads.search_service` |
//!
//! # 四处与上游一致、但容易写错的地方
//!
//! 1. **`apikey` 只在索引器自己有 key 时才带**。空 key 不是「带一个空参数」，
//!    而是「不带参数」—— 免鉴权的 Torznab 端点靠这条兼容。
//! 2. **不带绑定下载器的索引器直接跳过**。它的候选无法提交，搜出来也没用；
//!    但**不报错**（上游只记一条 warning）。
//! 3. **失败信息里不能出现 URL 与凭据**。Python 侧专门写了
//!    `_describe_search_error` 来剥掉 httpx 内嵌的完整 URL（含 apikey）——
//!    那个字符串会进日志、也会进 `ApiError.details`。这里照做。
//! 4. **`source_uri` 只认 `torznab:attr[magneturl]` / `<link>` / `<guid>`**，
//!    取**第一个非磁力链接**，都没有才回退到磁力链。`<enclosure>` 上游**不读**
//!    —— 虽然它是 Torznab 的常见写法，但改动它会改变既有的候选解析口径。
//!
//! # 番号归一是共用件
//!
//! [`build_search_query`] 用到的 [`normalize_movie_number`] 住在
//! [`crate::movie_numbers`]（上游 `src/common/movie_numbers.py`），
//! 与影片导入、候选标题过滤共用同一份。

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use regex::Regex;
use sm_db::repo::{IndexerDownloadClientRepository, IndexerRepository};
use sm_db::transfers::Indexer;
use sm_db::Db;

use crate::movie_numbers::normalize_movie_number;

/// Torznab 搜索失败。消息**不含** URL 与 apikey（见模块文档第 3 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorznabError(String);

impl TorznabError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TorznabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TorznabError {}

/// 解析出的一条原始条目（**未清洗**，字段与上游 `xmltodict` 的口径一致）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TorznabItem {
    pub title: String,
    pub description: String,
    /// `<link>`。
    pub link: String,
    /// `<guid>`。
    pub guid: String,
    /// `<torznab:attr name="magneturl" value="…">`。
    pub magnet_url: String,
    /// `<size>` 元素。**不读 `<enclosure length>`**（与上游一致）。
    pub size_bytes: i64,
    /// `<torznab:attr name="seeders">`。
    pub seeders: i32,
}

/// 候选可选的下载器概要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundClient {
    pub id: i32,
    pub name: String,
}

/// 一条搜索候选。字段对应上游 `DownloadCandidateResource`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorznabCandidate {
    pub source_uri: String,
    pub indexer_name: String,
    pub indexer_kind: String,
    /// 解析出的**默认**下载器 —— 绑定顺序的第一个。
    pub resolved_client_id: i32,
    pub resolved_client_name: String,
    /// 可选的下载器，**按绑定顺序**。
    pub download_clients: Vec<BoundClient>,
    pub movie_number: String,
    pub title: String,
    pub size_bytes: i64,
    pub seeders: i32,
}

/// Torznab 检索词。对应上游 `_build_search_query`。
///
/// FC2 资源在聚合器里通常按**纯数字**检索命中率更高，所以 `FC2-1234567`
/// 会被改写成 `1234567`；其余番号原样返回（注意返回的是**原始输入**，
/// 不是归一后的值）。
pub fn build_search_query(movie_number: &str) -> String {
    let normalized = normalize_movie_number(movie_number);
    if normalized.starts_with("FC2") {
        if let Some(digits) = fc2_digits(&normalized) {
            return digits;
        }
    }
    movie_number.to_owned()
}

/// `^FC2-?(\d+)$` 的捕获组。
fn fc2_digits(value: &str) -> Option<String> {
    let rest = value.strip_prefix("FC2")?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    if rest.is_empty() || !rest.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(rest.to_owned())
}

/// Torznab 客户端。
#[derive(Debug, Clone)]
pub struct TorznabClient {
    http: reqwest::Client,
}

impl Default for TorznabClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TorznabClient {
    /// 默认客户端：30s 超时，**不读环境代理**。
    ///
    /// `no_proxy()` 对应上游 httpx 的 `trust_env=False` —— 否则容器里一个
    /// `HTTP_PROXY` 就会把 indexer 请求导到别处，而错误表现为「超时」。
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
    }

    /// 注入自定义 HTTP 客户端（测试用）。
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// 跨全部配置好的索引器搜一次，返回**候选**。
    ///
    /// - `indexer_kind`：只搜该类型（`pt` / `bt`）；`None` 表示全部。
    /// - `continue_on_error`：
    ///   - `false`（`test_connection` 的口径）—— 第一个失败即整体失败；
    ///   - `true`（搜索端点的口径）—— 逐个容错，但**所有可搜索的索引器都失败**
    ///     时仍报错，避免把「服务不可用」误报成「没有搜索结果」。
    pub async fn search(
        &self,
        db: &Db,
        movie_number: &str,
        indexer_kind: Option<&str>,
        continue_on_error: bool,
    ) -> Result<Vec<TorznabCandidate>, TorznabError> {
        let indexers = IndexerRepository::new(db.clone())
            .list_all()
            .await
            .map_err(|err| TorznabError::new(format!("读取索引器失败：{err}")))?;
        let bindings = IndexerDownloadClientRepository::new(db.clone())
            .list_all_with_clients()
            .await
            .map_err(|err| TorznabError::new(format!("读取索引器绑定失败：{err}")))?;

        // 一趟 JOIN 的绑定行按 id 升序 → 每个索引器内部的顺序就是绑定顺序。
        let mut clients_by_indexer: HashMap<i32, Vec<BoundClient>> = HashMap::new();
        for (indexer_id, client_id, name) in bindings {
            clients_by_indexer
                .entry(indexer_id)
                .or_default()
                .push(BoundClient {
                    id: client_id,
                    name,
                });
        }

        let query = build_search_query(movie_number);
        let mut candidates: Vec<TorznabCandidate> = Vec::new();
        let mut searched = 0usize;
        let mut succeeded = 0usize;
        let mut last_failure: Option<TorznabError> = None;

        for indexer in &indexers {
            if let Some(kind) = indexer_kind {
                if indexer.kind != kind {
                    continue;
                }
            }
            // 无绑定下载器的索引器搜出来也无法提交，跳过（不报错）。
            let Some(clients) = clients_by_indexer.get(&indexer.id) else {
                continue;
            };
            searched += 1;

            let items = match self.fetch_indexer(indexer, &query).await {
                Ok(items) => {
                    succeeded += 1;
                    items
                }
                Err(err) if continue_on_error => {
                    tracing::warn!(
                        movie_number,
                        indexer = %indexer.name,
                        detail = %err,
                        "Torznab 搜索失败，跳过该索引器"
                    );
                    last_failure = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            };

            for item in items {
                candidates.push(build_candidate(movie_number, indexer, clients, &item));
            }
        }

        // 全部可搜索的索引器都失败 → 保留错误语义。
        if continue_on_error && searched > 0 && succeeded == 0 {
            return Err(last_failure.unwrap_or_else(|| TorznabError::new("所有索引器都失败了")));
        }

        // 上游按 `(seeders, size_bytes)` 降序排。
        candidates.sort_by_key(|item| std::cmp::Reverse((item.seeders, item.size_bytes)));
        Ok(candidates)
    }

    /// 向单个索引器发一次 `t=search` 请求并解析响应。
    async fn fetch_indexer(
        &self,
        indexer: &Indexer,
        query: &str,
    ) -> Result<Vec<TorznabItem>, TorznabError> {
        // 手拼 query 而不是 `RequestBuilder::query`：后者要开 reqwest 的
        // `serde` 系 feature，而 workspace 里只开了 `json/rustls/stream`。
        let mut url =
            reqwest::Url::parse(&indexer.url).map_err(|_| TorznabError::new("索引器 URL 非法"))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("t", "search");
            pairs.append_pair("q", query);
            pairs.append_pair("cat", "6000");
            // 空 key = 不带参数（免鉴权端点）。
            if let Some(key) = indexer
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
            {
                pairs.append_pair("apikey", key);
            }
        }

        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| TorznabError::new(describe_request_error(&err)))?;

        let status = response.status();
        if !status.is_success() {
            return Err(TorznabError::new(format!("HTTP {}", status.as_u16())));
        }

        let body = response
            .text()
            .await
            .map_err(|err| TorznabError::new(describe_request_error(&err)))?;
        parse_items(&body)
    }
}

/// 一条原始条目 → 候选。对应上游 `_build_candidate`。
fn build_candidate(
    movie_number: &str,
    indexer: &Indexer,
    clients: &[BoundClient],
    item: &TorznabItem,
) -> TorznabCandidate {
    let title = clean_candidate_text(&item.title);
    let description = clean_candidate_text(&item.description);
    let full_title = [title.as_str(), description.as_str()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");

    // 默认下载器 = 绑定顺序的第一个；`search` 已保证这里非空。
    let resolved = clients.first().cloned().unwrap_or(BoundClient {
        id: 0,
        name: String::new(),
    });

    TorznabCandidate {
        source_uri: resolve_source_uri(&item.magnet_url, &item.link, &item.guid),
        // 上游是 `indexer.name or 远端 id/name or channel title`；本仓库的
        // `indexer.name` 是 NOT NULL 且写入时校验非空，所以那串回退不可达。
        indexer_name: indexer.name.clone(),
        indexer_kind: indexer.kind.clone(),
        resolved_client_id: resolved.id,
        resolved_client_name: resolved.name,
        download_clients: clients.to_vec(),
        movie_number: movie_number.trim().to_owned(),
        title: full_title,
        size_bytes: item.size_bytes,
        seeders: item.seeders,
    }
}

/// 取第一个**非磁力**链接；都是磁力时取第一个磁力。
///
/// 对应上游 `_resolve_source_uri`。provider 收到的是一个不透明 URI，
/// 由它决定这是磁力、种子地址还是别的来源。
fn resolve_source_uri(magnet_url: &str, link: &str, guid: &str) -> String {
    let mut magnet = String::new();
    for raw in [magnet_url, link, guid] {
        let candidate = raw.trim();
        if candidate.is_empty() {
            continue;
        }
        if candidate.to_ascii_lowercase().starts_with("magnet:") {
            if magnet.is_empty() {
                magnet = candidate.to_owned();
            }
        } else {
            return candidate.to_owned();
        }
    }
    magnet
}

/// 去 HTML 标签并折叠空白。对应上游 `_clean_candidate_text`。
///
/// 上游用 `HTMLParser` 取文本节点、在标签边界插空格再折叠。这里用正则剥标签
/// 达到同一效果：**输入已经过 XML 解码**（`&amp;` 早被解析成 `&`），所以不需要
/// 再做实体解码；标签一律替换成空格，与 `HTMLParser` 在起止标签各插一个空格
/// 折叠后同形。
fn clean_candidate_text(value: &str) -> String {
    static TAGS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<[^>]*>").expect("手写正则应编译通过"));
    let without_tags = TAGS.replace_all(value, " ");
    without_tags
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 把请求异常压成**不含 URL 与凭据**的短描述。
///
/// `reqwest::Error` 的 `Display` 会把完整 URL（含 `apikey` 查询参数）带出来 ——
/// 那既会进日志，也会进 `ApiError.details`。所以这里只取类型 + 去掉 query 的地址。
fn describe_request_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect error"
    } else if err.is_request() {
        "request error"
    } else {
        "transport error"
    };
    match err.url() {
        Some(url) => {
            let mut stripped = url.clone();
            stripped.set_query(None);
            format!("{kind} url={stripped}")
        }
        None => kind.to_owned(),
    }
}

/// 解析 Torznab 的 `rss > channel > item`。
///
/// **只数 `item`**：连通性测试要的是条数，而 indexer 之间字段写法差异极大，
/// 把每种都当必需会让「能搜到」误报成「解析失败」。
pub fn parse_items(xml: &str) -> Result<Vec<TorznabItem>, TorznabError> {
    let mut reader = Reader::from_str(xml);
    // **不**开 `trim_text`：0.42 会把文本按实体引用切块，逐块 trim 会把
    // 实体两侧的空格吃掉（`a &lt; b` 变成 `a<b`）。只在字段收尾时 trim 一次。

    let mut buf = Vec::new();
    let mut items: Vec<TorznabItem> = Vec::new();
    let mut current = TorznabItem::default();
    let mut in_item = false;
    // 当前正在收集文本的元素（`title` / `description` / `size` / `link` / `guid`）。
    let mut text_field: Option<String> = None;
    // 该元素的文本缓冲。**不能直接赋值**：0.42 的 reader 不合并同一元素里的
    // 多段文本（一个实体引用就会把文本切成多块），赋值会只剩最后一块。
    let mut text_buffer = String::new();
    // 未闭合标签的深度。`check_end_names` 只校验**成对**标签名字是否一致，
    // 对「EOF 时仍未闭合」不报错 —— 而那正是「响应被截断」的形状，必须自己拦。
    let mut depth: i64 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(event)) => {
                depth += 1;
                let name = local_name(event.name().as_ref());
                match name.as_str() {
                    "item" => {
                        in_item = true;
                        current = TorznabItem::default();
                    }
                    "attr" if in_item => apply_attributes(&name, &event, &mut current),
                    "title" | "description" | "size" | "link" | "guid" if in_item => {
                        text_field = Some(name);
                        text_buffer.clear();
                    }
                    _ => {}
                }
            }
            // RSS 里 `<torznab:attr .../>` 几乎总是自闭合。
            Ok(Event::Empty(event)) => {
                if in_item && local_name(event.name().as_ref()) == "attr" {
                    apply_attributes("attr", &event, &mut current);
                }
            }
            Ok(Event::Text(text)) => {
                if in_item && text_field.is_some() {
                    // 文本块内不再含实体（它们是独立的 `GeneralRef` 事件）。
                    text_buffer.push_str(&text.into_inner());
                }
            }
            // 实体引用是**独立事件**：`&lt;b&gt;` 会给出 GeneralRef("lt") →
            // Text("b") → GeneralRef("gt")。不处理就会把 `<` / `>` 丢掉。
            Ok(Event::GeneralRef(reference)) => {
                if in_item && text_field.is_some() {
                    let raw = format!("&{};", reference.into_inner());
                    match quick_xml::escape::unescape(&raw) {
                        Ok(decoded) => text_buffer.push_str(&decoded),
                        // 未知实体：原样保留，别让它把候选标题吞掉。
                        Err(_) => text_buffer.push_str(&raw),
                    }
                }
            }
            // CDATA 是字面文本，不再含实体。
            Ok(Event::CData(data)) => {
                if in_item && text_field.is_some() {
                    text_buffer.push_str(&data.into_inner());
                }
            }
            Ok(Event::End(event)) => {
                depth -= 1;
                let name = local_name(event.name().as_ref());
                if text_field.as_deref() == Some(name.as_str()) {
                    apply_text(&mut current, &name, text_buffer.trim());
                    text_buffer.clear();
                    text_field = None;
                }
                if name == "item" {
                    in_item = false;
                    items.push(std::mem::take(&mut current));
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(TorznabError::new(format!("Torznab XML 解析失败：{err}"))),
            _ => {}
        }
        buf.clear();
    }

    if depth != 0 {
        // 不报错的话，一个被截断的响应会表现为「健康的 0 条结果」——
        // 而 `test_connection` 正是靠 `result_count` 判断 indexer 是否可用。
        return Err(TorznabError::new("Torznab XML 未闭合：响应可能被截断"));
    }
    Ok(items)
}

/// 把收集好的文本落到条目字段上。
fn apply_text(item: &mut TorznabItem, field: &str, value: &str) {
    match field {
        "title" => item.title = value.to_owned(),
        "description" => item.description = value.to_owned(),
        "link" => item.link = value.to_owned(),
        "guid" => item.guid = value.to_owned(),
        "size" => {
            if let Ok(size) = value.parse::<i64>() {
                item.size_bytes = size;
            }
        }
        _ => {}
    }
}

/// `<torznab:attr name= value=>` 的属性。
fn apply_attributes(name: &str, event: &quick_xml::events::BytesStart<'_>, item: &mut TorznabItem) {
    let mut attr_name = String::new();
    let mut attr_value = String::new();
    for attribute in event.attributes().flatten() {
        let key = local_name(attribute.key.as_ref());
        // `unescape_value()` 已废弃，0.42 用 `normalized_value()`。
        let value = attribute
            .normalized_value(XmlVersion::Explicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_default();
        if name == "attr" {
            match key.as_str() {
                "name" => attr_name = value,
                "value" => attr_value = value,
                _ => {}
            }
        }
    }
    if name == "attr" {
        match attr_name.as_str() {
            "seeders" => {
                if let Ok(seeders) = attr_value.trim().parse::<i32>() {
                    item.seeders = seeders;
                }
            }
            "magneturl" => item.magnet_url = attr_value.trim().to_owned(),
            _ => {}
        }
    }
}

/// 取元素/属性的**本地名**（忽略命名空间前缀，如 `torznab:attr` → `attr`）。
///
/// 0.42 的解析器直接产出 `&str`（旧版本是 `&[u8]`），所以这里收 `&str`。
fn local_name(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两条命中的 Torznab 响应（含 `description` / `guid` / `magneturl`）。
    const TWO_HITS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>渠道名</title>
    <item>
      <title>SSNI-888 第一版</title>
      <description>&lt;b&gt;高清&lt;/b&gt;  无码</description>
      <link>https://indexer.example.com/download/1</link>
      <guid>https://indexer.example.com/details/1</guid>
      <size>1073741824</size>
      <torznab:attr name="seeders" value="12"/>
    </item>
    <item>
      <title>SSNI-888 第二版</title>
      <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:BBB"/>
      <torznab:attr name="seeders" value="3"/>
      <size>4096</size>
    </item>
  </channel>
</rss>"#;

    fn indexer() -> Indexer {
        Indexer {
            id: 7,
            name: "我的索引器".to_owned(),
            url: "https://indexer.example.com/api".to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn clients() -> Vec<BoundClient> {
        vec![
            BoundClient {
                id: 11,
                name: "qb".to_owned(),
            },
            BoundClient {
                id: 12,
                name: "tr".to_owned(),
            },
        ]
    }

    #[test]
    fn fc2_queries_are_reduced_to_digits() {
        assert_eq!(build_search_query("FC2-1234567"), "1234567");
        assert_eq!(build_search_query("fc2-ppv-1234567"), "1234567");
        // 其余番号原样返回（不是归一回来的值）。
        assert_eq!(build_search_query("SSNI-888"), "SSNI-888");
        assert_eq!(build_search_query("abc_123"), "abc_123");
    }

    #[test]
    fn parses_the_upstream_fields() {
        let items = parse_items(TWO_HITS).expect("应能解析");
        assert_eq!(items.len(), 2, "只数 item，channel 的 title 不算");

        assert_eq!(items[0].title, "SSNI-888 第一版");
        assert_eq!(items[0].description, "<b>高清</b>  无码");
        assert_eq!(items[0].link, "https://indexer.example.com/download/1");
        assert_eq!(items[0].guid, "https://indexer.example.com/details/1");
        assert_eq!(items[0].size_bytes, 1_073_741_824);
        assert_eq!(items[0].seeders, 12);
        assert_eq!(items[0].magnet_url, "");

        assert_eq!(items[1].magnet_url, "magnet:?xt=urn:btih:BBB");
        assert_eq!(items[1].size_bytes, 4096);
        assert_eq!(items[1].seeders, 3);
    }

    #[test]
    fn a_candidate_prefers_a_non_magnet_link_over_the_magnet() {
        // 上游：取第一个**非磁力**链接；都是磁力才回退。
        let items = parse_items(TWO_HITS).expect("应能解析");
        let first = build_candidate("SSNI-888", &indexer(), &clients(), &items[0]);
        assert_eq!(first.source_uri, "https://indexer.example.com/download/1");
        // 只有磁力时用它。
        let second = build_candidate("SSNI-888", &indexer(), &clients(), &items[1]);
        assert_eq!(second.source_uri, "magnet:?xt=urn:btih:BBB");
    }

    #[test]
    fn a_candidate_carries_the_binding_order_and_the_first_client() {
        let items = parse_items(TWO_HITS).expect("应能解析");
        let candidate = build_candidate("SSNI-888", &indexer(), &clients(), &items[0]);
        assert_eq!(candidate.resolved_client_id, 11, "默认 = 绑定顺序第一个");
        assert_eq!(candidate.resolved_client_name, "qb");
        assert_eq!(
            candidate
                .download_clients
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![11, 12],
            "顺序即绑定顺序"
        );
        assert_eq!(candidate.indexer_name, "我的索引器");
        assert_eq!(candidate.indexer_kind, "pt");
        assert_eq!(candidate.movie_number, "SSNI-888");
    }

    #[test]
    fn a_candidate_joins_cleaned_title_and_description() {
        let items = parse_items(TWO_HITS).expect("应能解析");
        let candidate = build_candidate("SSNI-888", &indexer(), &clients(), &items[0]);
        assert_eq!(
            candidate.title, "SSNI-888 第一版 高清 无码",
            "标题 = 清洗后的 title + description"
        );
    }

    #[test]
    fn cleaning_strips_tags_and_collapses_whitespace() {
        assert_eq!(clean_candidate_text("<b>高清</b>  无码"), "高清 无码");
        assert_eq!(clean_candidate_text("  前后空白  "), "前后空白");
        assert_eq!(clean_candidate_text("<br/><br/>"), "");
    }

    #[test]
    fn an_empty_channel_yields_no_items() {
        let xml = r#"<rss><channel><title>空</title></channel></rss>"#;
        assert!(parse_items(xml).expect("应能解析").is_empty());
    }

    #[test]
    fn a_channel_title_is_not_an_item() {
        // `rss > channel > title` 与 `item` 里的 `title` 同名，靠 `in_item` 区分。
        let xml = r#"<rss><channel><title>渠道名</title><item><title>条目名</title></item></channel></rss>"#;
        let items = parse_items(xml).expect("应能解析");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "条目名");
    }

    #[test]
    fn broken_xml_is_an_error_not_an_empty_list() {
        // 空列表会让「indexer 挂了」与「搜到 0 条」长得一样。
        assert!(parse_items("<rss><channel><item>").is_err());
    }
}
