//! 够用的 HTML 链接提取器：只做 SubtitleCat 需要的两件事。
//!
//! # 上游用的是 `html.parser.HTMLParser`
//!
//! `subtitlecat.py` 里的 `_LinkParser` 继承它，靠 `handle_starttag` /
//! `handle_endtag` 攒出链接。Rust 侧手写扫描器，行为对齐：
//!
//! - 标签名与属性名**大小写不敏感**；
//! - 只关心 `<a>` 标签的 `href`、`id`、`class`；
//! - `<!-- … -->` 不产生事件；
//! - 自闭合写法（`<br/>`）只产生一次开始事件。
//!
//! # 两种过滤模式（与上游 `_LinkParser` 的两个参数对应）
//!
//! - `ancestor_class`：只收「某个 class 的元素内部」的 `<a>`；
//! - `anchor_id`：只收 `id` 等于指定值的 `<a>`。

/// 收集 HTML 中的链接。
///
/// - `ancestor_class`：只收集该 class 元素内的链接（`None` 表示不限制范围）；
/// - `anchor_id`：只收集 `id` 为该值的 `<a>`（`None` 表示不限制）。
///
/// 返回去重后的 `href` 列表（保持首次出现顺序，与上游 `dict.fromkeys` 一致）。
pub fn collect_links(
    html: &str,
    ancestor_class: Option<&str>,
    anchor_id: Option<&str>,
) -> Vec<String> {
    let mut collector = LinkCollector {
        ancestor_class,
        anchor_id,
        scope_depth: 0,
        open_tags: Vec::new(),
        links: Vec::new(),
    };
    collector.feed(html);
    // 去重，保持首次出现顺序。
    let mut seen = std::collections::HashSet::new();
    collector
        .links
        .into_iter()
        .filter(|link| seen.insert(link.clone()))
        .collect()
}

struct LinkCollector<'a> {
    ancestor_class: Option<&'a str>,
    anchor_id: Option<&'a str>,
    scope_depth: usize,
    open_tags: Vec<(String, bool)>,
    links: Vec<String>,
}

impl<'a> LinkCollector<'a> {
    fn feed(&mut self, html: &'a str) {
        let mut rest = html;
        while !rest.is_empty() {
            if let Some(tag_start) = rest.find('<') {
                // 标签前的内容跳过（我们不关心文本）。
                rest = &rest[tag_start..];
            } else {
                break;
            }

            // 注释：跳过。
            if rest.starts_with("<!--") {
                if let Some(end) = rest.find("-->") {
                    rest = &rest[end + 3..];
                } else {
                    break;
                }
                continue;
            }

            // 找标签结束。
            let Some(tag_end) = rest.find('>') else { break };
            let tag_text = &rest[1..tag_end];
            rest = &rest[tag_end + 1..];

            let tag_text = tag_text.trim();
            if tag_text.is_empty() {
                continue;
            }

            // 结束标签。
            if let Some(name) = tag_text.strip_prefix('/') {
                let name = name.trim().to_lowercase();
                self.handle_endtag(&name);
                continue;
            }

            // DOCTYPE 等：跳过。
            if tag_text.starts_with('!') || tag_text.starts_with('?') {
                continue;
            }

            // 自闭合。
            let self_closing = tag_text.ends_with('/');
            let tag_text = tag_text.trim_end_matches('/').trim();

            // 解析标签名和属性。
            let (name, attrs) = parse_tag(tag_text);
            let name_lower = name.to_lowercase();
            self.handle_starttag(&name_lower, &attrs);
            if self_closing {
                self.handle_endtag(&name_lower);
            }
        }
    }

    fn handle_starttag(&mut self, name: &str, attrs: &[(String, String)]) {
        let class_attr = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("class"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let is_scope = self.ancestor_class.is_some_and(|needle| {
            class_attr.split_whitespace().any(|item| item == needle)
        });
        self.open_tags.push((name.to_owned(), is_scope));
        if is_scope {
            self.scope_depth += 1;
        }

        if name != "a" {
            return;
        }
        let in_scope = self.ancestor_class.is_none() || self.scope_depth > 0;
        if !in_scope {
            return;
        }
        if let Some(want_id) = self.anchor_id {
            let id = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("id"))
                .map(|(_, v)| v.as_str());
            if id != Some(want_id) {
                return;
            }
        }
        if let Some(href) = attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("href"))
            .map(|(_, v)| v.clone())
        {
            if !href.is_empty() {
                self.links.push(href);
            }
        }
    }

    fn handle_endtag(&mut self, name: &str) {
        // 从后往前找匹配的开始标签（与上游 `_LinkParser.handle_endtag` 一致）。
        for index in (0..self.open_tags.len()).rev() {
            if self.open_tags[index].0 != name {
                continue;
            }
            let removed: Vec<_> = self.open_tags.drain(index..).collect();
            self.scope_depth -= removed.iter().filter(|(_, is_scope)| *is_scope).count();
            return;
        }
    }
}

/// 解析 `tagname attr="value" attr2='v2' attr3=v3`。
fn parse_tag(text: &str) -> (&str, Vec<(String, String)>) {
    let mut parts = split_tag(text);
    let name = parts.next().unwrap_or("");
    let mut attrs = Vec::new();
    for part in parts {
        if let Some(eq) = part.find('=') {
            let key = part[..eq].trim().to_owned();
            let mut value = part[eq + 1..].trim();
            // 去引号。
            if value.len() >= 2 {
                let first = value.as_bytes()[0];
                let last = value.as_bytes()[value.len() - 1];
                if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                    value = &value[1..value.len() - 1];
                }
            }
            attrs.push((key, value.to_owned()));
        } else if !part.is_empty() {
            attrs.push((part.to_owned(), String::new()));
        }
    }
    (name, attrs)
}

/// 按空白切分标签文本，但引号内的空白不切。
fn split_tag(text: &str) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote: Option<u8> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match in_quote {
            Some(q) => {
                if b == q {
                    in_quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    in_quote = Some(b);
                } else if b.is_ascii_whitespace() {
                    if start < i {
                        parts.push(&text[start..i]);
                    }
                    start = i + 1;
                }
            }
        }
        i += 1;
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_links_within_ancestor_class() {
        let html = r#"
            <div class="other"><a href="/x">x</a></div>
            <div class="subtitles">
                <a href="/d1">d1</a>
                <a href="/d2">d2</a>
            </div>
        "#;
        let links = collect_links(html, Some("subtitles"), None);
        assert_eq!(links, vec!["/d1", "/d2"]);
    }

    #[test]
    fn collects_links_by_anchor_id() {
        let html = r#"
            <a id="other" href="/o">o</a>
            <a id="download_zh-CN" href="/zh1">zh1</a>
            <a id="download_zh-CN" href="/zh1">dup</a>
        "#;
        let links = collect_links(html, None, Some("download_zh-CN"));
        // 去重，保持首次顺序。
        assert_eq!(links, vec!["/zh1"]);
    }

    #[test]
    fn tag_and_attr_names_are_case_insensitive() {
        let html = r#"<DIV CLASS="subtitles"><A HREF="/d">d</A></DIV>"#;
        let links = collect_links(html, Some("subtitles"), None);
        assert_eq!(links, vec!["/d"]);
    }

    #[test]
    fn comments_do_not_produce_links() {
        let html = r#"<!-- <a href="/hidden">x</a> --><a href="/real">r</a>"#;
        let links = collect_links(html, None, None);
        assert_eq!(links, vec!["/real"]);
    }
}
