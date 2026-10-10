//! 番号归并与识别，对应上游 `src/common/movie_numbers.py`。
//!
//! 上游把它放在 `common/` 下，被 `catalog`（影片导入）与 `transfers`
//! （Torznab 候选过滤）共用。本仓库暂时放在 `sm-service` 顶层；等 `catalog`
//! 的影片导入落地时若需要跨 crate 复用，再提到 `sm-core`。
//!
//! # 两个函数口径不同，不能互相替代
//!
//! | 函数 | 用途 | 纯数字番号的处理 |
//! |---|---|---|
//! | [`normalize_movie_number`] | **匹配键**（查库、比相等） | **保留分隔符** |
//! | [`parse_movie_number_from_text`] | 从自由文本**识别**番号 | 由正则捕获组决定 |
//!
//! 纯数字番号（`123-456`）里分隔符本身是片商标识（一本道 `_` / 加勒比 `-`，
//! 同日番号是两部不同影片），所以归一**绝不**把 `_` 折成 `-`。
//!
//! # 识别是启发式，输出是「查找键」而非规范值
//!
//! 正则可能截断超长前缀、吃掉边缘字符 —— 规范形态只来自 provider（JavDB）。
//! 所以调用方拿它**比相等**是安全的（两边都过同一套规则），但不要拿它落库。

use std::sync::LazyLock;

use regex::Regex;

/// `(正则, 捕获组拼接符)`。按顺序取**第一条**命中的规则。
///
/// 三处 `(?<!\.)` 的负向后顾在 Rust `regex` 里不支持（它不做回退），改成
/// `(?:^|[^.])` —— 它会**多消费**一个前缀字符，但**捕获组不变**，而调用方
/// 只读捕获组，所以结果等价。
///
/// 逐条核对过的等价性：`ab00123.def00456` 取第一条命中的 `00` 规则，
/// Python 与 Rust 都得到 `("ab", "123")`。
static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let raw: [(&str, &str); 20] = [
        (r"(?i)(DSVR)0(\d{3,4})", "-"),
        (r"(?i)(XXX)-(AV)-(\d+)", "-"),
        (r"(?i)(N\d{4})", "-"),
        (r"(?i)(LAFB?D?)-(\d+)", "-"),
        (r"(?i)(MISM)-(\d+)", "-"),
        (r"(?i)(MKB?D?)-(S\d+)", "-"),
        (r"(?i)(S2MB?D?)-(\d+)", "-"),
        (r"(?i)(CWPB?D?)-(\d+)", "-"),
        (r"(?i)(SMB?D?)-(\d+)", "-"),
        (r"(?i)(MCDV)-(\d+)", "-"),
        // 素人系数字番号：分隔符原样保留，拼接符是空串。
        (r"(?i)(\d{6})([-_])(\d{3})", ""),
        (r"(?i)(FC2)PPV_(\d+)", "-"),
        (r"(?i)(FC2)PPV-(\d+)", "-"),
        (r"(?i)(FC2)-PPV-(\d+)", "-"),
        (r"(?i)(FC2)-(\d+)", "-"),
        (r"(?i)9([a-zA-Z]{3,5})(\d{2,3})", "-"),
        // 原 `(?<!\.)([a-zA-Z]{2,6})00(\d{3})` 等三条，见本常量文档。
        (r"(?i)(?:^|[^.])([a-zA-Z]{2,6})00(\d{3})", "-"),
        (r"(?i)(?:^|[^.])([a-zA-Z]{2,6})-(\d{3,5})", "-"),
        (r"(?i)(?:^|[^.])([a-zA-Z]{2,6})(\d{3,5})", "-"),
        (r"(?i)([a-zA-Z]{3,5}) (\d{2,6})", "-"),
    ];
    raw.into_iter()
        .map(|(pattern, joiner)| (Regex::new(pattern).expect("手写正则应编译通过"), joiner))
        .collect()
});

/// 域名清理：`remove_disturb`。
///
/// 种子标题里常带发布站域名（`xxx.com`），不清掉会让后面的正则把域名片段
/// 当成番号前缀。
static DOMAIN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[a-zA-Z0-9]+\.)*[a-zA-Z0-9]+\.(?:com|cn|net|org|gov|edu)\b")
        .expect("手写正则应编译通过")
});

/// 番号**匹配键**。对应上游 `normalize_movie_number`。
///
/// 仅用于匹配，不用于落库改写：纯数字番号保留分隔符，其余番号把 `_` 折成 `-`
/// 并去掉 `PPV-`。
pub fn normalize_movie_number(value: &str) -> String {
    let normalized = value.trim().to_uppercase().replace(' ', "");
    if is_pure_numeric_number(&normalized) {
        return normalized;
    }
    normalized.replace('_', "-").replace("PPV-", "")
}

/// `^\d+[-_]\d+$`。
fn is_pure_numeric_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == 0 || index >= bytes.len() {
        return false;
    }
    if bytes[index] != b'-' && bytes[index] != b'_' {
        return false;
    }
    index += 1;
    if index >= bytes.len() {
        return false;
    }
    bytes[index..].iter().all(u8::is_ascii_digit)
}

/// 人工输入的**等值候选集**。对应上游 `movie_number_lookup_values`。
///
/// 调用方按**顺序**逐个点查，命中即返回 —— 顺序就是优先级。
///
/// # 纯数字番号不互换分隔符
///
/// `123-456` 与 `123_456` 是**两部不同的影片**（一本道用 `_`、加勒比用 `-`，
/// 同日番号是两个片商各发一部）。互换会把「查这部」变成「查另一部」，
/// 而这种错不会被任何报错拦下 —— 它只是安静地返回了另一部影片。
///
/// 其余番号才试互换：`ABC_123` 与 `ABC-123` 是同一个番号的两种写法。
pub fn movie_number_lookup_values(value: &str) -> Vec<String> {
    let stripped = value.trim().to_uppercase();
    if stripped.is_empty() {
        return Vec::new();
    }
    if is_pure_numeric_number(&stripped) {
        return vec![stripped];
    }
    let mut candidates = vec![stripped.clone()];
    for swapped in [stripped.replace('_', "-"), stripped.replace('-', "_")] {
        // 不含分隔符时两次 replace 得到同一个串，去重后只留原形。
        if !candidates.contains(&swapped) {
            candidates.push(swapped);
        }
    }
    candidates
}

/// 从自由文本里**识别**番号，识别不出返回空串。
///
/// 对应上游 `parse_movie_number_from_text`。输出是**查找键**，不是规范值
/// （见模块文档）。
pub fn parse_movie_number_from_text(value: &str) -> String {
    let cleaned = DOMAIN_PATTERN.replace_all(value, "");
    for (pattern, joiner) in PATTERNS.iter() {
        if let Some(captures) = pattern.captures(&cleaned) {
            let parts: Vec<String> = captures
                .iter()
                .skip(1)
                .map(|group| {
                    // 本表里的捕获组全部是必配的，`None` 不可达；真出现时
                    // 按空串处理而不是 panic —— 这是**启发式**，不该让一个
                    // 意外标题把整个列表打挂。
                    group.map_or_else(String::new, |m| m.as_str().to_uppercase())
                })
                .collect();
            return parts.join(joiner);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_numeric_numbers_keep_their_separator() {
        assert_eq!(normalize_movie_number("123-456"), "123-456");
        assert_eq!(normalize_movie_number("123_456"), "123_456");
        assert_eq!(normalize_movie_number("  123-456  "), "123-456");
    }

    #[test]
    fn other_numbers_fold_underscore_and_drop_ppv() {
        assert_eq!(normalize_movie_number("abc_123"), "ABC-123");
        assert_eq!(normalize_movie_number("FC2-PPV-1234567"), "FC2-1234567");
        assert_eq!(normalize_movie_number("ab 123"), "AB123");
    }

    #[test]
    fn parses_the_prefixed_and_suffixed_families() {
        assert_eq!(parse_movie_number_from_text("ABC-123 第 1 集"), "ABC-123");
        assert_eq!(parse_movie_number_from_text("(N1234) 作品"), "N1234");
        assert_eq!(parse_movie_number_from_text("MISM-123"), "MISM-123");
        assert_eq!(
            parse_movie_number_from_text("FC2-PPV-1234567"),
            "FC2-1234567"
        );
        // `(DSVR)0(\d{3,4})` 里那个 `0` 在捕获组**外**，所以它被吃掉而不是保留。
        assert_eq!(parse_movie_number_from_text("DSVR0123"), "DSVR-123");
    }

    #[test]
    fn the_numeric_family_keeps_its_separator() {
        // 素人系：拼接符是空串，所以 `123456-789` 原样保留。
        assert_eq!(parse_movie_number_from_text("123456-789"), "123456-789");
        assert_eq!(parse_movie_number_from_text("123456_789"), "123456_789");
    }

    #[test]
    fn domains_are_removed_before_matching() {
        // 不清域名的话，`xxx.com` 里的 `xxx` + `com` 会被当成番号。
        assert_eq!(
            parse_movie_number_from_text("abc-123 released by foo.com"),
            "ABC-123"
        );
    }

    #[test]
    fn the_lookbehind_rewrite_still_skips_dotted_prefixes() {
        // `foo.abc123`：紧邻点号的那个字母不能被算进番号前缀，所以是 `BC-123`
        // 而不是 `ABC-123`。
        //
        // 这条同时钉住 `(?<!\.)` → `(?:^|[^.])` 的**等价性**：后者会多消费一个
        // 前缀字符，但因为调用方只读捕获组，结果与 Python 一致。
        assert_eq!(parse_movie_number_from_text("foo.abc123"), "BC-123");
        // 没有点号时前缀完整保留，作为对照。
        assert_eq!(parse_movie_number_from_text("fooabc123"), "FOOABC-123");
    }

    #[test]
    fn free_text_without_a_number_yields_empty() {
        assert_eq!(parse_movie_number_from_text(""), "");
        assert_eq!(parse_movie_number_from_text("完全无关的标题"), "");
    }

    #[test]
    fn lookup_values_try_the_original_form_first() {
        assert_eq!(
            movie_number_lookup_values("  abc_123 "),
            vec!["ABC_123", "ABC-123"],
            "顺序是契约：原形 -> 下划线折横线 -> 横线折下划线"
        );
        assert_eq!(
            movie_number_lookup_values("abc-123"),
            vec!["ABC-123", "ABC_123"]
        );
    }

    /// 纯数字番号**只给原形** —— 互换会把查询指向另一部影片。
    #[test]
    fn lookup_values_keep_pure_numeric_separators_verbatim() {
        assert_eq!(movie_number_lookup_values("123-456"), vec!["123-456"]);
        assert_eq!(movie_number_lookup_values("123_456"), vec!["123_456"]);
    }

    #[test]
    fn lookup_values_without_a_separator_collapse_to_one() {
        assert_eq!(movie_number_lookup_values("SSNI888"), vec!["SSNI888"]);
        assert_eq!(movie_number_lookup_values("  "), Vec::<String>::new());
    }
}
