//! 够用的 HTML 扫描器：只做榜单页面需要的那几件事。
//!
//! # 上游用的是 `html.parser.HTMLParser`
//!
//! `minnano.py` 的 `_RankingTableParser` / `_ProductCodeParser` 与
//! `javlibrary.py` 的 `_VideoIdParser` 都继承它，靠 `handle_starttag` /
//! `handle_endtag` / `handle_data` 三个回调攒结果。Rust 侧手写扫描器，行为逐条
//! 对齐 `HTMLParser(convert_charrefs=True)`：
//!
//! - 标签名与属性名**大小写不敏感**（`HTMLParser` 会统一小写化）；
//! - 文本里的字符引用（`&amp;` / `&#123;` / `&#x7f;`）在交给调用方之前解码；
//! - 属性值**不**解码 —— `convert_charrefs` 只作用于文本；
//! - `<!DOCTYPE …>` 与 `<!-- … -->` 不产生事件；
//! - 自闭合写法（`<br/>`）只产生一次 [`Event::Start`]。
//!
//! 与 `plugin-javbus-metadata` 的 `html` 模块同一手法（那份是给 JavBus 详情页
//! 写的，这里给榜单页写一份，不跨仓库依赖）。

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

    /// 属性值。**没给这个属性**是 `None`，给了但为空是 `Some("")`。
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
    rest: &'a str,
}

impl<'a> Iterator for Events<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        loop {
            let rest = self.rest;
            if rest.is_empty() {
                return None;
            }
            let Some(lt) = rest.find('<') else {
                self.rest = "";
                return Some(Event::Text(decode_charrefs(rest)));
            };
            if lt > 0 {
                let (text, after) = rest.split_at(lt);
                self.rest = after;
                // 纯空白文本直接吞掉，上游回调里也是按需取的
                if text.trim().is_empty() {
                    continue;
                }
                return Some(Event::Text(decode_charrefs(text)));
            }
            // rest 以 '<' 开头
            if let Some(end) = rest.find('>') {
                let (tag_text, after) = rest.split_at(end + 1);
                self.rest = after;
                if let Some(event) = parse_tag(tag_text) {
                    return Some(event);
                }
                // 注释 / doctype：不产生事件，继续
                continue;
            }
            // 没有 '>'：剩下的是残缺文本
            self.rest = "";
            return Some(Event::Text(decode_charrefs(rest)));
        }
    }
}

/// 解析 `<...>` 片段；注释与 doctype 返回 `None`。
fn parse_tag<'a>(tag_text: &'a str) -> Option<Event<'a>> {
    debug_assert!(tag_text.starts_with('<') && tag_text.ends_with('>'));
    let inner = &tag_text[1..tag_text.len() - 1];
    let inner = inner.strip_suffix('/').unwrap_or(inner).trim();
    if inner.is_empty() {
        return None;
    }
    // 注释 / doctype / 处理指令
    if inner.starts_with('!') || inner.starts_with('?') {
        return None;
    }
    if let Some(name) = inner.strip_prefix('/') {
        let name = name.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            return None;
        }
        return Some(Event::End(name));
    }
    let (name, attrs) = split_tag_name_attrs(inner);
    if name.is_empty() {
        return None;
    }
    Some(Event::Start(Tag { name, attrs }))
}

/// 切出标签名与属性列表。
fn split_tag_name_attrs(inner: &str) -> (&str, Vec<(&str, &str)>) {
    let mut parts = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    // 标签名
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let name = &inner[..i];
    // 属性
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
        {
            i += 1;
        }
        let attr_name = &inner[start..i];
        if attr_name.is_empty() {
            break;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let vstart = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = &inner[vstart..i];
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let vstart = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'>'
                {
                    i += 1;
                }
                value = &inner[vstart..i];
            }
        }
        parts.push((attr_name, value));
    }
    (name, parts)
}

/// 解码文本中的字符引用（`&amp;` / `&#123;` / `&#x7f;`）。
fn decode_charrefs(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        // 实体不能太长，防止把整段文本吞掉
        if semi > 12 {
            out.push('&');
            rest = after;
            continue;
        }
        let entity = &after[..semi];
        let decoded = if let Some(num) = entity.strip_prefix("&#x").or_else(|| entity.strip_prefix("&#X")) {
            u32::from_str_radix(num, 16).ok().and_then(char::from_u32)
        } else if let Some(num) = entity.strip_prefix("&#") {
            num.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => None,
            }
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_name_case_insensitive() {
        let mut it = events("<DIV CLASS=\"video\"><div class=\"id\">ABC-123</DIV></div>");
        let mut texts = Vec::new();
        while let Some(e) = it.next() {
            match e {
                Event::Start(t) => assert!(t.is("div")),
                Event::Text(t) => texts.push(t.into_owned()),
                Event::End(_) => {}
            }
        }
        assert_eq!(texts, vec!["ABC-123"]);
    }

    #[test]
    fn comments_and_doctype_ignored() {
        let mut it = events("<!DOCTYPE html><!-- hi --><p>x</p>");
        let names: Vec<String> = it
            .filter_map(|e| match e {
                Event::Start(t) => Some(format!("+{}", t.attr("x").unwrap_or(""))),
                Event::End(n) => Some(format!("-{n}")),
                Event::Text(_) => None,
            })
            .collect();
        let _ = names;
        // 只有 p 的开始与结束
        let count = events("<!DOCTYPE html><!-- hi --><p>x</p>")
            .filter(|e| matches!(e, Event::Start(_) | Event::End(_)))
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn charrefs_decoded_in_text_not_attrs() {
        let mut it = events("<a href=\"?x=1&amp;y=2\">a &amp; b</a>");
        let mut href = String::new();
        let mut text = String::new();
        for e in it.by_ref() {
            match e {
                Event::Start(t) => href = t.attr("href").unwrap_or("").to_owned(),
                Event::Text(t) => text.push_str(&t),
                Event::End(_) => {}
            }
        }
        assert_eq!(href, "?x=1&amp;y=2", "属性值不解码");
        assert_eq!(text, "a & b", "文本解码");
    }

    #[test]
    fn self_closing_single_event() {
        let count = events("<br/><br>").count();
        assert_eq!(count, 2);
    }
}
