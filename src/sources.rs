//! JavDB JSON 与 MinnanoAV HTML 适配，不保存登录态。
//!
//! # 上游对应：`sources.py`
//!
//! 逐条照搬：`normalize_name` / `normalize_fields` / `map_javdb_gender` /
//! `unqualified_name` / `minnanoav_ref` / `parse_minnanoav` / `Sources.javdb` /
//! `Sources.minnanoav`。
//!
//! # 两处与上游不同
//!
//! 1. **MinnanoAV 的站内校验按配置的基址走**。上游把
//!    `www.minnano-av.com` 写死在 `minnanoav_ref` 里；这里要能打本地假服务
//!    （测试不许联网），所以按 `Settings::minnanoav_base_url` 的
//!    scheme + host 校验，path 的形状（`/actress[0-9]+\.html`）不变。
//! 2. **HTML 解析不用 BeautifulSoup**。`parse_minnanoav` 用的选择器只有
//!    「按标签名 + class 找第一个」和「直接子元素」两件事，[`crate::html`]
//!    的事件流 + 本模块底部的迷你 DOM 足够复刻（见 `parse_minnanoav`）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use reqwest::header::{ACCEPT_LANGUAGE, REFERER, USER_AGENT};
use reqwest::{Client, Url};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::html::{self, Event};
use crate::settings::Settings;

/// 上游 `sources.py` 的 `PROFILE_FIELDS`：统一资料字段。
pub const PROFILE_FIELDS: [&str; 9] = [
    "birthday",
    "height_cm",
    "bust_cm",
    "waist_cm",
    "hips_cm",
    "cup",
    "birthplace",
    "blood_type",
    "gender",
];

/// 上游 `_APP_SIGNING_SEED`。
///
/// App 协议固定签名常量，沿用已验证的 JavDB 客户端；不是用户账号凭据。
const APP_SIGNING_SEED: &str = "71cf27bb3c0bcdf207b64abecddc970098c7421ee7203b9cdae54478478a199e7d5a6e1a57691123c1a931c057842fb73ba3b3c83bcd69c17ccf174081e3d8aa";
/// 上游 `jdsignature` 里的固定中段。
const SIGNATURE_MIDDLE: &str = "lpw6vgqzsp";
/// JavDB API 的 UA（上游写死的 `okhttp/4.9.0`）。
const JAVDB_USER_AGENT: &str = "okhttp/4.9.0";
/// MinnanoAV 的 UA（上游写死的 Chrome 120）。
const MINNANOAV_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 归一化后的单个资料字段值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    Int(i64),
}

impl FieldValue {
    /// 读回文本（`Int` 按十进制转字符串；上游 `str(raw)` 的对等物）。
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Int(i) => i.to_string(),
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Text(s) => s.parse().ok(),
        }
    }
}

/// 一位演员的资料快照（上游 `Profile`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub fields: BTreeMap<String, FieldValue>,
    pub names: Vec<String>,
    /// JavDB 是请求 URL；MinnanoAV 是资料页 path。
    pub reference: String,
    /// `ok` / `not_found` / `empty_profile` / `identity_mismatch` / `ambiguous`。
    pub outcome: String,
}

impl Profile {
    fn ok(fields: BTreeMap<String, FieldValue>, names: Vec<String>, reference: String) -> Self {
        Self {
            fields,
            names,
            reference,
            outcome: "ok".to_owned(),
        }
    }

    pub(crate) fn outcome(outcome: &str) -> Self {
        Self {
            outcome: outcome.to_owned(),
            ..Self::default()
        }
    }
}

/// 站点故障或解析异常：结束本轮，避免消耗后续演员的尝试次数。
///
/// 上游 `SourceError`。消息里**不带** URL 与请求头（上游 `_get` 的注释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SourceError {}

/// 上游 `normalize_name`：NFKC + casefold + 去空白。
pub fn normalize_name(value: &str) -> String {
    value
        .nfkc()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// 归一化前的原始输入（`normalize_fields` 的入参）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Raw<'a> {
    Text(&'a str),
    Int(i64),
    Bool(bool),
    Missing,
}

impl<'a> From<&'a str> for Raw<'a> {
    fn from(value: &'a str) -> Self {
        Self::Text(value)
    }
}

impl<'a> From<Option<&'a str>> for Raw<'a> {
    fn from(value: Option<&'a str>) -> Self {
        match value {
            Some(s) => Self::Text(s),
            None => Self::Missing,
        }
    }
}

/// 上游 `normalize_fields`：把来源给的原始值洗成统一资料字段。
///
/// 未知标记（`?` / `未知` / `非公開` / …）直接丢掉；`birthday` 要严格的
/// `YYYY-MM-DD` 且在 1900-01-01..=今天 之间；`*_cm` 只认 1..=250 的整数；
/// `blood_type` 只认 A/B/AB/O；`cup` 只认 1..=3 个大写字母；`gender` 只认
/// 1/2（或 female/女、male/男）。
pub fn normalize_fields(values: &BTreeMap<&'static str, Raw<'_>>) -> BTreeMap<String, FieldValue> {
    let mut result = BTreeMap::new();
    for key in PROFILE_FIELDS {
        let raw = values.get(key).copied().unwrap_or(Raw::Missing);
        // bool 在 Python 侧是 `isinstance(raw, bool)` 先行跳过 —— 注意
        // `isinstance(True, int)` 也是真，所以 bool 必须先判。
        let text = match raw {
            Raw::Bool(_) | Raw::Missing => continue,
            Raw::Text(s) => s.trim().to_owned(),
            Raw::Int(i) => i.to_string(),
        };
        // 未知标记：大小写不敏感比较（上游 `value.casefold()`）。
        let folded = text.to_lowercase();
        if matches!(
            folded.as_str(),
            "" | "?" | "未知" | "不明" | "非公開" | "－" | "n/a" | "none" | "null" | "-"
        ) {
            continue;
        }
        match key {
            "birthday" => {
                if let Some(iso) = valid_birthday(&text) {
                    result.insert(key.to_owned(), FieldValue::Text(iso));
                }
            }
            "height_cm" | "bust_cm" | "waist_cm" | "hips_cm" => {
                if let Some(cm) = parse_cm(&text) {
                    result.insert(key.to_owned(), FieldValue::Int(cm));
                }
            }
            "blood_type" => {
                let upper = text.to_uppercase();
                let stripped = upper.strip_suffix('型').unwrap_or(&upper);
                if matches!(stripped, "A" | "B" | "AB" | "O") {
                    result.insert(key.to_owned(), FieldValue::Text(stripped.to_owned()));
                }
            }
            "cup" => {
                let upper = text.to_uppercase();
                if (1..=3).contains(&upper.len()) && upper.bytes().all(|b| b.is_ascii_uppercase()) {
                    result.insert(key.to_owned(), FieldValue::Text(upper));
                }
            }
            "gender" => {
                let lower = text.to_lowercase();
                let gender = match lower.as_str() {
                    "1" | "female" | "女" => Some(1),
                    "2" | "male" | "男" => Some(2),
                    _ => None,
                };
                if let Some(g) = gender {
                    result.insert(key.to_owned(), FieldValue::Int(g));
                }
            }
            _ => {
                if text.chars().count() <= 255 {
                    result.insert(key.to_owned(), FieldValue::Text(text));
                }
            }
        }
    }
    result
}

/// 严格 `YYYY-MM-DD` 且在 1900-01-01..=今天之间（上游 `date.fromisoformat`
/// + 范围检查）。返回归一化后的 ISO 字符串。
fn valid_birthday(text: &str) -> Option<String> {
    let (y, m, d) = parse_iso_date(text)?;
    if y < 1900 {
        return None;
    }
    let today = chrono::Local::now().date_naive();
    let birthday = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    if birthday > today {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

fn parse_iso_date(text: &str) -> Option<(i32, u32, u32)> {
    let mut parts = text.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // 严格性：`from_ymd_opt` 拒掉 2021-02-29 这类不存在的日期；
    // 这里先做位数检查，`2021-1-1` 不算合法输入（上游 fromisoformat 同样拒）。
    if text.len() != 10 {
        return None;
    }
    chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    Some((y, m, d))
}

/// 上游 `([0-9]{1,3})\s*(?:cm|公分|厘米)?`（fullmatch，大小写不敏感），
///
/// 1..=250 才认。
fn parse_cm(text: &str) -> Option<i64> {
    let lower = text.to_lowercase();
    let digits = lower
        .strip_suffix("cm")
        .or_else(|| lower.strip_suffix("公分"))
        .or_else(|| lower.strip_suffix("厘米"))
        .unwrap_or(&lower)
        .trim();
    if !(1..=3).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    (1..=250).contains(&value).then_some(value)
}

/// 上游 `map_javdb_gender`：未知不返回，女性 1，男性 2。
pub fn map_javdb_gender(raw: &Value) -> Option<i64> {
    match raw {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::String(s) => {
            let lower = s.trim().to_lowercase();
            match lower.as_str() {
                "female" | "女" => Some(1),
                "male" | "男" => Some(2),
                "0" | "1" => map_javdb_gender(&Value::Number(lower.parse::<i64>().ok()?.into())),
                _ => None,
            }
        }
        Value::Number(n) => match n.as_i64()? {
            0 => Some(1),
            1 => Some(2),
            _ => None,
        },
        _ => None,
    }
}

/// 上游 `unqualified_name`：`名前（別名）` 取括号前。
pub fn unqualified_name(value: &str) -> String {
    value
        .split(['（', '('])
        .next()
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// MinnanoAV 资料页 path 形状：`/actress[0-9]+\.html`（fullmatch）。
fn is_actress_path(path: &str) -> bool {
    let rest = match path.strip_prefix("/actress") {
        Some(rest) => rest,
        None => return false,
    };
    let digits = match rest.strip_suffix(".html") {
        Some(digits) => digits,
        None => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

// ── 迷你 DOM ──────────────────────────────────────────────────────
//
// `parse_minnanoav` 只需要「按标签名 + class 找第一个」和「直接子元素」
// 两件事，用事件流手搭一棵小树比在扁平事件上维护状态机更接近上游的
// BeautifulSoup 写法。

enum Child {
    Text(String),
    Elem(Element),
}

struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Child>,
}

impl Element {
    fn has_class(&self, needle: &str) -> bool {
        self.attrs.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("class") && value.split_whitespace().any(|item| item == needle)
        })
    }

    /// 直接子元素（按标签名，大小写不敏感）。
    fn direct_child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find_map(|child| match child {
            Child::Elem(elem) if elem.name.eq_ignore_ascii_case(name) => Some(elem),
            _ => None,
        })
    }

    /// 后代里第一个符合的（DFS）。
    fn find_descendant(&self, name: &str, class: Option<&str>) -> Option<&Element> {
        let mut stack: Vec<&Element> = self
            .children
            .iter()
            .filter_map(|child| match child {
                Child::Elem(elem) => Some(elem),
                _ => None,
            })
            .collect();
        while let Some(elem) = stack.pop() {
            if elem.name.eq_ignore_ascii_case(name) && class.is_none_or(|c| elem.has_class(c)) {
                return Some(elem);
            }
            for child in elem.children.iter().rev() {
                if let Child::Elem(e) = child {
                    stack.push(e);
                }
            }
        }
        None
    }

    /// 文本内容：`sep` 连接，跳过 `skip` 命名的子树。
    /// 上游 `get_text(" ", strip=True)` 的对等物。
    fn text(&self, sep: &str, skip: Option<&str>) -> String {
        let mut parts = Vec::new();
        self.collect_text(sep, skip, &mut parts);
        parts
            .join(sep)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn collect_text(&self, sep: &str, skip: Option<&str>, parts: &mut Vec<String>) {
        for child in &self.children {
            match child {
                Child::Text(t) => {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_owned());
                    }
                }
                Child::Elem(elem) => {
                    if skip.is_some_and(|s| elem.name.eq_ignore_ascii_case(s)) {
                        continue;
                    }
                    elem.collect_text(sep, skip, parts);
                }
            }
        }
    }
}

fn build_tree(html: &str) -> Element {
    let mut root = Element {
        name: String::new(),
        attrs: Vec::new(),
        children: Vec::new(),
    };
    let mut stack: Vec<Element> = Vec::new();
    for event in html::events(html) {
        match event {
            Event::Start(tag) => {
                let mut attrs = Vec::new();
                if let Some(class) = tag.attr("class") {
                    attrs.push(("class".to_owned(), class.to_owned()));
                }
                if let Some(href) = tag.attr("href") {
                    attrs.push(("href".to_owned(), href.to_owned()));
                }
                stack.push(Element {
                    name: tag.name().to_ascii_lowercase(),
                    attrs,
                    children: Vec::new(),
                });
            }
            Event::End(_) => {
                if let Some(elem) = stack.pop() {
                    let child = Child::Elem(elem);
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(child),
                        None => root.children.push(child),
                    }
                }
            }
            Event::Text(text) => {
                let target = match stack.last_mut() {
                    Some(elem) => &mut elem.children,
                    None => &mut root.children,
                };
                target.push(Child::Text(text.into_owned()));
            }
        }
    }
    // 没闭合的标签：按文档顺序挂回（上游 BeautifulSoup 也会尽量收尾）。
    while let Some(elem) = stack.pop() {
        let child = Child::Elem(elem);
        match stack.last_mut() {
            Some(parent) => parent.children.push(child),
            None => root.children.push(child),
        }
    }
    root
}

/// 上游 `parse_minnanoav`。
pub fn parse_minnanoav(html: &str, reference: &str) -> Result<Profile, SourceError> {
    // 个别资料页会返回 200 但没有正文；这是该演员的可重试缺资料，
    // 不能把整批扫描当作站点故障中止。
    if html.trim().is_empty() {
        return Ok(Profile::outcome("empty_profile"));
    }
    let tree = build_tree(html);
    let card = tree.find_descendant("div", Some("act-profile"));
    let heading = tree.find_descendant("h1", None);
    let (Some(card), Some(heading)) = (card, heading) else {
        return Err(SourceError("minnanoav:profile_markup_missing".to_owned()));
    };
    // h1 里的 span 是装饰（假名注音之类），上游先 decompose 再取文本。
    let mut names = vec![heading.text(" ", Some("span"))];
    let mut rows: BTreeMap<String, String> = BTreeMap::new();
    // tr 的直接 td → 直接 span（label）+ 直接 p（值）。
    let mut tr_stack: Vec<&Element> = vec![card];
    while let Some(elem) = tr_stack.pop() {
        for child in &elem.children {
            let Child::Elem(child_elem) = child else {
                continue;
            };
            if child_elem.name.eq_ignore_ascii_case("tr") {
                if let Some(td) = child_elem.direct_child("td") {
                    let label = td.direct_child("span").map(|s| s.text(" ", None));
                    let value = td.direct_child("p").map(|p| p.text(" ", None));
                    if let (Some(label), Some(value)) = (label, value) {
                        if label == "別名" {
                            names.push(unqualified_name(&value));
                        }
                        rows.insert(label, value);
                    }
                }
            } else {
                tr_stack.push(child_elem);
            }
        }
    }
    if names[0].is_empty() {
        return Err(SourceError("minnanoav:profile_name_missing".to_owned()));
    }
    let mut values: BTreeMap<&'static str, Raw<'_>> = BTreeMap::new();
    let size = rows.get("サイズ").map(String::as_str).unwrap_or("");
    for (letter, key) in [
        ("T", "height_cm"),
        ("B", "bust_cm"),
        ("W", "waist_cm"),
        ("H", "hips_cm"),
    ] {
        if let Some(m) = search_measure(size, letter) {
            values.insert(key, Raw::Text(m));
        }
    }
    if let Some(cup) = search_cup(size) {
        values.insert("cup", Raw::Text(cup));
    }
    if let Some(birthday) = search_birthday(rows.get("生年月日").map(String::as_str).unwrap_or(""))
    {
        values.insert("birthday", Raw::Text(Box::leak(birthday.into_boxed_str())));
    }
    values.insert("birthplace", rows.get("出身地").map(String::as_str).into());
    values.insert("blood_type", rows.get("血液型").map(String::as_str).into());
    // 资料页位于 MinnanoAV 的 actress 目录；身份校验在 checked() 中完成。
    values.insert("gender", Raw::Text("1"));
    let fields = normalize_fields(&values);
    Ok(Profile::ok(
        fields,
        names.into_iter().filter(|n| !n.is_empty()).collect(),
        reference.to_owned(),
    ))
}

/// `T\s*([0-9]{2,3})` 这类量体：在 `size` 里找字母后的数字。
fn search_measure<'a>(size: &'a str, letter: &str) -> Option<&'a str> {
    let bytes = size.as_bytes();
    let target = letter.as_bytes()[0];
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == target {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if (2..=3).contains(&(j - start)) {
                return Some(&size[start..j]);
            }
        }
        i += 1;
    }
    None
}

/// `B\s*[0-9]{2,3}\s*\(\s*([A-Z])\s*カップ`：罩杯字母。
///
/// 括号可能是半角 `()` 也可能是全角 `（）`（MinnanoAV 两种都出现过）。
fn search_cup(size: &str) -> Option<&str> {
    let b_pos = size.find('B')?;
    let after_b = &size[b_pos + 1..];
    // 半角或全角左括号。
    let (paren_end, after_paren) = if let Some(pos) = after_b.find('(') {
        (pos, &after_b[pos + 1..])
    } else if let Some(pos) = after_b.find('（') {
        (pos, &after_b[pos + '（'.len_utf8()..])
    } else {
        return None;
    };
    let digits: String = after_b[..paren_end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let after_paren = after_paren.trim_start();
    let cup = after_paren.chars().next()?;
    if !cup.is_ascii_uppercase() {
        return None;
    }
    let rest = after_paren[cup.len_utf8()..].trim_start();
    if !rest.starts_with("カップ") {
        return None;
    }
    Some(&after_paren[..cup.len_utf8()])
}

/// `([0-9]{4})年\s*([0-9]{1,2})月\s*([0-9]{1,2})日` → `YYYY-MM-DD`。
fn search_birthday(text: &str) -> Option<String> {
    let year_end = text.find('年')?;
    let year: i32 = text[..year_end].trim().parse().ok()?;
    let after_year = text[year_end + '年'.len_utf8()..].trim_start();
    let month_end = after_year.find('月')?;
    let month: u32 = after_year[..month_end].trim().parse().ok()?;
    let after_month = after_year[month_end + '月'.len_utf8()..].trim_start();
    let day_end = after_month.find('日')?;
    let day: u32 = after_month[..day_end].trim().parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

// ── 站点客户端 ────────────────────────────────────────────────────

/// JavDB / MinnanoAV 的 HTTP 适配（上游 `Sources`）。
pub struct Sources {
    javdb_base: Url,
    minnanoav_base: Url,
    interval: Duration,
    client: Client,
    last_request: Option<Instant>,
}

impl Sources {
    pub fn new(settings: &Settings) -> Result<Self, SourceError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(settings.timeout_seconds))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|e| SourceError(format!("client:init_failed:{e}")))?;
        Ok(Self {
            javdb_base: settings.javdb_base_url.clone(),
            minnanoav_base: settings.minnanoav_base_url.clone(),
            interval: Duration::from_secs_f64(settings.request_interval_seconds),
            client,
            last_request: None,
        })
    }

    /// 请求间隔节流（上游 `_get` 开头的 sleep）。
    async fn throttle(&mut self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
        }
        self.last_request = Some(Instant::now());
    }

    /// GET 一次。`Ok(None)` = 404（没收录）；`Err` = 站点故障。
    ///
    /// 异常消息里**不带** URL（上游 `_get` 的注释：不把 URL/请求头写进日志）。
    async fn get(
        &mut self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<Option<SitePage>, SourceError> {
        self.throttle().await;
        let site = if url.as_str().starts_with(self.minnanoav_base.as_str()) {
            "minnanoav"
        } else {
            "javdb"
        };
        let mut request = self.client.get(url.clone());
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| SourceError(format!("{site}:network_error")))?;
        // 先记最终 URL（跟随重定向后的），再按状态码分流。
        let final_url = response.url().clone();
        match response.status().as_u16() {
            404 => Ok(None),
            200 => {
                let text = response
                    .text()
                    .await
                    .map_err(|_| SourceError(format!("{site}:network_error")))?;
                Ok(Some(SitePage {
                    text,
                    url: final_url,
                }))
            }
            code => Err(SourceError(format!("{site}:http_{code}"))),
        }
    }

    /// 按 JavDB 演员 id 取资料（上游 `Sources.javdb`）。
    pub async fn javdb(&mut self, javdb_id: &str) -> Result<Profile, SourceError> {
        // `quote(javdb_id, safe='')`：全转义。
        let encoded: String = url::form_urlencoded::byte_serialize(javdb_id.as_bytes()).collect();
        let url = self
            .javdb_base
            .join(&format!("api/v1/actors/{encoded}"))
            .map_err(|e| SourceError(format!("javdb:bad_url:{e}")))?;
        // App 协议签名：`{ts}.lpw6vgqzsp.{md5(ts + seed)}`。
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let digest = format!(
            "{:x}",
            Md5::digest(format!("{ts}{APP_SIGNING_SEED}").as_bytes())
        );
        let signature = format!("{ts}.{SIGNATURE_MIDDLE}.{digest}");
        let page = match self
            .get(
                &url,
                &[
                    ("jdsignature", &signature),
                    (USER_AGENT.as_str(), JAVDB_USER_AGENT),
                    (ACCEPT_LANGUAGE.as_str(), "zh-TW"),
                ],
            )
            .await?
        {
            Some(page) => page,
            None => {
                return Ok(Profile {
                    reference: url.to_string(),
                    ..Profile::outcome("not_found")
                })
            }
        };
        let payload: Value = serde_json::from_str(&page.text)
            .map_err(|_| SourceError("javdb:invalid_json".to_owned()))?;
        if payload.get("success").and_then(Value::as_i64) != Some(1) {
            return Err(SourceError("javdb:api_rejected".to_owned()));
        }
        let actor = payload
            .get("data")
            .and_then(|d| d.get("actor"))
            .filter(|a| a.is_object());
        let actor = match actor {
            Some(a) => a,
            None => return Err(SourceError("javdb:actor_mismatch_or_missing".to_owned())),
        };
        let id_matches = actor.get("id").is_some_and(|id| {
            id.as_str().is_some_and(|s| s == javdb_id)
                || id.as_i64().is_some_and(|n| n.to_string() == javdb_id)
        });
        if !id_matches {
            return Err(SourceError("javdb:actor_mismatch_or_missing".to_owned()));
        }
        let mut names = Vec::new();
        for key in ["name", "name_zht"] {
            if let Some(name) = actor.get(key).and_then(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() && !names.contains(&name.to_owned()) {
                    names.push(name.to_owned());
                }
            }
        }
        if let Some(other) = actor.get("other_name").and_then(Value::as_str) {
            for name in other.split([',', '，', '、', ';', '；', '/', '／']) {
                let name = name.trim();
                if !name.is_empty() && !names.contains(&name.to_owned()) {
                    names.push(name.to_owned());
                }
            }
        }
        let mut raws: BTreeMap<&'static str, Raw<'_>> = BTreeMap::new();
        for key in PROFILE_FIELDS.iter().filter(|k| **k != "gender") {
            let raw = match actor.get(*key) {
                None | Some(Value::Null) => Raw::Missing,
                Some(Value::Bool(b)) => Raw::Bool(*b),
                Some(Value::Number(n)) => match n.as_i64() {
                    Some(i) => Raw::Int(i),
                    None => Raw::Text(""),
                },
                Some(Value::String(s)) => Raw::Text(s),
                Some(_) => Raw::Missing,
            };
            raws.insert(key, raw);
        }
        if let Some(gender) = actor.get("gender").and_then(map_javdb_gender) {
            raws.insert("gender", Raw::Int(gender));
        }
        for (upstream, local) in [
            ("height", "height_cm"),
            ("bust", "bust_cm"),
            ("waist", "waist_cm"),
            ("hips", "hips_cm"),
        ] {
            let raw = match actor.get(upstream) {
                None | Some(Value::Null) => Raw::Missing,
                Some(Value::Bool(b)) => Raw::Bool(*b),
                Some(Value::Number(n)) => match n.as_i64() {
                    Some(i) => Raw::Int(i),
                    None => Raw::Missing,
                },
                Some(Value::String(s)) => Raw::Text(s),
                Some(_) => Raw::Missing,
            };
            raws.insert(local, raw);
        }
        Ok(Profile::ok(normalize_fields(&raws), names, url.to_string()))
    }

    /// 按名字在 MinnanoAV 搜索并取资料（上游 `Sources.minnanoav`）。
    pub async fn minnanoav(
        &mut self,
        names: &[String],
        reference: &str,
    ) -> Result<Profile, SourceError> {
        let mut names: Vec<String> = names
            .iter()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        let expected: std::collections::HashSet<String> =
            names.iter().map(|n| normalize_name(n)).collect();
        // referer 先克隆出来：headers 借它，而 self.get 要 &mut self。
        let referer = self.minnanoav_base.as_str().to_owned();
        let headers = [
            (USER_AGENT.as_str(), MINNANOAV_USER_AGENT),
            (ACCEPT_LANGUAGE.as_str(), "ja-JP,ja;q=0.9,en;q=0.7"),
            (REFERER.as_str(), referer.as_str()),
        ];

        // 有缓存的资料页引用时先直取（仍须校验身份）。
        if !reference.is_empty() {
            if !is_actress_path(reference) {
                return Err(SourceError("minnanoav:invalid_mapping".to_owned()));
            }
            let url = self
                .minnanoav_base
                .join(reference.trim_start_matches('/'))
                .map_err(|e| SourceError(format!("minnanoav:bad_url:{e}")))?;
            if let Some(page) = self.get(&url, &headers).await? {
                let profile = self.checked(&page, &expected)?;
                if matches!(profile.outcome.as_str(), "ok" | "empty_profile") {
                    return Ok(profile);
                }
            }
            // 缓存失效时在本轮重新搜索，仍须校验身份。
        }

        let mut outcome = "not_found".to_owned();
        for name in names.iter().take(3) {
            let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
            let url = self
                .minnanoav_base
                .join(&format!(
                    "search_result.php?search_scope=actress&search_word={encoded}&search=Go"
                ))
                .map_err(|e| SourceError(format!("minnanoav:bad_url:{e}")))?;
            let page = match self.get(&url, &headers).await? {
                Some(page) => page,
                None => continue,
            };
            // 搜索可能 302 直达资料页。
            if !self.actress_ref(page.url.as_str()).is_empty() {
                let profile = self.checked(&page, &expected)?;
                if profile.outcome == "ok" {
                    return Ok(profile);
                }
                outcome = profile.outcome.clone();
                continue;
            }
            let tree = build_tree(&page.text);
            let heading = tree.find_descendant("h1", None);
            let is_search_page =
                heading.is_some_and(|h| h.text(" ", None).contains("AV女優検索結果"));
            if !is_search_page {
                return Err(SourceError("minnanoav:search_markup_missing".to_owned()));
            }
            // h2.ttl a[href] 里名字完全一致的候选。
            let mut candidates = std::collections::HashSet::new();
            let mut stack: Vec<&Element> = vec![&tree];
            while let Some(elem) = stack.pop() {
                for child in &elem.children {
                    if let Child::Elem(e) = child {
                        if e.name.eq_ignore_ascii_case("h2") && e.has_class("ttl") {
                            if let Some(a) = e.find_descendant("a", None) {
                                let href = a
                                    .attrs
                                    .iter()
                                    .find(|(k, _)| k == "href")
                                    .map(|(_, v)| v.as_str())
                                    .unwrap_or("");
                                let candidate_ref = self.actress_ref(href);
                                if !candidate_ref.is_empty()
                                    && normalize_name(&unqualified_name(&a.text(" ", None)))
                                        == normalize_name(name)
                                {
                                    candidates.insert(candidate_ref);
                                }
                            }
                        }
                        stack.push(e);
                    }
                }
            }
            if candidates.len() > 1 {
                return Ok(Profile::outcome("ambiguous"));
            }
            let Some(candidate) = candidates.into_iter().next() else {
                continue;
            };
            let url = self
                .minnanoav_base
                .join(candidate.trim_start_matches('/'))
                .map_err(|e| SourceError(format!("minnanoav:bad_url:{e}")))?;
            let page = match self.get(&url, &headers).await? {
                Some(page) => page,
                None => continue,
            };
            let profile = self.checked(&page, &expected)?;
            if profile.outcome == "ok" {
                return Ok(profile);
            }
            outcome = profile.outcome.clone();
        }
        Ok(Profile::outcome(&outcome))
    }

    /// 资料页引用校验（上游 `minnanoav_ref`）：scheme + host 按配置基址，
    /// path 形状 `/actress[0-9]+\.html` 不变。
    fn actress_ref(&self, url: &str) -> String {
        let joined = self.minnanoav_base.join(url);
        let link = match joined {
            Ok(link) => link,
            Err(_) => return String::new(),
        };
        if link.scheme() == self.minnanoav_base.scheme()
            && link.host_str() == self.minnanoav_base.host_str()
            && is_actress_path(link.path())
        {
            link.path().to_owned()
        } else {
            String::new()
        }
    }

    /// 取回的资料页必须通过身份校验（上游 `checked`）。
    fn checked(
        &self,
        page: &SitePage,
        expected: &std::collections::HashSet<String>,
    ) -> Result<Profile, SourceError> {
        let actual_ref = self.actress_ref(page.url.as_str());
        if actual_ref.is_empty() {
            return Err(SourceError("minnanoav:unexpected_redirect".to_owned()));
        }
        let profile = parse_minnanoav(&page.text, &actual_ref)?;
        if profile.outcome != "ok" {
            return Ok(profile);
        }
        let identified = profile
            .names
            .iter()
            .any(|n| expected.contains(&normalize_name(n)));
        if !identified {
            return Ok(Profile::outcome("identity_mismatch"));
        }
        Ok(profile)
    }
}

/// 一次 GET 的正文与最终 URL（跟随重定向后）。
struct SitePage {
    text: String,
    url: Url,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn raw<'a>(pairs: &[(&'static str, Raw<'a>)]) -> BTreeMap<&'static str, Raw<'a>> {
        pairs.iter().map(|(k, v)| (*k, *v)).collect()
    }

    #[test]
    fn unknown_markers_are_dropped() {
        for marker in [
            "?",
            "未知",
            "不明",
            "非公開",
            "－",
            "n/a",
            "none",
            "null",
            "-",
            "",
        ] {
            let fields = normalize_fields(&raw(&[("birthplace", Raw::Text(marker))]));
            assert!(!fields.contains_key("birthplace"), "标记 {marker} 应丢掉");
        }
        // bool 直接跳过（Python 先判 isinstance(bool)）。
        let fields = normalize_fields(&raw(&[("birthplace", Raw::Bool(true))]));
        assert!(!fields.contains_key("birthplace"));
    }

    #[test]
    fn birthday_must_be_a_valid_iso_date_in_range() {
        let ok = normalize_fields(&raw(&[("birthday", Raw::Text("1998-05-06"))]));
        assert_eq!(
            ok.get("birthday"),
            Some(&FieldValue::Text("1998-05-06".to_owned()))
        );
        for bad in [
            "1998/05/06",
            "1998-5-6",
            "1998-02-29",
            "1899-12-31",
            "3000-01-01",
            "昨天",
        ] {
            let fields = normalize_fields(&raw(&[("birthday", Raw::Text(bad))]));
            assert!(!fields.contains_key("birthday"), "{bad} 应丢掉");
        }
    }

    #[test]
    fn cm_values_accept_units_and_reject_out_of_range() {
        for (input, expected) in [
            ("170", 170),
            ("170cm", 170),
            ("170CM", 170),
            ("170公分", 170),
            ("85厘米", 85),
        ] {
            let fields = normalize_fields(&raw(&[("height_cm", Raw::Text(input))]));
            assert_eq!(
                fields.get("height_cm"),
                Some(&FieldValue::Int(expected)),
                "{input}"
            );
        }
        for bad in ["0", "251", "170.5", "abc", "1700"] {
            let fields = normalize_fields(&raw(&[("height_cm", Raw::Text(bad))]));
            assert!(!fields.contains_key("height_cm"), "{bad} 应丢掉");
        }
        // 整数输入同样走范围检查。
        let fields = normalize_fields(&raw(&[("height_cm", Raw::Int(165))]));
        assert_eq!(fields.get("height_cm"), Some(&FieldValue::Int(165)));
    }

    #[test]
    fn blood_type_cup_and_gender_are_validated() {
        let fields = normalize_fields(&raw(&[
            ("blood_type", Raw::Text("a型")),
            ("cup", Raw::Text("d")),
            ("gender", Raw::Text("女")),
        ]));
        assert_eq!(
            fields.get("blood_type"),
            Some(&FieldValue::Text("A".to_owned()))
        );
        assert_eq!(fields.get("cup"), Some(&FieldValue::Text("D".to_owned())));
        assert_eq!(fields.get("gender"), Some(&FieldValue::Int(1)));

        let fields = normalize_fields(&raw(&[
            ("blood_type", Raw::Text("X")),
            ("cup", Raw::Text("DDDD")),
            ("gender", Raw::Text("3")),
        ]));
        assert!(!fields.contains_key("blood_type"));
        assert!(!fields.contains_key("cup"));
        assert!(!fields.contains_key("gender"));

        let fields = normalize_fields(&raw(&[("gender", Raw::Text("male"))]));
        assert_eq!(fields.get("gender"), Some(&FieldValue::Int(2)));
    }

    #[test]
    fn javdb_gender_mapping_matches_upstream() {
        // 0 → 女(1)，1 → 男(2)；字符串同样认。
        assert_eq!(map_javdb_gender(&Value::from(0)), Some(1));
        assert_eq!(map_javdb_gender(&Value::from(1)), Some(2));
        assert_eq!(map_javdb_gender(&Value::from("female")), Some(1));
        assert_eq!(map_javdb_gender(&Value::from("男")), Some(2));
        assert_eq!(map_javdb_gender(&Value::Null), None);
        assert_eq!(map_javdb_gender(&Value::from(true)), None);
        assert_eq!(map_javdb_gender(&Value::from("unknown")), None);
        assert_eq!(map_javdb_gender(&Value::from(5)), None);
    }

    #[test]
    fn minnanoav_profile_parses() {
        let html = r#"
        <html><body>
        <h1>河北彩花 <span>かわきた さいか</span></h1>
        <div class="act-profile"><table>
        <tr><td><span>生年月日</span><p>1999年4月19日</p></td></tr>
        <tr><td><span>サイズ</span><p>T169 / B88（Dカップ） / W58 / H89</p></td></tr>
        <tr><td><span>出身地</span><p>東京都</p></td></tr>
        <tr><td><span>血液型</span><p>A型</p></td></tr>
        <tr><td><span>別名</span><p>河北彩伽（旧名）</p></td></tr>
        </table></div>
        </body></html>
        "#;
        let profile = parse_minnanoav(html, "/actress123.html").unwrap();
        assert_eq!(profile.outcome, "ok");
        assert_eq!(
            profile.names,
            vec!["河北彩花".to_owned(), "河北彩伽".to_owned()]
        );
        assert_eq!(
            profile.fields.get("birthday"),
            Some(&FieldValue::Text("1999-04-19".to_owned()))
        );
        assert_eq!(profile.fields.get("height_cm"), Some(&FieldValue::Int(169)));
        assert_eq!(profile.fields.get("bust_cm"), Some(&FieldValue::Int(88)));
        assert_eq!(
            profile.fields.get("cup"),
            Some(&FieldValue::Text("D".to_owned()))
        );
        assert_eq!(
            profile.fields.get("birthplace"),
            Some(&FieldValue::Text("東京都".to_owned()))
        );
        assert_eq!(
            profile.fields.get("blood_type"),
            Some(&FieldValue::Text("A".to_owned()))
        );
        assert_eq!(profile.fields.get("gender"), Some(&FieldValue::Int(1)));
    }

    #[test]
    fn minnanoav_empty_page_is_a_retryable_outcome_not_an_error() {
        let profile = parse_minnanoav("   \n  ", "/actress1.html").unwrap();
        assert_eq!(profile.outcome, "empty_profile");
    }

    #[test]
    fn minnanoav_missing_markup_is_an_error() {
        let err =
            parse_minnanoav("<html><body>no profile</body></html>", "/actress1.html").unwrap_err();
        assert_eq!(
            err,
            SourceError("minnanoav:profile_markup_missing".to_owned())
        );
    }

    #[test]
    fn normalize_name_folds_width_and_case() {
        assert_eq!(normalize_name("河北 彩花"), "河北彩花");
        assert_eq!(normalize_name("ＡＢＣ"), "abc");
    }
}
