//! 够用的 HTML 扫描器：只做资料页解析需要的那几件事。
//!
//! # 上游用的是 `bs4.BeautifulSoup`
//!
//! `sources.py` 里的 `parse_minnanoav` 用 CSS 选择器（`div.act-profile` /
//! `h1` / `tr > td > span` + `p`）攒出名字、资料表行。Rust 侧没有等价物可依赖：
//! `scraper` / `html5ever` 会引入重型依赖，而测试走本地假服务、
//! 不需要完整的 HTML5 树构建。
//!
//! 资料页只用到「标签名 + `class` + `href` + 文本」，所以这里手写扫描器，
//! 行为逐条对齐 `HTMLParser(convert_charrefs=True)`：
//!
//! - 标签名与属性名**大小写不敏感**（`HTMLParser` 会统一小写化）；
//! - 文本里的字符引用（`&amp;` / `&#123;` / `&#x7f;`）在交给调用方之前解码；
//! - 属性值**不**解码 —— `convert_charrefs` 只作用于文本；
//! - `<!DOCTYPE …>` 与 `<!-- … -->` 不产生事件；
//! - 自闭合写法（`<br/>`）只产生一次 [`Event::Start`]。
//!
//! # 刻意不做的部分
//!
//! 完整的 HTML5 树构建与 CSS 选择器：上游拿到的也**不是**一棵规范的树 ——
//! `BeautifulSoup` 的 `select_one` 在这里只做「按标签名 + class 找第一个」与
//! 「直接子元素」两件事，调用方（`sources::parse_minnanoav`）用事件流的状态机
//! 复刻了同样的遍历。少做这些反而更接近上游的行为。

use std::borrow::Cow;

/// 一个开始标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> Tag<'a> {
    /// 标签名比较（HTML 的标签名大小写不敏感）。
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// 标签名原文（调用方自行做大小写不敏感比较）。
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// 属性值。**没给这个属性**是 `None`，给了但为空是 `Some("")` —— 上游
    /// `attributes.get(key)` 也是这个区分，调用方要空字符串就 `unwrap_or("")`。
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| *value)
    }

    /// `class` 属性里有没有这一项（按空白分词后精确相等）。
    pub fn has_class(&self, needle: &str) -> bool {
        self.attr("class")
            .is_some_and(|value| value.split_whitespace().any(|item| item == needle))
    }
}

/// 扫描器吐出的三件事。
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    Start(Tag<'a>),
    End(&'a str),
    /// 已解码字符引用的文本。没有 `&` 时是切片，不分配。
    Text(Cow<'a, str>),
}

/// 扫描一遍，产出事件序列。
pub fn events(html: &str) -> Events<'_> {
    Events { rest: html }
}

/// [`events`] 返回的迭代器。
pub struct Events<'a> {
    /// 还没扫的那一段。
    rest: &'a str,
}

impl<'a> Iterator for Events<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        loop {
            let source: &'a str = self.rest;
            if source.is_empty() {
                return None;
            }
            match source.find('<') {
                // 没有标签了：剩下的整段都是文本。
                None => {
                    self.rest = "";
                    return Some(Event::Text(decode(source)));
                }
                Some(0) => {}
                Some(index) => {
                    self.rest = &source[index..];
                    return Some(Event::Text(decode(&source[..index])));
                }
            }
            // 走到这里 `source` 必以 `<` 开头。
            if let Some(tail) = source.strip_prefix("<!--") {
                // 注释：找到 `-->` 为止；找不到就吞掉余下全部。
                self.rest = match tail.find("-->") {
                    Some(index) => &tail[index + 3..],
                    None => "",
                };
            } else if let Some(tail) = source.strip_prefix("<!") {
                // 声明（`<!DOCTYPE html>`）：没有结束标签，找到 `>` 为止。
                self.rest = after(tail, '>');
            } else if let Some(tail) = source.strip_prefix("</") {
                let end = tail.find('>').unwrap_or(tail.len());
                let name = tail[..end].trim();
                self.rest = after(tail, '>');
                if !name.is_empty() {
                    return Some(Event::End(name));
                }
            } else {
                return Some(self.start_tag(source));
            }
        }
    }
}

impl<'a> Events<'a> {
    /// 解析一个开始标签。**调用前 `source` 必以 `<` 开头。**
    fn start_tag(&mut self, source: &'a str) -> Event<'a> {
        let body = &source[1..];
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(body.len());
        let name = &body[..name_end];
        let mut attrs = Vec::new();
        let mut rest = &body[name_end..];

        loop {
            rest = rest.trim_start();
            match rest.as_bytes().first() {
                None => {
                    self.rest = "";
                    break;
                }
                Some(b'>') => {
                    self.rest = &rest[1..];
                    break;
                }
                // 自闭合：`<br/>` 只给一次 `Start`，不给 `End`。
                Some(b'/') => {
                    self.rest = after(rest, '>');
                    break;
                }
                _ => {}
            }

            let key_end = rest
                .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
                .unwrap_or(rest.len());
            let key = &rest[..key_end];
            rest = rest[key_end..].trim_start();

            let value = match rest.strip_prefix('=') {
                None => "",
                Some(after_eq) => {
                    let after_eq = after_eq.trim_start();
                    match after_eq.as_bytes().first() {
                        Some(b'"') | Some(b'\'') => {
                            let quote = after_eq.as_bytes()[0] as char;
                            let inner = &after_eq[1..];
                            let end = inner.find(quote).unwrap_or(inner.len());
                            rest = inner.get(end + 1..).unwrap_or("");
                            &inner[..end]
                        }
                        // 无引号的值：到空白或 `>` 为止。
                        _ => {
                            let end = after_eq
                                .find(|c: char| c.is_whitespace() || c == '>')
                                .unwrap_or(after_eq.len());
                            rest = &after_eq[end..];
                            &after_eq[..end]
                        }
                    }
                }
            };

            if !key.is_empty() {
                attrs.push((key, value));
            }
        }

        Event::Start(Tag { name, attrs })
    }
}

/// `delimiter` 之后的那一段；没有它就把余下全部吞掉。
fn after(text: &str, delimiter: char) -> &str {
    match text.find(delimiter) {
        Some(index) => text.get(index + delimiter.len_utf8()..).unwrap_or(""),
        None => "",
    }
}

/// 解码 HTML 字符引用。没有 `&` 时原样返回切片。
fn decode(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        match charref(tail) {
            Some((ch, consumed)) => {
                out.push(ch);
                rest = &tail[consumed..];
            }
            // 不成引用：`&` 原样留下（`HTMLParser` 的做法）。
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// 解析 `amp;` / `#20329;` / `#x30AB;` 这类引用，返回字符与**消耗掉的字节数**
/// （含结尾的分号）。
fn charref(tail: &str) -> Option<(char, usize)> {
    let end = tail.find(';')?;
    let body = &tail[..end];
    if let Some(digits) = body.strip_prefix('#') {
        let (digits, radix) = match digits
            .strip_prefix('x')
            .or_else(|| digits.strip_prefix('X'))
        {
            Some(hex) => (hex, 16),
            None => (digits, 10),
        };
        let code = u32::from_str_radix(digits, radix).ok()?;
        return Some((char::from_u32(code)?, end + 1));
    }
    let ch = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    };
    Some((ch, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(html: &str) -> Vec<Event<'_>> {
        events(html).collect()
    }

    fn start(html: &str) -> Tag<'_> {
        match events(html).next() {
            Some(Event::Start(tag)) => tag,
            other => panic!("应当是开始标签：{other:?}"),
        }
    }

    #[test]
    fn a_start_tag_carries_its_name_and_attributes() {
        let tag = start(r#"<a class="bigImage" href="/pics/cover/1.jpg">"#);
        assert!(tag.is("A"), "标签名大小写不敏感");
        assert!(tag.has_class("bigImage"));
        assert_eq!(tag.attr("HREF"), Some("/pics/cover/1.jpg"));
        assert_eq!(tag.attr("title"), None, "没给的属性是 None");
    }

    #[test]
    fn classes_are_matched_whole_not_by_substring() {
        // `class="glyphicon glyphicon-plus"` 里不该有 "header" 或 "genre"。
        let tag = start(r#"<span id="x" class="glyphicon glyphicon-plus">"#);
        assert!(!tag.has_class("header"));
        assert!(!tag.has_class("genre"));
        assert!(tag.has_class("glyphicon-plus"));
    }

    #[test]
    fn single_quoted_and_unquoted_values_are_read() {
        let tag = start("<a href='/pics/1.jpg' class=bigImage>");
        assert_eq!(tag.attr("href"), Some("/pics/1.jpg"));
        assert!(tag.has_class("bigImage"));
    }

    #[test]
    fn a_value_may_contain_the_other_quote_and_the_delimiter() {
        // `onmouseover="hoverdiv(event,'star_2xi')"`：单引号在双引号里。
        let tag = start(r#"<span class="genre" onmouseover="hoverdiv(event,'x')">"#);
        assert!(tag.has_class("genre"));
        assert_eq!(tag.attr("onmouseover"), Some("hoverdiv(event,'x')"));
    }

    #[test]
    fn declarations_comments_and_self_closing_tags_produce_no_noise() {
        assert_eq!(scan("<!DOCTYPE html>"), vec![]);
        assert_eq!(
            scan("<!-- 说明 --><p>"),
            vec![Event::Start(start("<p>"))],
            "注释不产生事件"
        );
        // 自闭合：一次 Start，没有配套的 End。
        let events = scan("<input type='checkbox'/><b>");
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[0], Event::Start(_)));
        assert!(matches!(events[1], Event::Start(_)));
    }

    #[test]
    fn text_is_split_at_tags_and_charrefs_are_decoded() {
        assert_eq!(
            scan("<h3>SSIS-001 &amp; 続編</h3>"),
            vec![
                Event::Start(start("<h3>")),
                Event::Text(Cow::Borrowed("SSIS-001 & 続編")),
                Event::End("h3"),
            ]
        );
    }

    #[test]
    fn numeric_charrefs_are_decoded_and_bad_ones_are_left_alone() {
        assert_eq!(decode("&#20329;"), "佩", "十进制");
        assert_eq!(decode("&#x30AB;"), "カ");
        assert_eq!(decode("a & b"), "a & b", "不成引用就原样留下");
        assert_eq!(decode("没有任何引用"), "没有任何引用");
    }

    #[test]
    fn a_trailing_text_run_without_tags_is_emitted() {
        assert_eq!(
            scan("片尾文字"),
            vec![Event::Text(Cow::Borrowed("片尾文字"))]
        );
    }
}
