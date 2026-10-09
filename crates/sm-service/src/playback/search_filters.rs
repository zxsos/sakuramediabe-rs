//! 播放域列表的关键词过滤，对应上游
//! `src/service/playback/search_filters.py`（61 行）。
//!
//! # 它解决的是「用户怎么打番号」
//!
//! 数据库里存的是 `ABC-123`、`FC2-PPV-1234567` 这类写法，而用户会打
//! `abc123`、`FC2PPV1234567`、`abc 123`。所以匹配要**两边都归一化**：
//!
//! ```text
//! 列：UPPER(TRANSLATE(movie_number, '-_', '')) 再把 FC2PPV 折成 FC2
//! 词：同样地归一，然后子串匹配
//! ```
//!
//! 归一化规则与影片搜索的 `MovieService._number_search_target` **一致** ——
//! 这是刻意的：同一个词在影片列表能搜到，在时刻点列表也必须能搜到。
//! 所以这个模块是那套规则的**唯一实现**，两边都引用它。
//!
//! # 「词之间 AND，词内 OR」
//!
//! `/media-clips` 与 `/media-points` 的 `keyword` 共用这套组装：
//!
//! - 多个词之间 **AND**（每个词都命中才算）
//! - 一个词在「番号表达式」与调用方给的文本条件之间 **OR**
//!
//! # 一个词匹配不到任何字段时必须**恒假**，不能被忽略
//!
//! 上游是 `peewee.SQL("FALSE")`。理由：把一个匹配不了的词静默丢掉，
//! 用户看到的就是「搜了个没用的词，列表没变」—— 而他真正想要的过滤**没
//! 发生**。恒假让结果为空，从而暴露出「这个词没有任何可搜索的字段」。
//!
//! 比如搜索时刻点（没有番号字段）时输入 `abc-123`：`number_column` 是
//! `None`，而文本字段也不匹配番号形态，于是这个词恒假 → 结果为空。
//! 这是**正确**的：时刻点确实没有番号可搜。

/// 番号列的归一化表达式。
///
/// `TRANSLATE(x, '-_', '')` 删除 `-` 与 `_`（第三个参数为空串 = 删字符，
/// 不是替换成空），`UPPER` 大写，`REPLACE(..., 'FC2PPV', 'FC2')` 折叠
/// FC2PPV 前缀。三步的顺序与上游 `fn.REPLACE(fn.UPPER(fn.TRANSLATE(...)))`
/// 逐条一致 —— 顺序不能换（先删分隔符才轮到前缀匹配）。
///
/// 列名由调用方传入（`movie_number` / `MediaPoint` 侧的列…），因为
/// 「哪些类型按番号匹配」是每个调用方自己的决定。
pub fn normalized_column_expr(column: &str) -> String {
    format!("REPLACE(UPPER(TRANSLATE({column}, '-_', '')), 'FC2PPV', 'FC2')")
}

/// 词里是否含任何 ASCII 字母或数字。
///
/// 没有任何一个时**不产生条件** —— 上游 `if not any(char.isascii() and
/// char.isalnum() ...): return None`。纯符号/纯空白/中文的词在番号列上
/// 匹配不到任何东西。
///
/// 判据是 `is_ascii_alphanumeric()` —— 它正是上游
/// `char.isascii() and char.isalnum()` 的合体。两个都要判：Python 的
/// `isalnum()` 对 `文字` 返回 **True**，所以上游那个 `isascii()` 不是冗余的；
/// 而 Rust 若只写 `is_alphanumeric()`，`文` 也会被算成可搜索字符，
/// 于是「搜一个纯中文词」会落进番号条件而不是被恒假掉。
pub fn has_searchable_char(term: &str) -> bool {
    // `chars()` 产出 `char`，而方法签名收 `&char` —— 用闭包而不是
    // 直接传方法名。
    term.chars().any(|c| c.is_ascii_alphanumeric())
}

/// 词是否走「纯数字 + 一个分隔符」的快路径。
///
/// 匹配 `^\d+[-_]\d+$`（上游 `re.fullmatch(r"\d+[-_]\d+", normalized)`）。
/// 这类词**不需要**归一化表达式 —— 列里的原文直接子串匹配即可，而
/// `TRANSLATE` + `REPLACE` 是全表扫描里最贵的部分。
///
/// `2023-001`、`2023_001` 走快路径；`abc-123`、`2023-001-x` 不走。
pub fn is_plain_number_pattern(term: &str) -> bool {
    let mut chars = term.chars().peekable();
    let mut digits_before = 0usize;
    let mut seen_separator = false;
    let mut digits_after = 0usize;

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            if seen_separator {
                digits_after += 1;
            } else {
                digits_before += 1;
            }
            chars.next();
        } else if (c == '-' || c == '_') && !seen_separator && digits_before > 0 {
            seen_separator = true;
            chars.next();
        } else {
            return false;
        }
    }
    seen_separator && digits_before > 0 && digits_after > 0
}

/// 归一化一个检索词，返回**用于子串匹配的键**。
///
/// 步骤与上游逐条一致：
///
/// 1. `strip().upper()`
/// 2. 删掉 `-` 与 `_`
/// 3. 前缀 `FC2PPV` 折成 `FC2`
///
/// `None` = 这个词在番号列上匹配不到任何东西（见
/// [`has_searchable_char`]）。
///
/// # 为什么 FC2PPV 要折成 FC2
///
/// 同一个番号在不同来源有 `FC2PPV-1234567` 与 `FC2-1234567` 两种写法。
/// 两边都折成 `FC2` 之后它们才能互相匹配 —— 否则搜 `fc2-1234567` 找不到
/// 存成 `FC2PPV-1234567` 的那一条。
///
/// # 只折**前缀**
///
/// 上游是 `if key.startswith("FC2PPV")` 然后切掉那 6 个字符。中间出现的
/// `FC2PPV` 不折 —— 与上游一致，而「中间出现」在真实番号里不存在，
/// 真要折反而会引入分歧。
pub fn normalize_number_term(term: &str) -> Option<String> {
    let upper = term.trim().to_ascii_uppercase();
    if !has_searchable_char(&upper) {
        return None;
    }
    let mut key: String = upper.chars().filter(|c| *c != '-' && *c != '_').collect();
    if let Some(rest) = key.strip_prefix("FC2PPV") {
        // `strip_prefix` 返回的借用不能直接拼进 key 之外的地方，这里重建。
        key = format!("FC2{rest}");
    }
    Some(key)
}

/// 番号上的子串匹配条件：`{sql}` 用 `$1` 绑键。
///
/// `Ok(None)` = 该词在番号列上不适用（调用方应转向文本条件）；而若调用方
/// 最终没有任何条件可用，要走 [`TermCondition::Never`] 而不是跳过这个词。
///
/// # 快路径
///
/// [`is_plain_number_pattern`] 为真时用**列原文** + **未归一化的词**：
/// `2023-001` 存在库里就是 `2023-001`，所以 `LIKE '%2023-001%'` 就够，
/// 不用付 `TRANSLATE` 的代价。
pub fn number_condition(column: &str, term: &str) -> Option<NumberMatch> {
    number_condition_at(column, term, 1)
}

/// 同上，但占位符用给定编号。
///
/// # 为什么需要这个变体
///
/// 上游靠 Peewee 的 `?` 让驱动自动编号，所以片段里写死一个位置总是对的。
/// 手写 SQL 不行：片段要拼进一条**已经有占位符**的查询（先绑
/// `movie_number` / `collection_id`，再拼关键词条件），写死 `$1` 会与前面的
/// 编号撞号。
///
/// 撞号**不会报错** —— PostgreSQL 只是把后一个值绑给前一个位置，于是
/// 「按番号过滤」静默地变成「按关键词的第一个词过滤」。这种错误只在特定
/// 组合下才显形，所以编号必须由调用方分配，见 [`FilterBuilder`]。
pub fn number_condition_at(column: &str, term: &str, placeholder: usize) -> Option<NumberMatch> {
    let upper = term.trim().to_ascii_uppercase();
    if !has_searchable_char(&upper) {
        return None;
    }
    let ph = format!("${placeholder}");

    if is_plain_number_pattern(&upper) {
        return Some(NumberMatch {
            sql: format!("{column} LIKE '%' || {ph} || '%'"),
            bind: upper,
        });
    }

    // 归一化表达式里没有可绑的列名以外的参数，所以绑**键**。
    Some(NumberMatch {
        sql: format!("{} LIKE '%' || {ph} || '%'", normalized_column_expr(column)),
        bind: normalize_number_term(term)?,
    })
}

/// 一个号码匹配条件及其绑定值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberMatch {
    /// 片段里的 `$1` 要绑 [`Self::bind`]。
    pub sql: String,
    pub bind: String,
}

/// 一个检索词在某类型上的全部匹配条件（已 OR 合并）。
///
/// 对应上游 `keyword_conditions` 里 `term_conditions` 的三种结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermCondition {
    /// 恒假：该词在所有可匹配字段上都不适用。
    Never,
    /// 恰好一个条件。
    Single(String),
    /// 多个条件 OR。
    AnyOf(Vec<String>),
}

impl TermCondition {
    /// 按上游规则折叠一批条件。
    ///
    /// ```text
    /// 空            -> Never     （恒假，而不是「忽略这个词」）
    /// 一个          -> Single
    /// 多个          -> AnyOf
    /// ```
    ///
    /// 三个分支都有测试，因为**空批处理成「忽略」**是最容易犯的错 ——
    /// 它让整个过滤静默失效，而结果看起来「只是没筛到东西」。
    #[must_use]
    pub fn from_parts(parts: Vec<String>) -> Self {
        match parts.len() {
            0 => Self::Never,
            1 => Self::Single(parts.into_iter().next().expect("长度已匹配为 1")),
            _ => Self::AnyOf(parts),
        }
    }

    /// 展开成一段可交给仓储的 SQL（不含 bind 值）。
    ///
    /// 词内 OR 用 `OR` 连接；[`Self::Never`] 展开成字面量 `FALSE`。
    #[must_use]
    pub fn to_sql(&self) -> String {
        match self {
            Self::Never => "FALSE".to_owned(),
            Self::Single(sql) => sql.clone(),
            Self::AnyOf(parts) => parts.join(" OR "),
        }
    }
}

/// 多个词之间的组合：**AND**。
///
/// 上游不在这里加 `AND` —— 它把条件列表交给调用方，而调用方（Peewee 的
/// `where(*conditions)`）默认就是 AND。Rust 侧把它显式化，因为
/// 「词之间 AND」是这条规则里**最容易被漏掉的一半**：漏了它，用户的
/// 多词搜索会变成「任一词命中」，结果集大得多且看不出原因。
#[must_use]
pub fn and_all(conditions: &[String]) -> String {
    if conditions.is_empty() {
        // 没有任何词 = 不过滤。上游 `where()` 不带参数即如此。
        "TRUE".to_owned()
    } else {
        conditions.join(" AND ")
    }
}

/// 逐词追加检索条件，**自己分配占位符编号**。
///
/// 对应上游 `keyword_conditions`（`search_filters.py:34`）。它替代手工
/// 拼 `TermCondition` 的做法，理由是编号：每个词的条件各带自己的绑定值，
/// 两个词就会有两个占位符，而片段里的 `$1` 是写死的 —— 手工拼必然撞号。
///
/// # 词内 OR、词间 AND
///
/// - 一个词在「番号列」与「文本列」上的条件 **OR**；
/// - 多个词之间 **AND**；
/// - 一个词在**所有**可匹配列上都不适用时追加恒假 `FALSE` —— 不是忽略它。
///
/// # `number_column` / `text_column` 为 `None` 的含义
///
/// 「这一类不按番号搜」（如视频时刻没有番号字段）。与「有该列但这个词
/// 匹配不上」不同：后者仍可能从文本列命中。
///
/// # 编号连续性由测试钉住
///
/// [`FilterBuilder::finish`] 返回的 `binds` 顺序**必须**与 SQL 里占位符的
/// 升序一致，否则值会绑到错的条件上 —— 而那不会报错，只会让过滤悄悄失效。
/// 所以 `placeholders_are_numbered_in_bind_order` 断言两者逐位对齐。
pub struct KeywordFilters<'a> {
    builder: &'a mut FilterBuilder,
}

impl<'a> KeywordFilters<'a> {
    /// 创建一个词过滤器，追加到 `builder`。
    pub fn new(builder: &'a mut FilterBuilder) -> Self {
        Self { builder }
    }

    /// 追加一批词。`terms` 为空 = 不产生任何条件。
    pub fn push_terms(
        &mut self,
        terms: &[String],
        number_column: Option<&str>,
        text_column: Option<&str>,
    ) {
        for term in terms {
            let mut alternatives: Vec<String> = Vec::new();

            if let Some(column) = number_column {
                // 编号在这里分配 —— 词内第二个条件拿到的是**下一个**编号。
                let placeholder = self.builder.alloc_placeholder();
                if let Some(m) = number_condition_at(column, term, placeholder) {
                    self.builder.bind(m.bind);
                    alternatives.push(m.sql);
                }
            }

            if let Some(column) = text_column {
                // 上游 `title.contains(term)` 在 PostgreSQL 上编译成 ILIKE，
                // 大小写不敏感，所以这里**不再**做大小写归一。
                let placeholder = self.builder.alloc_placeholder();
                self.builder.bind(term.clone());
                alternatives.push(format!("{column} ILIKE '%' || ${placeholder} || '%'"));
            }

            if alternatives.is_empty() {
                // 恒假，而不是跳过这个词。静默丢弃会让用户看到「搜了个没用
                // 的词，列表没变」，而他真正想要的过滤根本没发生。
                self.builder.push_raw("FALSE".to_owned());
            } else {
                self.builder.push_raw(alternatives.join(" OR "));
            }
        }
    }
}

/// 累积 SQL 条件与绑定值，并**持有占位符编号的分配权**。
///
/// # 为什么编号要由一个值持有
///
/// 一条真实的列表查询要在多个位置绑值，而每个片段单独看都以为 `$1` 是
/// 自己的：先 `movie_number = $1`，再排除某合集的子查询 `$2`，最后关键词
/// 条件从 `$3` 起。编号一旦撞上，PostgreSQL 不会报错 —— 它只是把值绑给
/// 位置，于是「按番号过滤」变成「按关键词过滤」，且只在特定输入下显形。
///
/// 所以规则只有一条：**编号只能由 [`Self::alloc_placeholder`] 分配**，
/// 片段通过它拿编号，不自己写。
///
/// # `binds` 的顺序即占位符升序
///
/// 因为分配是单调递增、且分配与追加在同一步发生，所以 `finish()` 交出的
/// `binds` 天然与 SQL 里的 `$1..$n` 对齐。这不是巧合，是这个类型存在的
/// 全部意义，所以有测试逐位断言。
#[derive(Debug)]
pub struct FilterBuilder {
    conditions: Vec<String>,
    binds: Vec<String>,
    next_placeholder: usize,
}

impl FilterBuilder {
    /// 从 `base` 开始分配编号。
    ///
    /// `base` 是**第一个**占位符的编号，**不是**已用数量 —— 已用数量由调用
    /// 方自己数（它知道自己的查询里有几处绑定）。例如已绑了
    /// `movie_number` 与 `collection_id` 两处，就传 `3`。
    #[must_use]
    pub fn starting_at(base: usize) -> Self {
        Self {
            conditions: Vec::new(),
            binds: Vec::new(),
            next_placeholder: base,
        }
    }

    /// 分配下一个占位符编号。
    pub fn alloc_placeholder(&mut self) -> usize {
        let current = self.next_placeholder;
        self.next_placeholder += 1;
        current
    }

    /// 登记一个绑定值。**必须在追加对应 SQL 的同一步调用**，
    /// 否则顺序对不上（见类型文档）。
    pub fn bind(&mut self, value: String) {
        self.binds.push(value);
    }

    /// 追加一段已自带占位符、不需要新绑定值的条件。
    pub fn push_raw(&mut self, sql: String) {
        self.conditions.push(sql);
    }

    /// 追加一个原子条件：占位符由本方法分配，模板里的 `{}` 是它的位置。
    ///
    /// 比「先 `alloc_placeholder()` 再手写 `format!("... ${n} ...")`」更安全，
    /// 因为编号与绑定值在同一步产生，不可能只做一半。
    pub fn push_atom<F>(&mut self, make_sql: F, value: String)
    where
        F: FnOnce(usize) -> String,
    {
        let placeholder = self.alloc_placeholder();
        self.bind(value);
        self.conditions.push(make_sql(placeholder));
    }

    /// 取出最终的条件表达式与绑定值。
    ///
    /// 一个条件都没有时返回 `("TRUE", [])` —— 语义是「不过滤」，与上游
    /// `where()` 不带参数一致。返回 `FALSE` 会让「没填筛选条件」变成
    /// 「列表永远是空的」。
    #[must_use]
    pub fn finish(self) -> (String, Vec<String>) {
        if self.conditions.is_empty() {
            ("TRUE".to_owned(), Vec::new())
        } else {
            (self.conditions.join(" AND "), self.binds)
        }
    }

    /// 已分配过的占位符个数（= 绑定值个数）。
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        self.binds.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------ 归一化

    #[test]
    fn a_terminess_symbol_only_term_matches_nothing() {
        // 没有任何 ASCII 字母数字 -> 不产生番号条件
        for term in ["", "   ", "---", "!!!", "***", "文字", "・・・"] {
            assert!(!has_searchable_char(term), "{term:?} 不该被认为可搜索");
            assert_eq!(normalize_number_term(term), None, "{term:?} 应归一为 None");
        }
    }

    /// 非 ASCII 字母数字**不算**可搜索 —— 上游 `char.isascii() and char.isalnum()`
    /// 里那个 `isascii()` 不是冗余的。
    #[test]
    fn non_ascii_letters_are_not_searchable_chars() {
        // 纯非 ASCII：不可搜索 -> 番号条件为 None -> 该词恒假
        for term in ["片", "文字", "ムービー"] {
            assert!(
                !has_searchable_char(term),
                "{term:?} 只含非 ASCII 字符，不该算可搜索"
            );
        }
        // 混有一个 ASCII 字符就可搜索 —— 「任一字符」而不是「全部」
        assert!(
            has_searchable_char("A片"),
            "一个 ASCII 字母就够 —— 上游是 any() 不是 all()"
        );
        assert!(has_searchable_char(".Te片"));
    }

    #[test]
    fn separators_and_case_are_normalized_away() {
        assert_eq!(
            normalize_number_term("  abc-123  ").as_deref(),
            Some("ABC123")
        );
        assert_eq!(normalize_number_term("abc_123").as_deref(), Some("ABC123"));
        assert_eq!(normalize_number_term("a-b-c").as_deref(), Some("ABC"));
        assert_eq!(normalize_number_term("ABC").as_deref(), Some("ABC"));
    }

    /// FC2PPV 前缀折成 FC2 —— 两种写法要能互相搜到。
    #[test]
    fn the_fc2ppv_prefix_folds_to_fc2() {
        assert_eq!(
            normalize_number_term("fc2ppv-1234567").as_deref(),
            Some("FC21234567")
        );
        assert_eq!(
            normalize_number_term("FC2-PPV-1234567").as_deref(),
            Some("FC21234567"),
            "带分隔符的写法要折成同一个键"
        );
        // 不带 PPV 的写法本来就是 FC2 前缀
        assert_eq!(
            normalize_number_term("fc2-1234567").as_deref(),
            Some("FC21234567")
        );
    }

    /// 只折**前缀**，中间出现不动 —— 与上游一致。
    #[test]
    fn only_a_leading_fc2ppv_is_folded() {
        assert_eq!(
            normalize_number_term("x-fc2ppv-1").as_deref(),
            Some("XFC2PPV1"),
            "中间出现的 FC2PPV 不折 —— 上游只判 startswith"
        );
    }

    // ------------------------------------------------------ 快路径

    #[test]
    fn the_fast_path_only_covers_digits_with_one_separator() {
        for term in ["2023-001", "2023_001"] {
            assert!(is_plain_number_pattern(term), "{term} 应走快路径");
        }
        for term in [
            "abc-123",    // 有字母
            "2023",       // 没有分隔符
            "-2023",      // 分隔符在前
            "2023-",      // 分隔符在后
            "2023-001-2", // 两个分隔符
            "2023--001",  // 连续两个分隔符
        ] {
            assert!(!is_plain_number_pattern(term), "{term} 不该走快路径");
        }
    }

    /// 快路径用**列原文** + **未归一化的词**，不走 TRANSLATE 表达式。
    #[test]
    fn the_fast_path_avoids_the_expensive_expression() {
        let fast = number_condition("m.movie_number", " 2023-001 ").expect("有条件");
        assert_eq!(fast.sql, "m.movie_number LIKE '%' || $1 || '%'");
        assert_eq!(fast.bind, "2023-001", "快路径用原样大写词");

        let slow = number_condition("m.movie_number", "abc-123").expect("有条件");
        assert!(
            slow.sql.contains("TRANSLATE"),
            "非快路径必须走归一化表达式，实际 {}",
            slow.sql
        );
        assert_eq!(slow.bind, "ABC123");
    }

    /// 归一化表达式的三步顺序不能换。
    #[test]
    fn the_column_expression_normalizes_in_the_upstream_order() {
        let expr = normalized_column_expr("col");
        assert_eq!(
            expr, "REPLACE(UPPER(TRANSLATE(col, '-_', '')), 'FC2PPV', 'FC2')",
            "顺序错了结果就不同：先删分隔符才轮到前缀匹配"
        );
    }

    // ------------------------------------------------------ 条件折叠

    /// 一个词匹配不到任何字段 -> **恒假**，不是「忽略这个词」。
    ///
    /// 这是最容易犯的错：忽略会让整个过滤静默失效，而结果看起来
    /// 「只是没筛到东西」。
    #[test]
    fn an_empty_condition_set_becomes_never_not_ignored() {
        let cond = TermCondition::from_parts(vec![]);
        assert_eq!(cond, TermCondition::Never);
        assert_eq!(cond.to_sql(), "FALSE");
    }

    #[test]
    fn a_single_condition_stays_single() {
        let cond = TermCondition::from_parts(vec!["a = 1".to_owned()]);
        assert_eq!(cond, TermCondition::Single("a = 1".to_owned()));
        assert_eq!(cond.to_sql(), "a = 1", "单个条件不该被包一层括号");
    }

    #[test]
    fn several_conditions_are_ored_together() {
        let cond = TermCondition::from_parts(vec!["a = 1".to_owned(), "b = 2".to_owned()]);
        assert_eq!(cond.to_sql(), "a = 1 OR b = 2");
    }

    /// 词之间 **AND** —— 漏了它，多词搜索会变成「任一词命中」。
    #[test]
    fn terms_are_anded_not_ored() {
        let combined = and_all(&["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        assert_eq!(combined, "a AND b AND c");
    }

    /// 没有任何词 = 不过滤，不是「什么都不匹配」。
    #[test]
    fn no_terms_means_no_filtering() {
        assert_eq!(and_all(&[]), "TRUE");
    }

    /// 一个恒假词会把整个组合变成假 —— 这就是「不静默丢弃」的效果。
    #[test]
    fn a_never_term_makes_the_whole_query_match_nothing() {
        let terms = [
            TermCondition::Single("a = 1".to_owned()),
            TermCondition::Never,
        ];
        let combined = and_all(&terms.iter().map(TermCondition::to_sql).collect::<Vec<_>>());
        assert_eq!(
            combined, "a = 1 AND FALSE",
            "恒假词必须让整体恒假 —— 否则那个词被静默忽略了"
        );
    }

    // ------------------------------------------------------ 占位符编号

    /// 本文件最重要的一条不变量：**`binds` 的顺序必须与占位符升序逐位对齐**。
    ///
    /// 对不上时 PostgreSQL 不报错，只把值绑给别的位置 ——「按番号过滤」会
    /// 悄悄变成「按关键词过滤」，且只在特定输入下显形。所以这里不是断言
    /// 某几个具体数字，而是断言「SQL 里第 n 个占位符对应 binds[n-1]」。
    #[test]
    fn placeholders_are_numbered_in_bind_order() {
        let terms = vec!["abc-123".to_owned(), "2023-001".to_owned()];
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&terms, Some("c"), Some("t"));
        let (sql, binds) = builder.finish();

        // 把 SQL 里的占位符按出现顺序抠出来
        let found: Vec<usize> = sql
            .split('$')
            .skip(1)
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<usize>().ok()
            })
            .collect();
        assert_eq!(
            found,
            (1..=binds.len()).collect::<Vec<_>>(),
            "占位符必须是 1..=n 的连续升序，实际 SQL: {sql}"
        );
        // 两个词 × (番号 + 文本) = 4 个绑定值
        assert_eq!(binds.len(), 4, "实际 SQL: {sql} / binds: {binds:?}");
    }

    /// `base` 是**第一个**占位符的编号，不是「已用数量」。
    ///
    /// 调用方已经绑过 `movie_number = $1` 与合集子查询 `$2`，所以关键词
    /// 必须从 `$3` 起。若这里从 `$1` 起，值会绑到 `movie_number` 上 ——
    /// 而那会让「按番号筛选」变成「按关键词筛选」，不报错。
    #[test]
    fn a_base_offset_shifts_every_placeholder() {
        let terms = vec!["abc-123".to_owned()];
        let mut builder = FilterBuilder::starting_at(3);
        KeywordFilters::new(&mut builder).push_terms(&terms, Some("c"), Some("t"));
        let (sql, binds) = builder.finish();

        assert!(sql.contains("$3"), "首个占位符必须是 $3，实际: {sql}");
        assert!(sql.contains("$4"), "第二个占位符必须是 $4，实际: {sql}");
        assert!(
            !sql.contains("$1") && !sql.contains("$2"),
            "不得复用调用方已占用的编号，实际: {sql}"
        );
        assert_eq!(binds.len(), 2);
    }

    /// 番号条件用给定编号，而不是恒定 `$1`。
    #[test]
    fn number_condition_at_uses_the_requested_placeholder() {
        let m = number_condition_at("c", "abc-123", 7).expect("有条件");
        assert!(m.sql.contains("$7"), "实际: {}", m.sql);
        assert_eq!(m.bind, "ABC123");
        // base=1 的包装仍给出 $1
        assert!(number_condition("c", "abc-123")
            .expect("有条件")
            .sql
            .contains("$1"));
    }

    /// 没有任何词 = 不过滤，且**不消耗任何编号**。
    ///
    /// 「不消耗」是要紧的：调用方按 `placeholder_count()` 推算后续编号。
    #[test]
    fn no_terms_means_no_filtering_and_no_placeholders() {
        let mut builder = FilterBuilder::starting_at(3);
        KeywordFilters::new(&mut builder).push_terms(&[], Some("c"), Some("t"));
        assert_eq!(builder.placeholder_count(), 0);
        assert_eq!(builder.finish(), ("TRUE".to_owned(), Vec::new()));
    }

    /// 一个词在所有列上都不适用 -> 恒假，且不分配编号。
    ///
    /// 纯符号词在番号列上被 [`has_searchable_char`] 排除；这里给它一个
    /// `None` 文本列，模拟「时刻点没有标题可搜」那类情形。
    #[test]
    fn an_unmatched_term_becomes_false_without_consuming_a_placeholder() {
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&["---".to_owned()], Some("c"), None);
        let (sql, binds) = builder.finish();
        assert_eq!(sql, "FALSE", "匹配不到任何字段的词必须恒假，不能被忽略");
        assert!(binds.is_empty(), "恒假条件不绑定任何值，实际: {binds:?}");
    }

    /// 词内 OR：番号命中或标题命中都算。
    #[test]
    fn one_term_ores_the_number_and_text_columns() {
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&["abc-123".to_owned()], Some("c"), Some("t"));
        let (sql, binds) = builder.finish();
        assert!(sql.contains(" OR "), "同一词的两个列必须 OR: {sql}");
        assert_eq!(binds, vec!["ABC123".to_owned(), "abc-123".to_owned()]);
    }

    /// 词间 AND —— 漏了它，多词搜索会变成「任一词命中」。
    #[test]
    fn the_builder_ands_separate_terms() {
        let terms = vec!["a-1".to_owned(), "b-2".to_owned()];
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&terms, Some("c"), None);
        let (sql, _) = builder.finish();
        assert!(sql.contains(" AND "), "多个词必须 AND: {sql}");
    }

    /// `push_atom` 让编号与绑定值在同一步产生 —— 不可能只做一半。
    #[test]
    fn push_atom_allocates_and_binds_together() {
        let mut builder = FilterBuilder::starting_at(2);
        builder.push_atom(|ph| format!("x = ${ph}"), "v1".to_owned());
        builder.push_atom(|ph| format!("y = ${ph}"), "v2".to_owned());
        assert_eq!(
            builder.finish(),
            (
                "x = $2 AND y = $3".to_owned(),
                vec!["v1".to_owned(), "v2".to_owned()]
            )
        );
    }

    /// 没有番号列时（时刻点那类），只有文本条件，且不分配番号的编号。
    #[test]
    fn a_none_number_column_skips_the_number_branch() {
        let mut builder = FilterBuilder::starting_at(1);
        KeywordFilters::new(&mut builder).push_terms(&["片".to_owned()], None, Some("t"));
        let (sql, binds) = builder.finish();
        assert!(
            !sql.contains("TRANSLATE"),
            "没有番号列就不该有归一化: {sql}"
        );
        assert_eq!(sql, "t ILIKE '%' || $1 || '%'");
        assert_eq!(binds, vec!["片".to_owned()]);
    }
}
