//! 六个榜单的声明与取数映射（上游 `boards.py`）。
//!
//! # 本插件不持有 JavDB 客户端
//!
//! 出网那三样（UA、`jdsignature`、代理策略）与登录态归**宿主**一处管，番号由
//! 宿主的 `GetJavdbRankNumbers` 取（上游 `context.build_javdb_provider`）。这里
//! 只有「榜单 key → 那次请求长什么样」的映射 —— 与上游每个 board 上挂的
//! `fetch_numbers` lambda 一一对应。
//!
//! # 一张表派生两件事
//!
//! 声明（`register` 的 `RankingSourceExtension`）与取数（`FetchRanking` 里
//! 翻成 `GetJavdbRankNumbersRequest`）读的是**同一份** [`BOARDS`]：少写一个榜单、
//! 或者某个榜单只在一边出现，都不可能。
//!
//! # TOP250 是动态周期榜单
//!
//! 它的周期不在声明里（随年份滚动），要由 [`periods_to_fetch`] 现算；年份之外
//! 还有四个固定子榜（`all` / `uncensored` / `censored` / `fc2`，见
//! [`TOP250_FIXED_PERIODS`]）。

/// 年份回溯的起点（上游 `TOP250_START_YEAR`）。
pub const TOP250_START_YEAR: i32 = 2008;

/// TOP250 的固定子榜（上游 `TOP250_FIXED_PERIODS`）。
///
/// 这四个是**非年份**周期：不受「历史年份已有数据就不重抓」那条规则管。
pub const TOP250_FIXED_PERIODS: [&str; 4] = ["all", "uncensored", "censored", "fc2"];

/// 取数形状 —— 决定这次请求打哪个端点、参数怎么填。
///
/// 与宿主 `GetJavdbRankNumbersRequest.query` 的 oneof 同构（宿主那边是
/// `playback` / `video_type_rank` / `top` 三选一）。分三种是因为上游
/// `JavdbProvider` 的三个方法打的是三个不同端点、参数名也不同。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// `/api/v1/rankings/playback?filter_by=…&period=…`
    Playback(&'static str),
    /// `/api/v1/rankings?type=…&period=…`（参数名是 `type`，不是 `video_type`）
    Rank(&'static str),
    /// `/api/v1/movies/top`（`top_type` + `type_value` 现算）
    Top,
}

/// 一个榜单的声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    /// 榜单 key（进 URL 与配置键，必须是 slug）。
    pub key: &'static str,
    pub display_name: &'static str,
    /// 静态周期；**空** = 动态周期（要问 [`periods_to_fetch`]）。
    pub periods: &'static [&'static str],
    pub default_period: &'static str,
    /// 周期是否随外部状态滚动（TOP250 按年份）。
    pub dynamic_periods: bool,
    shape: Shape,
}

/// 全部六个榜单（顺序即声明顺序，也是同步顺序）。
pub const BOARDS: &[Board] = &[
    Board {
        key: "playback_all",
        display_name: "热播",
        periods: &["daily", "weekly", "monthly"],
        default_period: "daily",
        dynamic_periods: false,
        shape: Shape::Playback("all"),
    },
    Board {
        key: "playback_high_score",
        display_name: "高评分",
        periods: &["daily", "weekly", "monthly"],
        default_period: "daily",
        dynamic_periods: false,
        shape: Shape::Playback("high_score"),
    },
    Board {
        key: "censored",
        display_name: "有码",
        periods: &["daily", "weekly", "monthly"],
        default_period: "daily",
        dynamic_periods: false,
        // 有码 / 无码 / FC2 共用 `/rankings`，靠 `type` 区分。
        shape: Shape::Rank("0"),
    },
    Board {
        key: "uncensored",
        display_name: "无码",
        periods: &["daily", "weekly", "monthly"],
        default_period: "daily",
        dynamic_periods: false,
        shape: Shape::Rank("1"),
    },
    Board {
        key: "fc2",
        display_name: "FC2",
        periods: &["daily", "weekly", "monthly"],
        default_period: "daily",
        dynamic_periods: false,
        shape: Shape::Rank("3"),
    },
    Board {
        key: "top250",
        display_name: "TOP250",
        // 动态周期**不在载荷里**（它随年份滚动）——读侧要显式问
        // `ResolveRankingPeriods`，所以这里必须是空数组 + `dynamic_periods`。
        periods: &[],
        default_period: "all",
        dynamic_periods: true,
        shape: Shape::Top,
    },
];

/// 按 key 找榜单。
pub fn find(key: &str) -> Option<&'static Board> {
    BOARDS.iter().find(|board| board.key == key)
}

/// 一次取数请求的形状（宿主 `GetJavdbRankNumbersRequest.query` 的投影）。
///
/// 与 proto 类型分开是为了能**纯逻辑单测**：这里是 `String` / `&'static str`，
/// 不必构造 prost 结构就能断言映射对不对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardQuery {
    /// `filter_by` = `all`（热播）/ `high_score`（高评分）。
    Playback {
        filter_by: &'static str,
        period: String,
    },
    /// `video_type` = `0`（有码）/ `1`（无码）/ `3`（FC2）。
    Rank {
        video_type: &'static str,
        period: String,
    },
    /// TOP250：`top_type` = `all` / `year` / `video_type`。
    Top {
        top_type: &'static str,
        type_value: String,
    },
}

impl Board {
    /// 这个榜 + 这个周期 → 宿主取数请求。
    pub fn query(&self, period: &str) -> BoardQuery {
        match self.shape {
            Shape::Playback(filter_by) => BoardQuery::Playback {
                filter_by,
                period: period.to_owned(),
            },
            Shape::Rank(video_type) => BoardQuery::Rank {
                video_type,
                period: period.to_owned(),
            },
            Shape::Top => {
                let (top_type, type_value) = top250_type_for_period(period);
                BoardQuery::Top {
                    top_type,
                    type_value,
                }
            }
        }
    }
}

/// 把 TOP250 的 `period` 映射成宿主 `JavdbTopQuery` 的 `(top_type, type_value)`
/// （上游 `top250_type_for_period`）。
///
/// | period | top_type | type_value |
/// |---|---|---|
/// | `all` | `all` | 空串 |
/// | `censored` / `uncensored` / `fc2` | `video_type` | `0` / `1` / `3` |
/// | 年份（`2026`）| `year` | 年份 |
pub fn top250_type_for_period(period: &str) -> (&'static str, String) {
    if period == "all" {
        return ("all", String::new());
    }
    match period {
        "censored" => return ("video_type", "0".to_owned()),
        "uncensored" => return ("video_type", "1".to_owned()),
        "fc2" => return ("video_type", "3".to_owned()),
        _ => {}
    }
    ("year", period.to_owned())
}

/// TOP250 的年份周期：当前年回溯到 [`TOP250_START_YEAR`]。
pub fn top250_years(current_year: i32) -> impl Iterator<Item = i32> {
    // 起始年比当前年大（时钟错乱 / 提前跑）时**不 panic**，只是没有年份 ——
    // 固定子榜仍然照抓。
    (TOP250_START_YEAR..=current_year).rev()
}

/// 这个榜这次要抓哪些周期 —— 上游 `_iter_sync_targets` 的收敛，本仓挪到插件里
/// （一次 rpc 算完静态周期、动态年份、账号未配三件事）。
///
/// `periods_with_items` 是宿主递来的「该榜这些周期已经有条目了」；只有
/// **历史年份**会被它挡掉（上游 `should_fetch`）：当前年和四个固定子榜照抓 ——
/// 否则榜单会停在第一次同步的结果上，再也不更新。
///
/// 没配账号时 TOP250 一个都不抓：未登录会被站点拒（上游 `account_configured`）。
/// 返回空数组是**正常结果**，不是错误（proto 原话）。
pub fn periods_to_fetch(
    board: &Board,
    periods_with_items: &[String],
    account_configured: bool,
    current_year: i32,
) -> Vec<String> {
    if !board.dynamic_periods {
        return board
            .periods
            .iter()
            .map(|period| (*period).to_owned())
            .collect();
    }
    if !account_configured {
        return Vec::new();
    }
    TOP250_FIXED_PERIODS
        .iter()
        .map(|period| (*period).to_owned())
        .chain(top250_years(current_year).map(|year| year.to_string()).filter(
            |year| {
                let historical = year.parse::<i32>().is_ok_and(|value| value < current_year);
                !(historical && periods_with_items.iter().any(|period| period == year))
            },
        ))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boards_cover_all_six_with_their_periods() {
        let keys: Vec<_> = BOARDS.iter().map(|board| board.key).collect();
        assert_eq!(
            keys,
            vec![
                "playback_all",
                "playback_high_score",
                "censored",
                "uncensored",
                "fc2",
                "top250"
            ]
        );
        for board in BOARDS.iter().filter(|board| board.key != "top250") {
            assert_eq!(
                board.periods,
                ["daily", "weekly", "monthly"],
                "{} 的三个静态周期",
                board.key
            );
            assert_eq!(board.default_period, "daily");
            assert!(!board.dynamic_periods);
        }
        let top250 = find("top250").expect("TOP250 要在表里");
        assert!(top250.periods.is_empty(), "动态周期不进载荷");
        assert_eq!(top250.default_period, "all", "代表值仍要给");
        assert!(top250.dynamic_periods);
    }

    #[test]
    fn playback_boards_differ_only_by_filter_by() {
        assert_eq!(
            find("playback_all").unwrap().query("daily"),
            BoardQuery::Playback {
                filter_by: "all",
                period: "daily".to_owned()
            }
        );
        assert_eq!(
            find("playback_high_score").unwrap().query("weekly"),
            BoardQuery::Playback {
                filter_by: "high_score",
                period: "weekly".to_owned()
            }
        );
    }

    #[test]
    fn video_type_boards_carry_their_type_code() {
        // 0 有码 / 1 无码 / 3 FC2（上游 `SUPPORTED_RANK_VIDEO_TYPES`）。
        for (key, code) in [("censored", "0"), ("uncensored", "1"), ("fc2", "3")] {
            assert_eq!(
                find(key).unwrap().query("daily"),
                BoardQuery::Rank {
                    video_type: code,
                    period: "daily".to_owned()
                },
                "{key}"
            );
        }
    }

    #[test]
    fn top250_period_maps_to_the_host_query() {
        assert_eq!(top250_type_for_period("all"), ("all", String::new()));
        assert_eq!(
            top250_type_for_period("censored"),
            ("video_type", "0".to_owned())
        );
        assert_eq!(
            top250_type_for_period("uncensored"),
            ("video_type", "1".to_owned())
        );
        assert_eq!(top250_type_for_period("fc2"), ("video_type", "3".to_owned()));
        assert_eq!(top250_type_for_period("2026"), ("year", "2026".to_owned()));
    }

    #[test]
    fn unknown_board_has_no_query() {
        assert!(find("nope").is_none());
    }

    #[test]
    fn static_boards_fetch_all_their_periods() {
        let periods = periods_to_fetch(find("playback_all").unwrap(), &[], false, 2026);
        assert_eq!(periods, ["daily", "weekly", "monthly"]);
    }

    /// 没配账号时 TOP250 一个都不抓 —— 空数组是正常结果（proto 原话）。
    #[test]
    fn top250_needs_an_account() {
        let top250 = find("top250").unwrap();
        assert!(periods_to_fetch(top250, &[], false, 2026).is_empty());
    }

    /// 固定子榜照抓；当前年照抓；**历史年份**已有条目才跳过。
    #[test]
    fn top250_skips_only_historical_years_with_items() {
        let top250 = find("top250").unwrap();
        let with_items = vec![
            "2026".to_owned(), // 当前年：照抓
            "2025".to_owned(), // 历史年份：跳
            "all".to_owned(),  // 固定子榜：照抓
        ];
        let periods = periods_to_fetch(top250, &with_items, true, 2026);
        // 固定子榜在前（顺序即抓取顺序）。
        assert_eq!(&periods[..4], &["all", "uncensored", "censored", "fc2"]);
        let years: Vec<&String> = periods[4..].iter().collect();
        assert!(years.contains(&&"2026".to_owned()), "当前年要在：{years:?}");
        assert!(
            !years.contains(&&"2025".to_owned()),
            "已有条目的历史年份不该再抓：{years:?}"
        );
        assert!(years.contains(&&"2024".to_owned()), "没条目的历史年份要抓");
        assert_eq!(
            years.last().map(|year| year.as_str()),
            Some("2008"),
            "一直回溯到起点"
        );
    }

    /// 时钟错乱（当前年早于起点）不该 panic，也不该吐出年份。
    #[test]
    fn top250_survives_a_clock_before_the_start_year() {
        let top250 = find("top250").unwrap();
        let periods = periods_to_fetch(top250, &[], true, 2000);
        assert_eq!(periods, TOP250_FIXED_PERIODS);
    }
}
