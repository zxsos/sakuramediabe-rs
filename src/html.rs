//! DMM 页面扫描器：只做 `dmm.py` 的 `_Page` 需要的那几件事。
//!
//! # 上游用的是 `html.parser.HTMLParser`
//!
//! `dmm.py` 的 `_Page` 继承它，用 `handle_starttag` / `handle_endtag` /
//! `handle_data` 三个回调攒出：`links`、`visible`、`page_title`、`heading`、
//! `description`、`products`（JSON-LD 里的 Product）。
//!
//! Rust 侧手写扫描器，行为对齐 `HTMLParser(convert_charrefs=True)`：
//!
//! - 标签名与属性名**大小写不敏感**；
//! - 文本里的字符引用在交给调用方之前解码；
//! - `<!DOCTYPE …>` 与 `<!-- … -->` 不产生事件；
//! - 自闭合写法（`<br/>`）只产生一次开始事件。
//!
//! # 与 `plugin-javbus-metadata` 的 `html` 模块的关系
//!
//! 那边是无栈的事件流（JavBus 详情页只需要「标签名 + class + href + 文本」）；
//! 这里需要**栈** —— `heading` / `description` / `json` / `hidden` 都是「祖先
//! 标签满足条件」的语义，必须知道当前在哪些标签里面。所以各写一份，不复用。

use std::borrow::Cow;

/// 解码 HTML 字符引用（`&amp;` / `&#123;` / `&#x7f;` 等）。
fn decode_entities(text: &str) -> Cow<'_, str> {
    html_escape::decode_html_entities(text)
}

/// 一个开始标签（标签名与属性都借用原文）。
#[derive(Debug, Clone)]
pub struct Tag<'a> {
    pub name: &'a str,
    pub attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> Tag<'a> {
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| *value)
    }

    pub fn has_class(&self, needle: &str) -> bool {
        self.attr("class")
            .is_some_and(|value| value.split_whitespace().any(|item| item == needle))
    }
}

/// 扫描器。用法：`DmmPage::parse(html)` 一次拿到全部字段。
#[derive(Debug, Default)]
pub struct DmmPage {
    /// 所有 `<a href>`。
    pub links: Vec<String>,
    /// 可见文本（script/style 里的不要）。
    pub visible: Vec<String>,
    /// `<title>` 里的文本。
    pub page_title: Vec<String>,
    /// `h1#title` 或 `h1.item.fn` 里的文本。
    pub heading: Vec<String>,
    /// `div.mg-b20.lh4` 里的文本。
    pub description: Vec<String>,
    /// JSON-LD 里的 Product 对象（已解析的 JSON）。
    pub products: Vec<serde_json::Value>,
}

impl DmmPage {
    pub fn parse(html: &str) -> Self {
        let mut page = Self::default();
        let mut parser = Parser {
            page: &mut page,
            stack: Vec::new(),
            script_buf: Vec::new(),
        };
        parser.run(html);
        page
    }
}

/// 标签上的标记（上游 `_Page.handle_starttag` 里的 flags）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Hidden,      // script / style
    Json,        // script[type=application/ld+json]
    Title,       // title
    Heading,     // h1#title 或 h1.item.fn
    Description, // div.mg-b20.lh4
}

struct Parser<'p> {
    page: &'p mut DmmPage,
    stack: Vec<(&'p str, Vec<Flag>)>,
    script_buf: Vec<&'p str>,
}

impl<'p> Parser<'p> {
    fn run(&mut self, html: &'p str) {
        let bytes = html.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'<' {
                // 注释 / DOCTYPE：跳过。
                if html[i..].starts_with("<!--") {
                    if let Some(end) = html[i..].find("-->") {
                        i += end + 3;
                        continue;
                    }
                    break;
                }
                if html[i..].starts_with("<!") {
                    if let Some(end) = html[i..].find('>') {
                        i += end + 1;
                        continue;
                    }
                    break;
                }
                // 结束标签。
                if html[i..].starts_with("</") {
                    if let Some(end) = html[i..].find('>') {
                        let name = html[i + 2..i + end].trim();
                        self.handle_end_tag(name);
                        i += end + 1;
                        continue;
                    }
                    break;
                }
                // 开始标签。
                if let Some(end) = html[i..].find('>') {
                    let raw = &html[i + 1..i + end];
                    let self_closing = raw.trim_end().ends_with('/');
                    let tag = parse_tag(raw.trim_end_matches('/').trim_end());
                    // <br> / <p>：上游会先喂一个空格。
                    if tag.is("br") || tag.is("p") {
                        self.handle_data(" ");
                    }
                    self.handle_start_tag(&tag);
                    if !self_closing && !is_void(&tag) {
                        let flags = flags_for(&tag);
                        self.stack.push((tag.name, flags));
                    } else if tag.is("script") {
                        // 自闭合的 script 不会有内容，直接弹栈检查 JSON。
                        self.check_json_on_close(&tag);
                    }
                    i += end + 1;
                    continue;
                }
                break;
            }
            // 文本：读到下一个 `<`。
            let next = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
            self.handle_data(&html[i..next]);
            i = next;
        }
    }

    fn handle_start_tag(&mut self, tag: &Tag<'p>) {
        let flags = flags_for(tag);
        if flags.contains(&Flag::Json) {
            self.script_buf.clear();
        }
        if tag.is("a") {
            if let Some(href) = tag.attr("href") {
                self.page.links.push(href.to_owned());
            }
        }
        // 注意：flags 要在 handle_data 时从 stack 里 union 出来；
        // 这里只处理 script_buf 的清空（Json 开始）。
        let _ = flags;
    }

    fn handle_end_tag(&mut self, name: &str) {
        // 从栈顶往下找第一个同名标签，弹掉它及之后的所有。
        for idx in (0..self.stack.len()).rev() {
            if self.stack[idx].0.eq_ignore_ascii_case(name.trim()) {
                let popped: Vec<_> = self.stack.drain(idx..).collect();
                if popped.iter().any(|(_, f)| f.contains(&Flag::Json)) {
                    let text: String = self.script_buf.iter().copied().collect();
                    self.script_buf.clear();
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        read_products(&value, &mut self.page.products);
                    }
                }
                break;
            }
        }
    }

    fn check_json_on_close(&mut self, _tag: &Tag<'p>) {
        // 自闭合 script：buf 是空的，无事可做。
    }

    fn handle_data(&mut self, data: &'p str) {
        let all_flags: Vec<Flag> = self
            .stack
            .iter()
            .flat_map(|(_, f)| f.iter().copied())
            .collect();
        if all_flags.contains(&Flag::Json) {
            self.script_buf.push(data);
        }
        if all_flags.contains(&Flag::Hidden) {
            return;
        }
        // 可见文本：原样保留（调用方再做 _text 规范化）。
        self.page.visible.push(data.to_owned());
        for (flag, dest) in [
            (Flag::Title, &mut self.page.page_title),
            (Flag::Heading, &mut self.page.heading),
            (Flag::Description, &mut self.page.description),
        ] {
            if all_flags.contains(&flag) {
                dest.push(data.to_owned());
            }
        }
    }
}

/// 上游 `_Page.handle_starttag` 里的 flags 判定。
fn flags_for(tag: &Tag<'_>) -> Vec<Flag> {
    let mut flags = Vec::new();
    if tag.is("script") || tag.is("style") {
        flags.push(Flag::Hidden);
    }
    if tag.is("script")
        && tag
            .attr("type")
            .is_some_and(|t| t.eq_ignore_ascii_case("application/ld+json"))
    {
        flags.push(Flag::Json);
    }
    if tag.is("title") {
        flags.push(Flag::Title);
    }
    if tag.is("h1")
        && (tag.attr("id").is_some_and(|id| id == "title")
            || (tag.has_class("item") && tag.has_class("fn")))
    {
        flags.push(Flag::Heading);
    }
    if tag.is("div") && tag.has_class("mg-b20") && tag.has_class("lh4") {
        flags.push(Flag::Description);
    }
    flags
}

/// 不需要闭合标签的 void 元素（上游的那个集合）。
fn is_void(tag: &Tag<'_>) -> bool {
    matches!(
        tag.name.to_ascii_lowercase().as_str(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// 解析一个开始标签的标签名与属性。
fn parse_tag(raw: &str) -> Tag<'_> {
    let raw = raw.trim();
    let name_end = raw.find(|c: char| c.is_whitespace()).unwrap_or(raw.len());
    let name = &raw[..name_end];
    let mut attrs = Vec::new();
    let mut rest = raw[name_end..].trim();
    while !rest.is_empty() {
        // 属性名。
        let key_end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        if key.is_empty() {
            break;
        }
        // 属性值。
        let value = if rest.starts_with('=') {
            rest = rest[1..].trim_start();
            if rest.starts_with('"') {
                let end = rest[1..].find('"').map(|p| p + 1).unwrap_or(rest.len() - 1);
                let v = &rest[1..end];
                rest = rest[end + 1..].trim_start();
                v
            } else if rest.starts_with('\'') {
                let end = rest[1..]
                    .find('\'')
                    .map(|p| p + 1)
                    .unwrap_or(rest.len() - 1);
                let v = &rest[1..end];
                rest = rest[end + 1..].trim_start();
                v
            } else {
                let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                let v = &rest[..end];
                rest = rest[end..].trim_start();
                v
            }
        } else {
            ""
        };
        attrs.push((key, value));
    }
    Tag { name, attrs }
}

/// 从 JSON-LD 里抠 Product（上游 `_read_products`）。
fn read_products(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                read_products(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            let is_product = match map.get("@type") {
                Some(serde_json::Value::String(kind)) => kind == "Product",
                Some(serde_json::Value::Array(kinds)) => kinds
                    .iter()
                    .any(|k| k.as_str().is_some_and(|s| s == "Product")),
                _ => false,
            };
            if is_product {
                out.push(value.clone());
            }
            if let Some(graph) = map.get("@graph") {
                read_products(graph, out);
            }
        }
        _ => {}
    }
}

/// 文本规范化（上游 `_text`）：解码字符引用、折叠空白、去首尾。
pub fn text(parts: &[String]) -> String {
    let joined: String = parts.iter().map(|s| s.as_str()).collect();
    let decoded = decode_entities(&joined);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_heading_and_description_are_collected() {
        let page = DmmPage::parse(
            r#"<html><head><title>ABC-123 - DMM</title></head><body>
               <h1 id="title">日文标题</h1>
               <div class="mg-b20 lh4">这是简介</div>
               <a href="/detail/=/cid=abc123/">详情</a>
               </body></html>"#,
        );
        assert_eq!(text(&page.page_title), "ABC-123 - DMM");
        assert_eq!(text(&page.heading), "日文标题");
        assert_eq!(text(&page.description), "这是简介");
        assert_eq!(page.links, vec!["/detail/=/cid=abc123/"]);
    }

    #[test]
    fn script_content_is_hidden_but_json_ld_is_parsed() {
        let page = DmmPage::parse(
            r#"<html><body><script>var x = 1;</script>
               <script type="application/ld+json">{"@type":"Product","name":"商品名"}</script>
               <p>可见</p></body></html>"#,
        );
        assert!(!page.visible.join("").contains("var x = 1;"));
        assert_eq!(page.products.len(), 1);
        assert_eq!(page.products[0]["name"], "商品名");
    }

    #[test]
    fn html_entities_are_decoded() {
        assert_eq!(text(&["a &amp; b".to_owned()]), "a & b");
        assert_eq!(text(&["&#x65;&#66;".to_owned()]), "eB");
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(text(&["  a\n  b\t c ".to_owned()]), "a b c");
    }

    #[test]
    fn h1_with_item_fn_class_is_a_heading() {
        let page = DmmPage::parse(r#"<h1 class="item fn">标题</h1>"#);
        assert_eq!(text(&page.heading), "标题");
    }
}
