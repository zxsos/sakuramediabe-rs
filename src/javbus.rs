//! JavBus 详情页解析与元数据交付（只按番号查询，不保存登录态）。
//!
//! # 上游对应：`javbus.py`
//!
//! 逐条照搬：`_PageParser` / `parse_movie_page` / `_clean_title` / `_compact` /
//! `_suffix` / `JavBusMetadataSource.fetch_movie` / `_download_image`。
//!
//! # 三处与上游不同（任务书第四节，展开见各自的位置）
//!
//! 1. **图片落点是请求给的 `delivery_dir`**，不是插件自己的
//!    `<data_dir>/metadata-tmp/`：proto 写在 `FetchMovieRequest.delivery_dir`
//!    上的原话是「元数据图片必须落在其中」，而宿主还要再验「再深一层」
//!    （`sm_plugins::movie_delivery`）。见 [`JavBusSource::fetch_movie`]。
//! 2. **「没收录」是 `Ok(None)`**，不是 `Err`：gRPC 里它是 `found = false` 的
//!    正常响应，`Err` 只表示「调用失败」—— 混起来会让宿主的兜底链路在第一
//!    个来源就停下。见 [`JavBusSource::fetch_movie`] 的返回值。
//! 3. **配置从宿主写的文件读**，不是 `context.settings`。见
//!    [`crate::settings`]。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::NaiveDate;
use reqwest::header::{COOKIE, LOCATION, REFERER, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode, Url};
use sm_plugin_api::v1::{FetchMovieResponse, MetadataActor};
use uuid::Uuid;

use crate::html::{self, Event, Tag};
use crate::settings::Settings;

/// 上游 `_HEADERS` 里的 UA（JavBus 对空 UA 直接拒）。
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// 上游 `Cookie: dv=1` —— 站点的「已成年」开关。
const COOKIE_VALUE: &str = "dv=1";

/// 人机验证的落点路径与页面标记（上游 `_requires_verification`）。
const VERIFICATION_PATH: &str = "/doc/driver-verify";
const VERIFICATION_MARKER: &str = "Age Verification JavBus";

/// 图片下载允许跟随的重定向次数。
const MAX_REDIRECTS: usize = 5;

/// 上游 `_IMAGE_SUFFIXES`：认不出后缀就当 `.jpg`。
const IMAGE_SUFFIXES: [&str; 5] = [".jpg", ".jpeg", ".png", ".webp", ".gif"];

/// 详情页信息表里的键（页面上的中文标签）。
const KEY_MOVIE_NUMBER: &str = "識別碼";
const KEY_RELEASE_DATE: &str = "發行日期";
const KEY_DURATION: &str = "長度";
const KEY_MAKER: &str = "製作商";
const KEY_DIRECTOR: &str = "導演";
const KEY_SERIES: &str = "系列";

/// 時长的单位（上游 `_DURATION = r"(\d+)\s*分鐘"`）。
const DURATION_UNIT: &str = "分鐘";

/// 一页详情页里能读出来的东西。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedMovie {
    pub movie_number: String,
    pub title: String,
    /// 严格 `YYYY-MM-DD`（proto 写在 `FetchMovieResponse.release_date` 上）。
    pub release_date: String,
    pub duration_minutes: i32,
    pub maker_name: Option<String>,
    pub director_name: Option<String>,
    pub series_name: Option<String>,
    pub actors: Vec<String>,
    pub tags: Vec<String>,
    /// 封面的原始 `href`（相对或绝对，落盘前要按基址解析）。
    pub cover_href: String,
    pub plot_hrefs: Vec<String>,
}

/// 页面拿到了，但不是一部能用的片子（缺少必填字段）。
///
/// 上游 `JavBusPageError`。与「没收录」（`Ok(None)`）是两回事：这里是**站点
/// 给了页面但页面不对**，值得让宿主记一条失败，而不是静默去试下一个来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// 有标题但没 `識別碼`。
    MissingMovieNumber,
    /// 没有 `a.bigImage`（上游：封面是必填）。
    MissingCover { movie_number: String },
    /// 发行日期或时长缺失 / 非法（上游同一条消息）。
    MissingDateOrDuration { movie_number: String },
}

impl PageError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingMovieNumber => "javbus_page_missing_movie_number",
            Self::MissingCover { .. } => "javbus_page_missing_cover",
            Self::MissingDateOrDuration { .. } => "javbus_page_missing_date_or_duration",
        }
    }
}

impl std::error::Error for PageError {}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMovieNumber => write!(f, "JavBus 页面缺少識別碼"),
            Self::MissingCover { movie_number } => {
                write!(f, "JavBus 页面缺少封面 movie_number={movie_number}")
            }
            Self::MissingDateOrDuration { movie_number } => {
                write!(
                    f,
                    "JavBus 页面缺少发行日期或时长 movie_number={movie_number}"
                )
            }
        }
    }
}

/// 一次查询失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 站点要求人机验证 —— 本次查询**未执行**，等宿主下次再试。
    Verification,
    /// HTTP 层：连不上 / 超时 / 非 2xx。
    Http(String),
    /// 页面拿到了但不能用（[`PageError`]）。
    Page(PageError),
    /// 封面下载失败。剧照失败是**跳过**，不走这里。
    Image(String),
    /// 交付目录写不进去 —— 那是宿主给的目录有问题，不是站点的问题。
    Delivery(String),
    /// HTTP 客户端建不出来（TLS 后端初始化失败之类）。
    Client(String),
}

impl FetchError {
    /// 给日志用的稳定标识。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Verification => "javbus_verification_required",
            Self::Http(_) => "javbus_http_failed",
            Self::Page(err) => err.code(),
            Self::Image(_) => "javbus_image_failed",
            Self::Delivery(_) => "javbus_delivery_failed",
            Self::Client(_) => "javbus_client_failed",
        }
    }
}

impl std::error::Error for FetchError {}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verification => write!(f, "JavBus 要求人机验证，本次查询未执行"),
            Self::Http(detail) => write!(f, "JavBus 请求失败：{detail}"),
            Self::Page(err) => write!(f, "{err}"),
            Self::Image(detail) => write!(f, "JavBus 图片下载失败：{detail}"),
            Self::Delivery(detail) => write!(f, "JavBus 交付失败：{detail}"),
            Self::Client(detail) => write!(f, "JavBus HTTP 客户端无法创建：{detail}"),
        }
    }
}

impl From<PageError> for FetchError {
    fn from(err: PageError) -> Self {
        Self::Page(err)
    }
}

/// 按番号查询 JavBus。
pub struct JavBusSource {
    /// 站点基址。结尾必有 `/`（[`crate::settings::Settings::base_url`]）。
    base: Url,
    /// 详情页：**不跟随**重定向。上游 `fetch_movie` 里是
    /// `follow_redirects=False` —— 跟着走就只能看到验证页，看不到那个指向
    /// `/doc/driver-verify` 的 302。
    pages: Client,
    /// 图片：跟随重定向（上游 `_download_image` 的 `follow_redirects=True`）。
    images: Client,
}

impl JavBusSource {
    pub fn new(settings: &Settings) -> Result<Self, FetchError> {
        let timeout = Duration::from_secs(settings.timeout_seconds);
        Ok(Self {
            base: settings.base_url.clone(),
            pages: client(timeout, Policy::none())?,
            images: client(timeout, Policy::limited(MAX_REDIRECTS))?,
        })
    }

    /// 按番号取一次元数据。
    ///
    /// `Ok(None)` = **没收录**（`found = false` 的正常响应）；`Err` = 调用失败。
    /// 这两件事在 gRPC 上是分开的，宿主据此决定「试下一个来源」还是「记一条
    /// 失败」（`sm_plugins::extension_calls::MovieLookup`）。
    ///
    /// `delivery_dir` 是宿主为本次请求分配的临时目录：图片写在
    /// `<delivery_dir>/<uuid>/<文件>` 里 —— **不是**上游那个插件自有的
    /// `<data_dir>/metadata-tmp/`，因为交付校验的边界是 `delivery_dir`
    /// （`sm_plugins::movie_delivery`）。
    pub async fn fetch_movie(
        &self,
        movie_number: &str,
        delivery_dir: &Path,
    ) -> Result<Option<FetchMovieResponse>, FetchError> {
        let requested = movie_number.trim();
        if requested.is_empty() {
            return Ok(None);
        }
        // 上游 `f"{BASE_URL}/{requested.upper()}"`。用字符串拼接而不是
        // `Url::join`：番号里出现 `//host` 时 `join` 会跳到别的站点，拼接不会。
        let detail_url = Url::parse(&format!("{}{}", self.base, requested.to_uppercase()))
            .map_err(|_| FetchError::Http(format!("番号 {requested} 拼不出详情页地址")))?;

        let response = self
            .pages
            .get(detail_url.clone())
            .header(USER_AGENT, BROWSER_USER_AGENT)
            .header(COOKIE, COOKIE_VALUE)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let landing_path = response.url().path().to_owned();
        let body = response.text().await.map_err(http_error)?;

        // 上游先看人机验证再看 404：被挡住时的状态码是 302，不是 404。
        if requires_verification(&landing_path, &location, &body) {
            return Err(FetchError::Verification);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(FetchError::Http(format!(
                "详情页 {detail_url} 返回 {status}"
            )));
        }

        let parsed = match parse_movie_page(&body)? {
            Some(parsed) => parsed,
            None => return Ok(None),
        };
        // 上游 `_compact(parsed.movie_number) != _compact(requested)` → None。
        if compact(&parsed.movie_number) != compact(requested) {
            return Ok(None);
        }

        let request_dir = delivery_dir.join(Uuid::new_v4().simple().to_string());
        if let Err(err) = std::fs::create_dir_all(&request_dir) {
            return Err(FetchError::Delivery(format!(
                "请求目录 {} 建不出来：{err}",
                request_dir.display()
            )));
        }
        match self
            .deliver(&parsed, &request_dir, detail_url.as_str())
            .await
        {
            Ok(response) => Ok(Some(response)),
            Err(err) => {
                // 上游 `shutil.rmtree(request_dir, ignore_errors=True)`：整体
                // 失败就整批清掉，不留半批图片让宿主误当成一次成功的交付。
                let _ = std::fs::remove_dir_all(&request_dir);
                Err(err)
            }
        }
    }

    /// 下载封面与剧照，拼出响应。
    async fn deliver(
        &self,
        parsed: &ParsedMovie,
        request_dir: &Path,
        referer: &str,
    ) -> Result<FetchMovieResponse, FetchError> {
        let cover = request_dir.join(format!("cover{}", suffix(&parsed.cover_href)));
        self.download(&parsed.cover_href, &cover, referer).await?;

        let mut plot_image_paths = Vec::with_capacity(parsed.plot_hrefs.len());
        for (index, href) in parsed.plot_hrefs.iter().enumerate() {
            let target = request_dir.join(format!("plot-{:02}{}", index + 1, suffix(href)));
            match self.download(href, &target, referer).await {
                Ok(()) => plot_image_paths.push(target.display().to_string()),
                // 单张剧照失败就跳过那张（`javbus.py:243`）：一部片子少一两张
                // 剧照不值得把整次查询判失败。
                Err(_) => {
                    let _ = std::fs::remove_file(&target);
                }
            }
        }

        Ok(FetchMovieResponse {
            found: true,
            movie_number: parsed.movie_number.clone(),
            title: parsed.title.clone(),
            release_date: parsed.release_date.clone(),
            duration_minutes: parsed.duration_minutes,
            cover_image_path: cover.display().to_string(),
            // 上游 `summary=""`：JavBus 详情页没有简介，给空串而不是缺省，
            // 让导入方明确知道「这里就是没有」。
            summary: Some(String::new()),
            maker_name: parsed.maker_name.clone(),
            director_name: parsed.director_name.clone(),
            series_name: parsed.series_name.clone(),
            actors: parsed
                .actors
                .iter()
                .map(|name| MetadataActor {
                    name: name.clone(),
                    alias_names: Vec::new(),
                })
                .collect(),
            tag_names: parsed.tags.clone(),
            plot_image_paths,
            source_url: Some(referer.to_owned()),
            source_id: None,
        })
    }

    async fn download(&self, href: &str, target: &Path, referer: &str) -> Result<(), FetchError> {
        // 上游 `urljoin(BASE_URL, url)`：相对地址挂到基址下，绝对地址（剧照常
        // 指向 `pics.dmm.co.jp`）原样保留。
        let url = self
            .base
            .join(href)
            .map_err(|_| FetchError::Image(format!("地址拼不出来：{href}")))?;
        let response = self
            .images
            .get(url)
            .header(USER_AGENT, BROWSER_USER_AGENT)
            .header(COOKIE, COOKIE_VALUE)
            .header(REFERER, referer)
            .send()
            .await
            .map_err(http_error)?;
        let bytes = response
            .error_for_status()
            .map_err(http_error)?
            .bytes()
            .await
            .map_err(http_error)?;
        std::fs::write(target, &bytes).map_err(|err| {
            FetchError::Delivery(format!("图片 {} 写不进去：{err}", target.display()))
        })
    }
}

fn client(timeout: Duration, redirect: Policy) -> Result<Client, FetchError> {
    Client::builder()
        .timeout(timeout)
        .redirect(redirect)
        // 与 `sm_service::transfers::torznab::TorznabClient` 同一个理由：环境
        // 里的 `HTTP_PROXY` 不该把请求导到别处（测试打的是回环假服务）。
        .no_proxy()
        .build()
        .map_err(|err| FetchError::Client(err.to_string()))
}

fn http_error(err: reqwest::Error) -> FetchError {
    FetchError::Http(err.to_string())
}

/// 上游 `_requires_verification`：落点路径、`Location` 或页面正文里有痕迹就算。
fn requires_verification(landing_path: &str, location: &str, body: &str) -> bool {
    landing_path.contains(VERIFICATION_PATH)
        || path_of(location).contains(VERIFICATION_PATH)
        || body.contains(VERIFICATION_MARKER)
}

/// 取一段地址里的 path。解析不了就按「去掉 query」处理（相对地址本就没有
/// scheme，上游对 `Location` 用的是 `urlsplit(...).path`）。
fn path_of(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(url) => url.path().to_owned(),
        Err(_) => raw.split('?').next().unwrap_or(raw).to_owned(),
    }
}

/// 解析详情页。`Ok(None)` = 这页面不是影片页；`Err` = 是影片页但缺必填字段。
pub fn parse_movie_page(html: &str) -> Result<Option<ParsedMovie>, PageError> {
    let mut parser = PageParser::default();
    parser.feed(html);

    let heading = squash(&parser.heading);
    let movie_number = parser
        .info
        .get(KEY_MOVIE_NUMBER)
        .cloned()
        .unwrap_or_default();
    // 上游：`if not movie_number and not heading: return None`。
    if movie_number.is_empty() && heading.is_empty() {
        return Ok(None);
    }
    if movie_number.is_empty() {
        return Err(PageError::MissingMovieNumber);
    }
    let Some(cover_href) = parser.cover_href.clone() else {
        return Err(PageError::MissingCover { movie_number });
    };

    let release_date = find_iso_date(info(&parser.info, KEY_RELEASE_DATE));
    let duration_minutes =
        find_duration_minutes(info(&parser.info, KEY_DURATION)).unwrap_or_default();
    let (Some(release_date), true) = (release_date, duration_minutes > 0) else {
        return Err(PageError::MissingDateOrDuration { movie_number });
    };

    let actors = dedup(parser.actors);
    Ok(Some(ParsedMovie {
        // 标题要在演员定下来之后才算 —— 它要把挂在末尾的演员名去掉。
        title: clean_title(&heading, &movie_number, &actors),
        movie_number,
        release_date,
        duration_minutes,
        maker_name: parser.info.get(KEY_MAKER).cloned(),
        director_name: parser.info.get(KEY_DIRECTOR).cloned(),
        series_name: parser.info.get(KEY_SERIES).cloned(),
        actors,
        tags: dedup(parser.tags),
        cover_href,
        plot_hrefs: parser.plot_hrefs,
    }))
}

fn info<'a>(map: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    map.get(key).map(String::as_str).unwrap_or_default()
}

/// 上游 `_PageParser`：把 `_PageParser` 的三个回调搬成 `feed`。
///
/// 状态机的每个分支都与 `javbus.py` 里的那一个对应，包括 `handle_data` 里
/// 「`_in_header_span` 时的数据**只**进 header，不进 value」那个 `elif`。
#[derive(Default)]
struct PageParser {
    heading: Vec<String>,
    info: BTreeMap<String, String>,
    actors: Vec<String>,
    tags: Vec<String>,
    cover_href: Option<String>,
    plot_hrefs: Vec<String>,

    in_heading: bool,
    in_header_span: bool,
    header: Vec<String>,
    value: Vec<String>,
    info_key: Option<String>,
    in_star: bool,
    star: Vec<String>,
    in_genre: bool,
    genre: Vec<String>,
    genre_has_link: bool,
}

impl PageParser {
    fn feed(&mut self, html: &str) {
        for event in html::events(html) {
            match event {
                Event::Start(tag) => self.start(&tag),
                Event::End(name) => self.end(name),
                Event::Text(text) => self.data(&text),
            }
        }
    }

    fn start(&mut self, tag: &Tag<'_>) {
        // 上游把这条放在整个 elif 链**之前**：`a` 也可能带着别的 class。
        if self.in_genre && tag.is("a") && tag.attr("href").is_some_and(|h| h.contains("/genre/")) {
            self.genre_has_link = true;
        }
        if tag.is("h3") {
            self.in_heading = true;
        } else if tag.is("span") && tag.has_class("header") {
            self.in_header_span = true;
            self.header.clear();
        } else if tag.is("span") && tag.has_class("genre") {
            self.in_genre = true;
            self.genre.clear();
            self.genre_has_link = false;
        } else if tag.is("a") && tag.has_class("bigImage") && self.cover_href.is_none() {
            // 只认第一个：页面上 `a.bigImage` 也只有一个。
            self.cover_href = tag.attr("href").map(str::to_owned);
        } else if tag.is("a") && tag.has_class("sample-box") {
            if let Some(href) = tag.attr("href") {
                self.plot_hrefs.push(href.to_owned());
            }
        } else if tag.is("div") && tag.has_class("star-name") {
            self.in_star = true;
            self.star.clear();
        }
    }

    fn end(&mut self, name: &str) {
        if name.eq_ignore_ascii_case("h3") {
            self.in_heading = false;
        } else if name.eq_ignore_ascii_case("span") {
            if self.in_header_span {
                self.in_header_span = false;
                // 上游 `_squash(...).rstrip(":") or None`。
                self.info_key = match squash(&self.header).trim_end_matches(':') {
                    "" => None,
                    key => Some(key.to_owned()),
                };
                self.value.clear();
            } else if self.in_genre {
                self.in_genre = false;
                let name = squash(&self.genre);
                // 上游：只有带 `/genre/` 链接的才是标签 —— 页面里还有一排
                // 长得一样、指向 `/star/` 的 `span.genre`，那是演员。
                if !name.is_empty() && self.genre_has_link {
                    self.tags.push(name);
                }
            }
        } else if name.eq_ignore_ascii_case("p") {
            let value = squash(&self.value);
            if let Some(key) = self.info_key.take() {
                if !value.is_empty() {
                    self.info.insert(key, value);
                }
            }
            self.value.clear();
        } else if name.eq_ignore_ascii_case("div") && self.in_star {
            self.in_star = false;
            let name = squash(&self.star);
            if !name.is_empty() {
                self.actors.push(name);
            }
        }
    }

    fn data(&mut self, text: &str) {
        if self.in_heading {
            self.heading.push(text.to_owned());
        }
        if self.in_star {
            self.star.push(text.to_owned());
        }
        if self.in_genre {
            self.genre.push(text.to_owned());
        }
        if self.in_header_span {
            self.header.push(text.to_owned());
        } else if self.info_key.is_some() {
            self.value.push(text.to_owned());
        }
    }
}

/// 上游 `_squash`：拼起来再按空白归一化。
fn squash(parts: &[String]) -> String {
    parts
        .concat()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 保序去重（上游 `tuple(dict.fromkeys(...))`）。
fn dedup(values: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        if !kept.contains(&value) {
            kept.push(value);
        }
    }
    kept
}

/// 上游 `_clean_title`：去掉开头的番号与结尾的演员名。
///
/// 页面上 `<h3>` 是「番号 + 标题 + 演员名」，而标题本身要单独存一列。
pub fn clean_title(heading: &str, movie_number: &str, actors: &[String]) -> String {
    let trimmed = heading.trim();
    let mut title = trimmed.to_owned();
    if title
        .to_uppercase()
        .starts_with(&movie_number.to_uppercase())
    {
        title = title
            .chars()
            .skip(movie_number.chars().count())
            .collect::<String>()
            .trim()
            .to_owned();
    }
    // 上游 `for name in reversed(actors)`：页面上的演员名顺序与标题末尾一致，
    // 倒着剥才不会把前一个的名字剩下一半。
    for name in actors.iter().rev() {
        if title.ends_with(name.as_str()) {
            let keep = title.chars().count() - name.chars().count();
            title = title
                .chars()
                .take(keep)
                .collect::<String>()
                .trim_end()
                .to_owned();
        }
    }
    // 上游 `return title or heading.strip()`：剥过头就退回原标题。
    if title.is_empty() {
        trimmed.to_owned()
    } else {
        title
    }
}

/// 上游 `_compact`：`re.sub(r"[^A-Z0-9]", "", value.upper())`。
///
/// 番号的一致性比较用它：页面上是 `SSIS-001`，请求可能是 `ssis-001`。
pub fn compact(value: &str) -> String {
    value
        .to_uppercase()
        .chars()
        .filter(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        .collect()
}

/// 上游 `_suffix`：认不出（或没有）后缀就当 `.jpg`。
pub fn suffix(url: &str) -> String {
    let path = Url::parse(url)
        .map(|parsed| parsed.path().to_owned())
        .unwrap_or_else(|_| url.to_owned());
    let extension = Path::new(&path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let candidate = format!(".{}", extension.to_lowercase());
    if IMAGE_SUFFIXES.contains(&candidate.as_str()) {
        candidate
    } else {
        ".jpg".to_owned()
    }
}

/// 上游 `_DATE.search(...)` + `date.fromisoformat`：第一个 `YYYY-MM-DD`。
fn find_iso_date(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for start in 0..=bytes.len().saturating_sub(10) {
        let Some(candidate) = text.get(start..start + 10) else {
            continue;
        };
        if !is_iso_shape(candidate.as_bytes()) {
            continue;
        }
        if candidate.parse::<NaiveDate>().is_ok() {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn is_iso_shape(bytes: &[u8]) -> bool {
    bytes.len() == 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// 上游 `_DURATION.search(...)`：`(\d+)\s*分鐘`。
fn find_duration_minutes(text: &str) -> Option<i32> {
    let marker = text.find(DURATION_UNIT)?;
    let bytes = text.as_bytes();
    let mut end = marker;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end {
        return None;
    }
    text.get(start..end)?.parse::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游 `tests/data/ssis001.html`，原样搬过来。剧照是绝对地址
    /// （`pics.dmm.co.jp`），单测不碰那部分；集成测试在喂给假服务前会把它改
    /// 成相对地址（见 `tests/lifecycle.rs`）。
    const FIXTURE: &str = include_str!("../tests/data/ssis001.html");

    const EXPECTED_TITLE: &str =
        "一ヶ月間の禁欲の果てに彼女のルームメイト2人と浮気SEXだけに没頭した\
        彼女不在の3日間。";

    #[test]
    fn the_page_is_parsed_field_by_field() {
        let parsed = parse_movie_page(FIXTURE)
            .expect("不该报错")
            .expect("应当是影片页");
        assert_eq!(parsed.movie_number, "SSIS-001");
        assert_eq!(parsed.title, EXPECTED_TITLE);
        assert_eq!(parsed.release_date, "2021-02-18");
        assert_eq!(parsed.duration_minutes, 150);
        assert_eq!(
            parsed.maker_name.as_deref(),
            Some("エスワン ナンバーワンスタイル")
        );
        assert_eq!(parsed.director_name.as_deref(), Some("苺原"));
        assert_eq!(parsed.series_name, None, "这一页没有系列");
        assert_eq!(parsed.actors, vec!["葵つかさ", "乙白さやか"]);
        assert_eq!(
            parsed.tags,
            vec![
                "多P",
                "美少女",
                "乳房",
                "薄馬賽克",
                "高畫質",
                "出軌",
                "戲劇",
                "DMM獨家"
            ]
        );
        assert_eq!(parsed.cover_href, "/pics/cover/83ie_b.jpg");
        assert_eq!(parsed.plot_hrefs.len(), 10);
    }

    #[test]
    fn actors_are_deduplicated_but_kept_in_order() {
        // 页面上有两处演员区（`star-name` 各出现两次），去重后仍是页面顺序。
        let mut parser = PageParser::default();
        parser.feed(FIXTURE);
        assert_eq!(
            parser.actors.len(),
            4,
            "原始条目未去重：{:?}",
            parser.actors
        );
        let parsed = parse_movie_page(FIXTURE).unwrap().unwrap();
        assert_eq!(parsed.actors, vec!["葵つかさ", "乙白さやか"]);
    }

    #[test]
    fn a_genre_span_without_a_genre_link_is_not_a_tag() {
        // 页面里还有一排 `span.genre` 指向 `/star/`（那是演员区），上游靠
        // 「有没有 `/genre/` 链接」把它排除掉。
        let parsed = parse_movie_page(FIXTURE).unwrap().unwrap();
        assert!(!parsed.tags.iter().any(|tag| tag == "葵つかさ"));
        assert!(
            !parsed.tags.iter().any(|tag| tag == "多選提交"),
            "提交按钮所在的 span.genre 没有链接，不算标签"
        );
    }

    #[test]
    fn a_page_without_a_duration_is_an_error() {
        let error = parse_movie_page(&FIXTURE.replace("150分鐘", "未知")).expect_err("时长缺失");
        assert_eq!(
            error,
            PageError::MissingDateOrDuration {
                movie_number: "SSIS-001".to_owned()
            }
        );
        assert_eq!(error.code(), "javbus_page_missing_date_or_duration");
    }

    #[test]
    fn an_impossible_date_is_an_error_not_a_garbage_value() {
        // `2021-13-45` 形状对但不是日期：上游 `date.fromisoformat` 也会拒。
        let html = FIXTURE.replace("2021-02-18", "2021-13-45");
        assert!(matches!(
            parse_movie_page(&html),
            Err(PageError::MissingDateOrDuration { .. })
        ));
    }

    #[test]
    fn a_page_without_the_movie_number_is_an_error() {
        let html = FIXTURE.replace("識別碼", "别的键");
        assert_eq!(
            parse_movie_page(&html).expect_err("缺少識別碼"),
            PageError::MissingMovieNumber
        );
    }

    #[test]
    fn a_page_without_a_cover_is_an_error() {
        let html = FIXTURE.replace("bigImage", "notBigImage");
        assert!(
            matches!(
                parse_movie_page(&html),
                Err(PageError::MissingCover { movie_number }) if movie_number == "SSIS-001"
            ),
            "封面是必填"
        );
    }

    #[test]
    fn a_page_that_is_not_a_movie_page_is_none() {
        // 既没番号也没标题：上游返回 `None`（不是一个错误）。
        assert_eq!(
            parse_movie_page("<html><body>請稍候</body></html>"),
            Ok(None)
        );
    }

    #[test]
    fn the_title_loses_the_number_prefix_and_the_actor_suffix() {
        let actors = vec!["葵つかさ".to_owned(), "乙白さやか".to_owned()];
        let heading = format!("SSIS-001 {EXPECTED_TITLE} 葵つかさ 乙白さやか");
        assert_eq!(clean_title(&heading, "SSIS-001", &actors), EXPECTED_TITLE);
        // 番号的大小写不影响剥离。
        assert_eq!(clean_title(&heading, "ssis-001", &actors), EXPECTED_TITLE);
    }

    #[test]
    fn a_title_that_is_only_the_number_falls_back_to_the_heading() {
        // 上游 `return title or heading.strip()`：剥光了就退回原标题，
        // 总比交一个空标题上去好。
        let actors = vec!["某人".to_owned()];
        assert_eq!(
            clean_title("ABC-123 某人", "ABC-123", &actors),
            "ABC-123 某人"
        );
    }

    #[test]
    fn the_number_comparison_ignores_separators_and_case() {
        assert_eq!(compact("ssis-001"), "SSIS001");
        assert_eq!(compact("SSIS-001"), "SSIS001");
        assert_eq!(compact("S S I S . 001"), "SSIS001");
        assert_ne!(compact("SSIS-001"), compact("SSIS-002"));
    }

    #[test]
    fn only_known_image_suffixes_survive() {
        assert_eq!(suffix("/pics/cover/83ie_b.jpg"), ".jpg");
        assert_eq!(suffix("https://x.example/a/b.PNG"), ".png");
        assert_eq!(suffix("https://x.example/a/b.webp?x=1"), ".webp");
        // 认不出就 `.jpg`（上游 `_IMAGE_SUFFIXES` 的兜底）。
        assert_eq!(suffix("https://x.example/a/b"), ".jpg");
        assert_eq!(suffix("https://x.example/a/b.tiff"), ".jpg");
    }

    #[tokio::test]
    async fn a_blank_number_is_not_found_without_any_request() {
        // 上游 `if not requested: return None` —— 连详情页都不该发。
        let source = JavBusSource::new(&Settings::default()).expect("客户端");
        let response = source
            .fetch_movie("   ", Path::new("/tmp"))
            .await
            .expect("不该失败");
        assert!(response.is_none());
    }

    #[test]
    fn the_verification_marker_is_seen_in_all_three_places() {
        assert!(requires_verification("/doc/driver-verify", "", ""));
        assert!(requires_verification(
            "/",
            "https://x/doc/driver-verify?next=1",
            ""
        ));
        assert!(requires_verification(
            "/",
            "",
            "<title>Age Verification JavBus</title>"
        ));
        assert!(!requires_verification("/SSIS-001", "", "<html>正文</html>"));
    }

    #[test]
    fn every_error_has_a_code() {
        // 错误码要能进日志，缺一个就成了「无码」。
        assert_eq!(
            FetchError::Verification.code(),
            "javbus_verification_required"
        );
        assert_eq!(
            FetchError::Http("x".to_owned()).code(),
            "javbus_http_failed"
        );
        assert_eq!(
            FetchError::Image("x".to_owned()).code(),
            "javbus_image_failed"
        );
        assert_eq!(
            FetchError::Delivery("x".to_owned()).code(),
            "javbus_delivery_failed"
        );
        assert_eq!(
            FetchError::Page(PageError::MissingMovieNumber).code(),
            "javbus_page_missing_movie_number"
        );
        assert!(
            FetchError::Verification.to_string().contains("人机验证"),
            "消息里要说清楚为什么没查"
        );
    }

    #[test]
    fn the_duration_and_date_scanners_take_the_first_match() {
        assert_eq!(find_duration_minutes("150分鐘"), Some(150));
        assert_eq!(
            find_duration_minutes("120 分鐘"),
            Some(120),
            "允许中间有空白"
        );
        assert_eq!(find_duration_minutes("未知"), None);
        assert_eq!(
            find_iso_date("發行：2021-02-18（水）"),
            Some("2021-02-18".to_owned())
        );
        assert_eq!(find_iso_date("沒有日期"), None);
    }
}
