//! JavDB 元数据 provider —— 上游 `src/metadata/_providers/javdb.py`（975 行）
//! 里**本批需要的那三个方法**。
//!
//! # 为什么只做三个
//!
//! 上游那个类还带榜单（`get_rank_numbers` / `get_playback_rank_numbers`）、
//! 演员（`search_actors`）、系列（`search_series`）、 reviews、以及**登录态**
//! （`_ensure_logged_in` + 设备指纹 `_device_payload`）。本批只做：
//!
//! | 上游方法 | 行号 | 用途 |
//! |---|---|---|
//! | `_search_movie` | `:386` | 按番号搜到候选（`GET /api/v2/search`）|
//! | `get_movie_by_javdb_id` | `:416` | 取详情（`GET /api/v4/movies/{id}`）|
//! | `_normalize_image_url` | `:125` | 图片 URL 归一（纯函数）|
//!
//! 其余的**不是「漏了」，是还没接线**：见
//! `docs/handoff.md` §7.2f 的被阻塞端点清单。
//!
//! # ★ 返回**原始 JSON**，不做字段映射
//!
//! 上游的 `get_movie_by_javdb_id` 返回 `JavdbMovieDetailResource`（`_build_movie_detail`
//! 逐字段映射）。而本仓的 [`MetadataProvider`] 返回 `serde_json::Value` ——
//! `PluginDelivery.javdb_detail` 的文档写的是「provider 原文，宿主只搬运」。
//! 映射在 `catalog_import` 那一侧做，所以这里**不重复一遍**。
//!
//! # 不做登录态
//!
//! `_ensure_logged_in` 只在「需要登录的接口」上用（榜单的某些分区）。搜索与
//! 详情在未登录下可取，所以本批**不带 Cookie**。带上的话，「未登录也能用」
//! 这个性质会悄悄变成「登录态过期就开始报错」。
//!
//! # host 不带 scheme
//!
//! 上游 `_build_api_url` 拼的是 `https://{host}{path}`，而 `host` 来自配置
//! （`javdb.host`）。本仓照抄那个形状 —— 于是配置里写 `javdb.com` 而不是
//! `https://javdb.com`。**写错 scheme 的后果**：拼出 `https://https://…` 而
//! reqwest 报「无效 URL」，那是 502 而不是 404。

use serde_json::Value;

use crate::catalog::metadata_source::{MetadataProvider, MetadataSourceError};

/// 搜索接口（上游 `API_PATH_SEARCH`）。
pub const API_PATH_SEARCH: &str = "/api/v2/search";
/// 影片详情（上游 `API_PATH_MOVIE_DETAIL`）。
pub const API_PATH_MOVIE_DETAIL: &str = "/api/v4/movies/{javdb_id}";

/// 搜索的固定查询参数（上游 `API_PARAMS_MOVIE_SEARCH`）。
///
/// `limit=24` 是上游的值：**搜索结果按发行日期倒序后取第一个番号精确匹配**，
/// 而精确匹配通常落在前几条里。调大它只会让无关候选变多（而匹配逻辑是
/// 「番号归一后**完全相等**」，不是模糊）。
const MOVIE_SEARCH_PARAMS: [(&str, &str); 6] = [
    ("from_recent", "false"),
    ("type", "movie"),
    ("movie_type", "all"),
    ("movie_sort_by", "relevance"),
    ("movie_filter_by", "all"),
    ("page", "1"),
];

/// 图片 URL 归一（上游 `_normalize_image_url`，`:125-134`）。
///
/// JavDB 的 API 返回的图片路径是**相对片段**（`covers/abc.jpg`），要拼成
/// CDN 的完整 URL 才能下载。上游按 `covers` / `samples` / `avatars` 三个
/// 关键词分别拼，**都不含**就原样返回。
///
/// # 为什么不能用 `contains` 之外的判断
///
/// 上游是 `if "covers" in url`（**子串 anywhere**），不是 `startswith`。
/// 照抄子串判定：URL 里任意位置出现 `covers` 都会被重写 —— 那看起来像 bug，
/// 但改掉它就与上游对同一 URL 的处理不一致，而这种不一致会以「封面 404」
/// 的形式出现（某一类 URL 恰好只在一处不同）。
pub fn normalize_image_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    if url.is_empty() {
        return None;
    }
    for kind in ["covers", "samples", "avatars"] {
        if url.contains(kind) {
            let tail = url.rsplit(&format!("{kind}/")).next().unwrap_or(url);
            return Some(format!("https://c0.jdbstatic.com/{kind}/{tail}"));
        }
    }
    Some(url.to_owned())
}

/// JavDB provider。
#[derive(Debug, Clone)]
pub struct JavdbProvider {
    client: reqwest::Client,
    /// 拼 URL 用的前缀，**末尾无斜杠**。生产是 `https://{host}`。
    base: String,
}

impl JavdbProvider {
    /// 构造。`host` **不带** `https://`（上游形状）。
    ///
    /// 客户端刻意用 `no_proxy()`：JavDB 是**直连公网**的目标，走环境里的
    /// HTTP 代理会让「能不能搜」取决于代理配置 —— 那是一个与本服务无关的
    /// 故障源。同样的理由见 `TorznabClient`。
    pub fn new(host: &str) -> Result<Self, MetadataSourceError> {
        let host = host.trim().trim_end_matches('/');
        if host.is_empty() {
            return Err(MetadataSourceError::RequestFailed(
                "javdb.host 为空".to_owned(),
            ));
        }
        Self::with_base_url(&format!("https://{host}"))
    }

    /// 直接给 base URL（**带 scheme**）。测试打桩与「走反向代理」用。
    ///
    /// # 为什么需要这个缝
    ///
    /// 上游把 `https://` 写死在 `_build_api_url` 里，于是「本机起一个假 JavDB」
    /// 不可能 —— 而那正是唯一能覆盖「候选里挑番号**完全相等**的那个」与
    /// 「`success != 1` 不是 404」这两条实现级断言的办法。它也是**代理**场景
    /// 唯一说得通的入口：把 base 指到内网网关，其余代码一行不改。
    pub fn with_base_url(base: &str) -> Result<Self, MetadataSourceError> {
        let base = base.trim().trim_end_matches('/').to_owned();
        if base.is_empty() {
            return Err(MetadataSourceError::RequestFailed(
                "JavDB base URL 为空".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| {
                MetadataSourceError::RequestFailed(format!("构造 JavDB 客户端失败：{error}"))
            })?;
        Ok(Self { client, base })
    }

    /// 拼完整 URL（上游 `_build_api_url`）。
    ///
    /// 查询参数的顺序**照抄上游的字典序**（`q` 在最前，其余按
    /// `API_PARAMS_MOVIE_SEARCH` 的声明序）。请求 URL 会被 JavDB 记进日志，
    /// 顺序不一致会让「同一请求」在两边对不上，而排查时那是最先要排除的
    /// 变量。
    fn api_url(&self, path: &str, query: &[(&str, String)]) -> String {
        let base = format!("{}{}", self.base, path);
        if query.is_empty() {
            return base;
        }
        let encoded: Vec<String> = query
            .iter()
            .map(|(key, value)| format!("{key}={}", encode_component(value)))
            .collect();
        format!("{base}?{}", encoded.join("&"))
    }

    /// 发一次 GET 并解析 JSON。上游 `request_json`。
    async fn request_json(&self, url: &str) -> Result<Value, MetadataSourceError> {
        let response = self.client.get(url).send().await.map_err(|error| {
            MetadataSourceError::RequestFailed(format!("请求 JavDB 失败 {url}: {error}"))
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            MetadataSourceError::RequestFailed(format!("读 JavDB 响应失败 {url}: {error}"))
        })?;
        if !status.is_success() {
            return Err(MetadataSourceError::RequestFailed(format!(
                "JavDB 返回 {status}：{}",
                truncate(&body)
            )));
        }
        serde_json::from_str(&body).map_err(|error| {
            MetadataSourceError::RequestFailed(format!("JavDB 响应不是 JSON：{error}"))
        })
    }

    /// 按番号搜候选（上游 `_search_movie`，`:386-414`）。
    ///
    /// 候选按 `release_date` **倒序**后，取第一个「番号归一后完全相等」的。
    /// 精确相等而不是包含：JavDB 的模糊搜索会把 `ABC-123` 与 `ABC-1234`、
    /// `ABC-123B` 混在一起，而导入要的是**同一部片**。
    async fn search_movie(&self, movie_number: &str) -> Result<Value, MetadataSourceError> {
        let normalized = crate::movie_numbers::normalize_movie_number(movie_number);
        let mut query: Vec<(&str, String)> = vec![("q", normalized.clone())];
        for (key, value) in MOVIE_SEARCH_PARAMS {
            query.push((key, value.to_owned()));
        }
        let url = self.api_url(API_PATH_SEARCH, &query);
        let payload = self.request_json(&url).await?;

        let mut candidates: Vec<&Value> = payload
            .get("data")
            .and_then(|data| data.get("movies"))
            .and_then(Value::as_array)
            .map(|movies| movies.iter().collect())
            .unwrap_or_default();
        if candidates.is_empty() {
            return Err(MetadataSourceError::NotFound);
        }
        // `release_date` 缺失当空串 —— 与上游 `m.get("release_date") or ""` 一致，
        // 且排序必须是**稳定**的：同日期的候选要保持 API 返回的次序。
        candidates.sort_by(|left, right| release_date(right).cmp(release_date(left)));
        candidates
            .into_iter()
            .find(|movie| {
                let number = movie
                    .get("number")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                crate::movie_numbers::normalize_movie_number(number) == normalized
            })
            .cloned()
            .ok_or(MetadataSourceError::NotFound)
    }

    /// 取详情 payload（上游 `_get_movie_detail_payload`，`:699-`）。
    async fn movie_detail_payload(&self, javdb_id: &str) -> Result<Value, MetadataSourceError> {
        let path = API_PATH_MOVIE_DETAIL.replace("{javdb_id}", javdb_id);
        let url = self.api_url(&path, &[("from_rankings", "true".to_owned())]);
        let payload = self.request_json(&url).await?;
        // `success != 1` 是 JavDB 的「业务失败」标志（HTTP 仍是 200）。
        // 不看它的话，一个 `{"success":0,"message":"..."}` 会被当成「没有这部片」
        // → 404，而真实原因是服务端拒绝了请求。
        if payload.get("success").and_then(Value::as_i64) != Some(1) {
            let detail = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unexpected success")
                .to_owned();
            return Err(MetadataSourceError::RequestFailed(format!(
                "JavDB 详情返回失败：{detail}"
            )));
        }
        payload
            .get("data")
            .and_then(|data| data.get("movie"))
            .filter(|movie| !movie.is_null())
            .cloned()
            .ok_or(MetadataSourceError::NotFound)
    }
}

#[tonic::async_trait]
impl MetadataProvider for JavdbProvider {
    /// 上游 `get_movie_by_number`（`:430-439`）：先搜到 id，再取详情。
    async fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<Value>, MetadataSourceError> {
        let movie = self.search_movie(movie_number).await?;
        let javdb_id = movie
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(MetadataSourceError::NotFound)?;
        self.movie_detail_payload(javdb_id).await.map(Some)
    }

    /// 上游 `get_movie_by_javdb_id`（`:416-428`）。
    async fn get_movie_by_javdb_id(
        &self,
        javdb_id: &str,
    ) -> Result<Option<Value>, MetadataSourceError> {
        self.movie_detail_payload(javdb_id).await.map(Some)
    }

    /// ★ **本批未实现**，显式报错而不是返回空列表。
    ///
    /// 上游 `search_actors`（`:326-372`）要另一个 API 形状（`type=actor`）。
    /// 返回空列表会让调用方（演员 SSE，`metadata_source.rs:478`）报
    /// 「导入 0 个」—— 那是**谎报**：用户看到「没搜到」而不是「搜不了」。
    async fn search_actors(&self, _keyword: &str) -> Result<Vec<Value>, MetadataSourceError> {
        Err(MetadataSourceError::RequestFailed(
            "JavDB 演员搜索尚未移植（见 docs/handoff.md §7.2f）".to_owned(),
        ))
    }
}

/// 出网的 UA。上游走官方 App 的接口，带一个浏览器 UA 即可（不带登录态）。
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0 Safari/537.36";

/// 候选的 `release_date`（缺失当空串，与上游 `or ""` 一致）。
fn release_date(movie: &Value) -> &str {
    movie
        .get("release_date")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// 查询参数的百分号编码（上游 `urlencode(..., safe=':-')`）。
///
/// 保留 `:` 与 `-` 是为了番号 `ABC-123` 与带冒号的值不被编码成 `%3A` ——
/// JavDB 按字面量匹配，写 `%3A` 会搜不到。
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 错误消息里的响应体截断（上游日志只打摘要）。
fn truncate(body: &str) -> String {
    const LIMIT: usize = 200;
    if body.chars().count() <= LIMIT {
        return body.to_owned();
    }
    let head: String = body.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_urls_are_rewritten_to_the_cdn() {
        assert_eq!(
            normalize_image_url(Some("covers/abc.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/covers/abc.jpg")
        );
        assert_eq!(
            normalize_image_url(Some("pics/samples/x.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/samples/x.jpg")
        );
        assert_eq!(
            normalize_image_url(Some("avatars/y.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/avatars/y.jpg")
        );
        // 不含那三个关键词的**原样返回**（上游 `:134`）。
        assert_eq!(
            normalize_image_url(Some("https://other.example/z.jpg")).as_deref(),
            Some("https://other.example/z.jpg")
        );
        // 空 / 缺值 → None，而不是 `Some("")`。
        assert_eq!(normalize_image_url(None), None);
        assert_eq!(normalize_image_url(Some("")), None);
    }

    #[test]
    fn the_url_is_https_plus_host_plus_path() {
        let provider = JavdbProvider::new("javdb.com").expect("构造");
        assert_eq!(
            provider.api_url("/api/v2/search", &[]),
            "https://javdb.com/api/v2/search"
        );
        // 末尾斜杠被去掉（`new` 里 trim）—— 否则会拼出 `//api/...`。
        let trimmed = JavdbProvider::new("javdb.com/").expect("构造");
        assert_eq!(
            trimmed.api_url("/x", &[]),
            "https://javdb.com/x",
            "host 末尾的斜杠要归一"
        );
    }

    #[test]
    fn movie_numbers_keep_their_colons_and_dashes() {
        // JavDB 按字面量匹配番号：编码成 %3A / %2D 会搜不到。
        assert_eq!(encode_component("ABC-123"), "ABC-123");
        assert_eq!(encode_component("A:B"), "A:B");
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn the_search_query_carries_the_upstream_parameters() {
        let provider = JavdbProvider::new("javdb.com").expect("构造");
        let query: Vec<(&str, String)> = vec![
            ("q", "ABC-123".to_owned()),
            ("from_recent", "false".to_owned()),
            ("type", "movie".to_owned()),
            ("movie_type", "all".to_owned()),
            ("movie_sort_by", "relevance".to_owned()),
            ("movie_filter_by", "all".to_owned()),
            ("page", "1".to_owned()),
        ];
        let url = provider.api_url(API_PATH_SEARCH, &query);
        for expected in [
            "q=ABC-123",
            "from_recent=false",
            "type=movie",
            "movie_type=all",
            "movie_sort_by=relevance",
            "movie_filter_by=all",
            "page=1",
        ] {
            assert!(url.contains(expected), "{url} 缺 {expected}");
        }
    }

    #[test]
    fn the_detail_path_substitutes_the_javdb_id() {
        assert_eq!(
            API_PATH_MOVIE_DETAIL.replace("{javdb_id}", "A123"),
            "/api/v4/movies/A123"
        );
    }

    #[test]
    fn an_empty_host_is_refused_rather_than_producing_a_broken_url() {
        let error = JavdbProvider::new("   ").expect_err("空 host 该拒");
        assert!(matches!(error, MetadataSourceError::RequestFailed(_)));
    }

    #[test]
    fn long_response_bodies_are_truncated_in_errors() {
        let long = "x".repeat(500);
        let short = truncate(&long);
        assert!(short.chars().count() <= 201, "不该把整个响应体塞进错误消息");
        assert!(short.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }
}
