//! 检索词切分，对应上游 `src/common/text_search.py:12`。
//!
//! # 放在 `sm-core` 而不是 `sm-service`
//!
//! 上游它在 `common/` 下，播放域的片段列表与时刻点列表都要用。Rust 侧同理：
//! 任何接受 `?keyword=` 的列表端点都要先切词，而那些端点分属不同 service 模块。
//! 放 `sm-service` 的某个域里会让另一个域反向依赖它。
//!
//! # 三条规则
//!
//! 1. 按空白切成若干词；空输入返回**空列表**（= 不加条件，不是「匹配不到」）；
//! 2. 词数超过 [`SEARCH_TERM_MAX_COUNT`] 或词长超过 [`SEARCH_TERM_MAX_LENGTH`]
//!    报 [`TermLimitError`]，由调用方转成自己域里的 422 错误码；
//! 3. **去重但保持输入顺序**。

/// 单个检索词的长度上限（**字符**数，见下）。
pub const SEARCH_TERM_MAX_LENGTH: usize = 64;

/// 检索词个数上限。
pub const SEARCH_TERM_MAX_COUNT: usize = 6;

/// 切分结果越界。调用方把它映射到本域的错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermLimitError {
    /// 词数超过 [`SEARCH_TERM_MAX_COUNT`]。
    TooManyTerms { count: usize },
    /// 某个词超过 [`SEARCH_TERM_MAX_LENGTH`] 个字符。
    TermTooLong { length: usize },
}

impl TermLimitError {
    /// 哪个词超长 —— 用于回显细节。
    ///
    /// 长度**不是**错误的一部分：超长的那个词可能是第 4 个，而客户端需要
    /// 知道的是「你多打了一个 70 字的词」，不是「有一个词是 70 字」。
    #[must_use]
    pub fn offending_length(self) -> usize {
        match self {
            Self::TooManyTerms { count } => count,
            Self::TermTooLong { length } => length,
        }
    }

    /// 供错误 details 使用的一句话说明。
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooManyTerms { .. } => "搜索关键词过多",
            Self::TermTooLong { .. } => "搜索关键词过长",
        }
    }
}

/// 把搜索输入按空白拆成检索词。
///
/// # `None` 与空白输入都返回空列表
///
/// 上游 `if not terms: return []` 之后 `keyword_conditions` 收不到任何条件，
/// 于是**不加过滤**。这是「不过滤」而不是「匹配不到」—— 客户端不填关键词时
/// 看到空列表才是 bug。
///
/// # 去重保持顺序
///
/// 上游注释写了理由：重复词会让相关度分数翻倍。这里是子串匹配、不打分，
/// 但去重仍然保留 —— 它是上游行为，且能省掉一次重复的 `stat`（见
/// `sm_service::playback::clip_artifact`，每次列表请求都按词数产生条件）。
///
/// # 长度按**字符**算，不是字节
///
/// 上游是 Python 的 `len(term)`，即字符数。若按字节算，一个 30 字的中文
/// 关键词是 90 字节，会被判「超长」而 422 —— 而上游允许它。`term.chars().count()`
/// 才是对应的写法，这条有测试钉住。
///
/// # 空白定义与 Python 的 `str.split()` 有一处极小差异
///
/// 两者都把连续空白当作单个分隔符、都丢弃空段。差异只在 Unicode 空白字符
/// 的集合上（Python 用 `str.isspace()`，Rust 用 `char::is_whitespace()`），
/// 落在 U+2000..U+200A 与 U+0085 这几个冷僻字符上。这些字符不会出现在
/// 浏览器提交的查询串里，所以照 `split_whitespace()` 走。
pub fn split_search_terms(value: Option<&str>) -> Result<Vec<String>, TermLimitError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    // `split_whitespace` 丢弃空段并把连续空白折叠，与上游一致。
    let terms: Vec<String> = value.split_whitespace().map(str::to_owned).collect();

    if terms.is_empty() {
        return Ok(Vec::new());
    }
    if terms.len() > SEARCH_TERM_MAX_COUNT {
        return Err(TermLimitError::TooManyTerms { count: terms.len() });
    }
    for term in &terms {
        // 字符数，不是字节数。
        let length = term.chars().count();
        if length > SEARCH_TERM_MAX_LENGTH {
            return Err(TermLimitError::TermTooLong { length });
        }
    }

    // 去重且保持顺序：靠 `HashSet` 判重，但顺序由 `Vec` 决定。
    let mut seen = std::collections::HashSet::with_capacity(terms.len());
    Ok(terms
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_blank_input_means_no_filtering() {
        for input in [None, Some(""), Some("   "), Some("\t\n  \r")] {
            assert_eq!(
                split_search_terms(input).expect("不该报错"),
                Vec::<String>::new(),
                "{input:?} 应切成空列表"
            );
        }
    }

    #[test]
    fn terms_are_split_on_whitespace_runs() {
        // 混合空格 / 制表 / 换行，且连续出现 —— 每一段连续空白只算一个分隔符。
        assert_eq!(
            split_search_terms(Some("  abc-123 \t\n  FC2-456\t\t789  ")).expect("不该报错"),
            vec!["abc-123", "FC2-456", "789"]
        );
    }

    /// 去重**保持首次出现的顺序** —— 上游用 `dict.fromkeys` 就是这个语义。
    #[test]
    fn duplicates_are_removed_keeping_first_occurrence_order() {
        assert_eq!(
            split_search_terms(Some("b a b c a")).expect("不该报错"),
            vec!["b", "a", "c"],
            "顺序按首次出现，且重复词只留一个"
        );
    }

    /// 边界：恰好等于上限必须**通过**。
    ///
    /// `>` 而非 `>=` 是上游的写法；写成 `>=` 会让「正好 6 个词」被拒，
    /// 而客户端无法从错误里看出边界在哪。
    #[test]
    fn the_limits_are_inclusive_at_the_boundary() {
        let six: Vec<String> = (0..SEARCH_TERM_MAX_COUNT)
            .map(|i| format!("a{i}"))
            .collect();
        let joined = six.join(" ");
        assert_eq!(
            split_search_terms(Some(&joined))
                .expect("恰好 6 个词应通过")
                .len(),
            SEARCH_TERM_MAX_COUNT
        );

        let seven: Vec<String> = (0..SEARCH_TERM_MAX_COUNT + 1)
            .map(|i| format!("a{i}"))
            .collect();
        assert_eq!(
            split_search_terms(Some(&seven.join(" "))),
            Err(TermLimitError::TooManyTerms { count: 7 })
        );

        let sixty_four = "x".repeat(SEARCH_TERM_MAX_LENGTH);
        assert!(
            split_search_terms(Some(&sixty_four)).is_ok(),
            "恰好 64 字应通过"
        );
        let sixty_five = "x".repeat(SEARCH_TERM_MAX_LENGTH + 1);
        assert_eq!(
            split_search_terms(Some(&sixty_five)),
            Err(TermLimitError::TermTooLong { length: 65 })
        );
    }

    /// 长度按**字符**算：30 个汉字 = 90 字节，但上游允许。
    ///
    /// 若误用 `term.len()`（字节），这个关键词会 422 —— 而它只有 30 个字符。
    #[test]
    fn length_is_counted_in_characters_not_bytes() {
        let thirty_cjk = "字".repeat(30);
        assert_eq!(thirty_cjk.len(), 90, "前提：90 字节");
        assert!(
            split_search_terms(Some(&thirty_cjk)).is_ok(),
            "30 个汉字远未超限 —— 按字节算会误判为超长"
        );

        let sixty_five_cjk = "字".repeat(65);
        assert_eq!(
            split_search_terms(Some(&sixty_five_cjk)),
            Err(TermLimitError::TermTooLong { length: 65 }),
            "65 个汉字确实超限，且报的是字符数 65 而不是字节数 195"
        );
    }

    /// 词数超限优先于词长超限 —— 上游先查数量。
    #[test]
    fn the_count_check_runs_before_the_length_check() {
        // 7 个词，每个都超长：上游先报数量
        let input = vec!["x".repeat(100); 7].join(" ");
        assert_eq!(
            split_search_terms(Some(&input)),
            Err(TermLimitError::TooManyTerms { count: 7 })
        );
    }
}
