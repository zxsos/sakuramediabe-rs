//! crontab 表达式：方言转换与校验。
//!
//! # 两种字段顺序，两种星期编号
//!
//! | | 字段顺序 | 段数 | 周日 | 周一 | 周六 |
//! |---|---|---|---|---|---|
//! | 上游 `CronTrigger.from_crontab` | 分 时 日 月 周 | 5 | `0`/`7` | `1` | `6` |
//! | `cron` crate `Schedule` | **秒** 分 时 日 月 周（可选年） | 6/7 | `1` | `2` | `7` |
//!
//! 配置 TOML 里写的是**上游那种**（5 段、分在前），而 `cron` crate 要 6 段、
//! 秒在前。直接透传的后果：
//!
//! - 段数不对 → 解析失败，症状是**全部** cron 字段一起报错；
//! - 星期编号不对 → **静默错一天**（`0 4 * * 1` 从「周一」变「周日」）。
//!
//! 所以本模块是这两种方言之间**唯一**的转换点，`sm-scheduler` 的求值与
//! `sm_core::config_schema` 的校验都走它。

use std::str::FromStr;

/// 把 5 段 crontab 转成 `cron` crate 能解析的 6 段表达式。
///
/// 秒段补 0（cron 精度到秒，秒固定 0 = 每分钟一次，与上游语义一致）。
/// 已是 6/7 段的原样透传。
pub fn to_cron_crate_expr(expr: &str) -> String {
    let mut fields: Vec<String> = expr.split_whitespace().map(str::to_owned).collect();
    if fields.len() == 5 {
        fields.insert(0, "0".to_owned());
    }
    // 星期字段在两种布局里都是索引 5。
    if let Some(day_of_week) = fields.get_mut(5) {
        *day_of_week = remap_day_of_week(day_of_week);
    }
    fields.join(" ")
}

/// 星期字段：把 crontab 编号改成 Quartz 编号。
///
/// 名字（`MON`/`FRI`）两套约定一致，原样保留；`*/n` 的步长锚在字段起点，
/// 而 0-6 映射到 1-7 是等长平移，所以步长不用动。
pub fn remap_day_of_week(field: &str) -> String {
    field
        .split(',')
        .map(remap_day_of_week_part)
        .collect::<Vec<_>>()
        .join(",")
}

fn remap_day_of_week_part(part: &str) -> String {
    // 步长跟在 `/` 之后，且**不该**被映射（`*/2` 的 2 是步长不是星期）。
    let (range, step) = part
        .split_once('/')
        .map_or((part, None), |(range, step)| (range, Some(step)));
    let mapped = if range == "*" {
        "*".to_owned()
    } else if let Some((from, to)) = range.split_once('-') {
        format!("{}-{}", map_day_number(from), map_day_number(to))
    } else {
        map_day_number(range)
    };
    match step {
        Some(step) => format!("{mapped}/{step}"),
        None => mapped,
    }
}

/// `n → n % 7 + 1`。名字与越界值原样返回（后者交给解析器报错）。
fn map_day_number(raw: &str) -> String {
    match raw.parse::<u32>() {
        Ok(0) => "1".to_owned(),
        Ok(n) if n <= 7 => (n % 7 + 1).to_string(),
        _ => raw.to_owned(),
    }
}

/// 校验一个 **5 段 crontab** 表达式（运维在配置文件里写的那种）。
///
/// 判据是「转换后能否被 `cron` crate 解析」，所以它与运行时用的是同一套
/// 语法规则 —— 不会出现「配置校验通过但调度时解析失败」。
///
/// # 5 段是硬要求
///
/// 上游 `Scheduler._validate_cron_expressions` 用
/// `CronTrigger.from_crontab(value)`，而 `from_crontab` **只接受 5 段**。
/// 所以 6 段表达式在配置里**非法**，尽管 `cron` crate 能解析它 —— 那会造成
/// 「配置能存进去但上游会拒绝」的静默分歧。
pub fn is_valid_crontab(expr: &str) -> bool {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    cron::Schedule::from_str(&to_cron_crate_expr(expr)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_expressions_gain_a_zero_seconds_field() {
        assert_eq!(to_cron_crate_expr("15 0 * * *"), "0 15 0 * * *");
        assert_eq!(to_cron_crate_expr("*/5 * * * *"), "0 */5 * * * *");
        assert_eq!(to_cron_crate_expr("0 4 * * 1"), "0 0 4 * * 2");
        assert_eq!(to_cron_crate_expr("30 15 0 * * *"), "30 15 0 * * *");
        assert_eq!(to_cron_crate_expr("0 0 12 * * ? 2030"), "0 0 12 * * ? 2030");
    }

    #[test]
    fn day_of_week_is_remapped_from_crontab_to_quartz() {
        assert_eq!(map_day_number("0"), "1", "周日");
        assert_eq!(map_day_number("7"), "1", "7 也是周日");
        assert_eq!(map_day_number("1"), "2", "周一");
        assert_eq!(map_day_number("6"), "7", "周六");
        assert_eq!(map_day_number("MON"), "MON", "名字两套约定一致");
        assert_eq!(map_day_number("9"), "9", "越界值不猜，交给解析器报错");
    }

    #[test]
    fn weekday_ranges_and_lists_shift_by_one() {
        assert_eq!(remap_day_of_week("1-5"), "2-6", "周一..周五");
        assert_eq!(remap_day_of_week("0-6"), "1-7", "全周仍是全周");
        assert_eq!(remap_day_of_week("1,3,5"), "2,4,6");
        assert_eq!(remap_day_of_week("*/2"), "*/2", "步长不动");
        assert_eq!(remap_day_of_week("1-5/2"), "2-6/2");
        assert_eq!(remap_day_of_week("*"), "*");
        assert_eq!(remap_day_of_week("MON-FRI"), "MON-FRI");
    }

    #[test]
    fn validation_accepts_the_five_field_forms_operators_write() {
        for ok in [
            "0 2 * * *",
            "*/5 * * * *",
            "*/30 * * * *",
            "15 0 * * *",
            "* * * * *",
            "0 4 * * 1", // 周一
            "0 4 * * 0", // 周日
            "0 4 * * 7", // 周日（另一种写法）
            "0 0 1,15 * *",
            "0 9-18 * * 1-5",
        ] {
            assert!(is_valid_crontab(ok), "{ok:?} 应当合法");
        }
    }

    #[test]
    fn validation_rejects_wrong_shapes_and_syntax() {
        for bad in [
            "",            // 空
            "* * * *",     // 4 段
            "* * * * * *", // 6 段：上游 from_crontab 不接受
            "60 * * * *",  // 分钟越界
            "* 24 * * *",  // 小时越界
            "* * 32 * *",  // 日越界
            "* * * 13 *",  // 月越界
            "* * * * 8",   // 星期越界
            "abc * * * *", // 非数字
        ] {
            assert!(!is_valid_crontab(bad), "{bad:?} 应当不合法");
        }
    }

    #[test]
    fn a_weekly_expression_lands_on_the_right_weekday() {
        // 这是本模块存在的主要理由：不映射的话 `0 4 * * 1` 会变成周日。
        use chrono::{DateTime, Datelike, Utc};
        let schedule = cron::Schedule::from_str(&to_cron_crate_expr("0 4 * * 1")).expect("解析");
        let from: DateTime<Utc> = "2026-10-04T10:00:00Z".parse().expect("时间戳");
        let next = schedule.after(&from).next().expect("下一次");
        assert_eq!(next.to_rfc3339(), "2026-10-05T04:00:00+00:00");
        assert_eq!(next.weekday(), chrono::Weekday::Mon);
    }
}
