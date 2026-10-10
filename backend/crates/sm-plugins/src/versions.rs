//! 插件版本号的比较（**PEP 440 的常用子集**）。
//!
//! 只服务一件事：升级时判断「新包版本**严格高于**已装版本」。上游
//! `manager.py:247` 用的是 `Version(manifest.version) > Version(current.version)`，
//! 即 `packaging.version` 的 PEP 440。
//!
//! # 为什么不用 `semver` crate
//!
//! PEP 440 与 semver **不是同一套**：PEP 440 允许 `1.0`（等价于 `1.0.0`）、
//! `1.0rc1` 这类预发布后缀、`1!2.0` 这种 epoch、`1.0+local` 这种本地标记。
//! 插件作者实际写的是最常见的 `1.2.3`，所以这里实现**够用的子集**，并把不支持
//! 的形态明确列出来 —— 而不是引一个依赖去覆盖用不到的部分。
//!
//! # 支持的规则
//!
//! | 规则 | 例 |
//! |---|---|
//! | 按 `.` / `-` / `_` 分段 | `1.2.3` → `1,2,3` |
//! | 数字段**按数值**比（不是字典序）| `1.10 > 1.9` |
//! | 缺段视作 `0` | `1.2 == 1.2.0` |
//! | 预发布后缀**低于**同版本正式版 | `1.0rc1 < 1.0` |
//! | 预发布之间：dev < a < b < rc < 其它 < 正式 < post | `1.0a1 < 1.0b1 < 1.0rc1 < 1.0 < 1.0.post1` |
//!
//! # **不支持**的形态
//!
//! | 形态 | 例 | 后果 |
//! |---|---|---|
//! | epoch | `1!2.0` | `!` 不参与分段，落进段内后缀 → 可能比出意外结果 |
//! | 本地标记 | `1.0+local` | 同上（`+` 也不是分隔符）|
//!
//! 这两种形态的后果是「比出来的结果可能不合预期」，而它的表现**要么是明确
//! 报错**（「升级包版本必须高于当前版本」）**要么是照常装上** —— 不会出现
//! 「拿旧代码覆盖新代码」那种静默损坏（那需要比反方向，而两个都带 `!`/`+`
//! 的版本才会走到那里）。这一点是本模块敢用子集的前提。
//!
//! 另有一处**故意**的宽松：`1.0foo`（不认识的字母后缀）被当作预发布，
//! 于是它**低于** `1.0`。PEP 440 会直接判它非法。取宽松是因为「拒绝安装一个
//! 作者写了奇怪版本号的包」的代价比「少让你升一次」大。

use std::cmp::Ordering;

/// 比较两个版本号。
///
/// 不区分大小写（`1.0RC1 == 1.0rc1`）。
///
/// # 为什么分两趟
///
/// 第一趟只比**数字段**（release），第二趟才比后缀（预发布 / post）。
/// 一趟比完（每段先数字后后缀）会在 `1.0.1` vs `1.0.post9` 上判反：
/// 那段的下标 1 是 `0` vs `0(post9)`，后缀规则立刻判 `post9` 更大 ——
/// 而 PEP 440 里 `1.0.1` 的 release 是 `(1,0,1)`、`1.0.post9` 是 `(1,0)`，
/// **前者更大**。「段数更多」是 release 变大的信号，必须先比完 release。
pub fn compare(left: &str, right: &str) -> Ordering {
    let left = segments(left);
    let right = segments(right);
    let longest = left.len().max(right.len());

    // 第一趟：release。缺段 = `0`（PEP 440：`1.2 == 1.2.0`）。
    for index in 0..longest {
        let a = left.get(index).map_or(0, |segment| segment.number);
        let b = right.get(index).map_or(0, |segment| segment.number);
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return other,
        }
    }

    // 第二趟：后缀。缺段 = 空后缀（= 正式版）。
    for index in 0..longest {
        let a = left.get(index).map_or("", |segment| segment.suffix);
        let b = right.get(index).map_or("", |segment| segment.suffix);
        match cmp_suffix(a, b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// `candidate` 是否**严格高于** `current`。
///
/// 相等返回 `false` —— 上游要求升级包版本必须更高（`manager.py:252-256`），
/// 而「同版本重装」不是升级。允许它会让「上传错了包」表现为「升级成功」。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

/// 一段版本号：前导数字 + 挂在它上面的后缀。
#[derive(Debug, Clone, Copy)]
struct Segment<'a> {
    number: u64,
    suffix: &'a str,
}

impl<'a> Segment<'a> {
    fn new(number: u64, suffix: &'a str) -> Self {
        Self { number, suffix }
    }
}

/// 切段。
///
/// # 「没有前导数字的段」要挂到上一段
///
/// `1.0.dev1` 的 `dev1` 不属于新的一段，它是 `0` 的**后缀** —— 与 `1.0a1` 里
/// 的 `a1` 挂在同一个数上。不这么处理的话，比较会在**下标 1** 就分出胜负
/// （`0` vs `0a1`，正式版后缀高于预发布），于是得出
/// `1.0.dev1 > 1.0a1` —— 与 PEP 440 正好相反。
///
/// 空段被丢掉（`1..2` 当作 `1.2`）。
fn segments(value: &str) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment<'_>> = Vec::new();
    for part in value
        .trim()
        .split(['.', '-', '_'])
        .filter(|part| !part.is_empty())
    {
        let end = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        let (digits, suffix) = part.split_at(end);

        if !digits.is_empty() {
            out.push(Segment::new(parse_number(digits), suffix));
            continue;
        }
        // 已经挂过后缀就另起一段（`1.0a1b2` 这种）：覆盖会让前半段消失。
        match out.last_mut() {
            Some(last) if last.suffix.is_empty() => last.suffix = suffix,
            _ => out.push(Segment::new(0, suffix)),
        }
    }
    out
}

/// 解析数字段。溢出（`99999999999999999999`）按最大值处理 —— 那是个荒唐的
/// 版本号，但让它**比任何真实版本都大**比让它变成 0 更接近原意。
fn parse_number(digits: &str) -> u64 {
    digits.parse().unwrap_or(u64::MAX)
}

/// 前缀匹配，**忽略大小写**（`1.0RC1` 与 `1.0rc1` 同阶段）。
fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    let (value, prefix) = (value.as_bytes(), prefix.as_bytes());
    // 按字节切片而不是 `value[..n]`：后者在非 ASCII 值上会切到字符中间而 panic。
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 后缀的「阶段」权重。空后缀（正式版）是 5，post 类高于它。
fn severity(suffix: &str) -> u8 {
    if suffix.is_empty() {
        return 5;
    }
    // 顺序有意义：`rc` 必须排在 `r` 之前，否则 `rc1` 会被 `r` 抓走。
    const TABLE: [(&str, u8); 12] = [
        ("dev", 0),
        ("alpha", 1),
        ("a", 1),
        ("beta", 2),
        ("b", 2),
        ("preview", 3),
        ("pre", 3),
        ("rc", 3),
        ("c", 3),
        ("post", 6),
        ("rev", 6),
        ("r", 6),
    ];
    for (name, rank) in TABLE {
        if starts_with_ignore_case(suffix, name) {
            return rank;
        }
    }
    // 不认识的字母后缀：低于正式版（与 `1.0foo < 1.0` 一致），高于 rc。
    4
}

/// 后缀内部的比较：先按阶段，再按里面的数字，最后按字典序。
fn cmp_suffix(left: &str, right: &str) -> Ordering {
    match severity(left).cmp(&severity(right)) {
        Ordering::Equal => {}
        other => return other,
    }
    match tail_number(left).cmp(&tail_number(right)) {
        Ordering::Equal => {}
        other => return other,
    }
    // 忽略大小写（`1.0ZETA` 与 `1.0zeta` 等价）—— 只在前面都打平时才走到这里。
    left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
}

/// 后缀里出现的数字（`rc2` → 2）。没有数字就是 0。
fn tail_number(suffix: &str) -> u64 {
    let digits: String = suffix.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        0
    } else {
        digits.parse().unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_segments_compare_as_numbers_not_strings() {
        // 字典序会给出 `1.9 > 1.10` —— 那正是版本比较最容易写错的一处。
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.9.0", "1.10.0"));
        assert!(is_newer("2.0", "1.99.99"));
    }

    #[test]
    fn a_missing_segment_counts_as_zero() {
        // PEP 440：`1.2 == 1.2.0` —— 于是「同版本重装」不算升级。
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert!(!is_newer("1.2.0", "1.2"), "相等不是更高");
        assert!(is_newer("1.2.1", "1.2"));
    }

    #[test]
    fn pre_releases_rank_below_the_final_release() {
        assert!(is_newer("1.0", "1.0rc1"), "正式版高于它自己的 rc");
        assert!(!is_newer("1.0rc1", "1.0"));
        assert!(is_newer("1.0rc2", "1.0rc1"), "rc 之间的数字要按数值比");
    }

    #[test]
    fn the_pre_release_ladder_matches_pep440() {
        let ladder = ["1.0.dev1", "1.0a1", "1.0b1", "1.0rc1", "1.0", "1.0.post1"];
        for window in ladder.windows(2) {
            assert!(
                is_newer(window[1], window[0]),
                "{} 应当高于 {}",
                window[1],
                window[0]
            );
        }
    }

    #[test]
    fn post_releases_rank_above_the_final_release() {
        assert!(is_newer("1.0.post1", "1.0"));
        assert!(is_newer("1.0.1", "1.0.post9"), "次版本号高于任何 post");
    }

    #[test]
    fn an_unknown_suffix_behaves_like_a_pre_release() {
        // 不认识的字母后缀排在 rc 之后、正式版之前 —— 后果是「可能少让你升一次」，
        // 而不是「把在用的插件换成旧代码」。
        assert!(is_newer("1.0", "1.0zeta"));
        assert!(is_newer("1.0zeta", "1.0rc9"));
    }

    #[test]
    fn comparison_is_case_insensitive_and_tolerates_separators() {
        // `-` 与 `_` 也是 PEP 440 的分隔符。
        assert_eq!(compare("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare("1.0-1", "1.0.1"), Ordering::Equal);
        assert_eq!(compare("1.0_1", "1.0.1"), Ordering::Equal);
        // 大小写：`RC` 与 `rc` 同阶段。
        assert_eq!(compare("1.0RC1", "1.0rc1"), Ordering::Equal);
    }

    #[test]
    fn a_same_version_is_never_newer() {
        // 上游要求**严格**更高（`manager.py:252-256`）：允许相等会让
        // 「上传错了包」表现为「升级成功」。
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0", "1.0.0.0"));
        assert!(!is_newer("0.1", "0.1"));
    }

    #[test]
    fn a_downgrade_is_rejected() {
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(!is_newer("0.9", "1.0"));
    }

    #[test]
    fn an_absurd_number_does_not_wrap_to_zero() {
        // 溢出若退化成 0，`99999999999999999999` 就成了「低于任何版本」。
        assert!(is_newer("99999999999999999999", "1.0"));
    }

    #[test]
    fn comparison_is_a_total_order_on_the_common_forms() {
        let versions = [
            "0.1", "0.9", "1.0a1", "1.0b1", "1.0rc1", "1.0", "1.0.1", "1.1", "2.0",
        ];
        for (index, left) in versions.iter().enumerate() {
            for (other_index, right) in versions.iter().enumerate() {
                let expected = index.cmp(&other_index);
                assert_eq!(
                    compare(left, right),
                    expected,
                    "compare({left}, {right}) 应为 {expected:?}"
                );
            }
        }
    }
}
