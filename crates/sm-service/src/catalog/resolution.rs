//! 分辨率档位，对应上游 `src/service/catalog/movie_resolution_service.py`。
//!
//! # 这个文件解决的是「档位不可比较」
//!
//! 上游的 `Media.resolution` 存的是 `1920x1080` 这样的字符串，而筛选参数是
//! `4K` / `1080P` 这样的**档位标签**。两者要能对上，靠的是一个整数序号：
//!
//! ```text
//! media.resolution --CASE--> 档位序号(0..7) --MAX--> 影片档位 --落在区间--> 命中
//! ```
//!
//! `CASE` 那一步在 SQL 里（[`sm_db::repo::movie`] 的
//! `RESOLUTION_LEVEL_CASE`），本模块负责剩下三步：标签 ↔ 区间、序号 → 标签、
//! 以及非法档位的错误契约。
//!
//! # 档位是**互斥**的，这一点决定了区间是半开的
//!
//! 上游 `resolution_interval` 返回 `[threshold, upper)` 而不是
//! `[threshold, upper]`：`4K → (6, 7)`。若闭区间上界，一部 8K 影片
//! (`level=7`) 会在 `>= 6 AND <= 7` 下同时命中 4K 和 8K 两个筛选项。
//! 8K 的 `upper` 是 `None`（没有比它更高的档位），所以那一侧是单边约束。
//!
//! # 档位 0 不对应任何标签
//!
//! `CASE` 的 `ELSE 0` 覆盖「能解析但有一维为 0」（如 `0x1080`，能被
//! `^\d+x\d+$` 匹配）。它**不是**一个真实档位：`bucket_for_level(0)`
//! 返回 `None`，于是这类影片既不出现在筛选项里，也不会被任何档位筛中。

use crate::error::{details_of, ServiceError};

/// 档位表：`(标签, 该档位的最低序号)`，**按序号从高到低**排列。
///
/// 顺序是契约的一部分，不只是排版：
///
/// - [`resolution_interval`] 用「上一项的阈值」当 `upper`，所以**倒序时
///   拿到的 `upper` 才是更高的那一档**；正序会得到 `4K → (6, 3)` 这种
///   空区间，于是所有筛选都返回空。
/// - [`bucket_for_level`] 返回**第一个** `level >= threshold` 的标签，
///   所以正序会让 `level=4`（1080P）落进 `360P`。
/// - `list_playlist_resolutions` 按这个顺序输出选项，前端直接照此渲染，
///   改顺序等于改 UI。
pub const RESOLUTION_LEVELS: [(&str, i32); 7] = [
    ("8K", 7),
    ("4K", 6),
    ("2K", 5),
    ("1080P", 4),
    ("720P", 3),
    ("480P", 2),
    ("360P", 1),
];

/// 档位序号区间 `[threshold, upper)`。
///
/// `upper == None` 表示没有上界 —— 只有 `8K` 是这种情况（它是最高一档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionInterval {
    /// 该档位的最低序号，闭区间下界。
    pub threshold: i32,
    /// 上一档的序号，**开**区间上界。`None` = 无上界。
    pub upper: Option<i32>,
}

/// 一部影片的聚合结果：它最高的媒体落在哪一档。
///
/// 仓储层返回 `(movie_id, max_level)` 元组，具名类型放在这里 —— 理由见
/// `sm_db::repo::movie::MovieResolutionLevelRow` 的文档（投影行不是表镜像，
/// 在 `sm-db` 里声明成 `pub struct` 会被 schema 对拍当成待验证的表模型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovieResolutionLevel {
    pub movie_id: i32,
    /// 档位序号。**恒非空**：0 表示「能解析但两维有一维为 0」，
    /// [`bucket_for_level`] 对它返回 `None`，于是不计入任何档位。
    pub max_level: i32,
}

impl From<(i32, i32)> for MovieResolutionLevel {
    fn from((movie_id, max_level): (i32, i32)) -> Self {
        Self {
            movie_id,
            max_level,
        }
    }
}

/// 解析分辨率筛选档位。
///
/// `Ok(None)` 表示「不按分辨率筛选」，调用方因此不必改写查询。
/// 非法档位返回 422，`details` 是 `{"resolution": <原始输入>}`。
///
/// # 大小写与空白都归一，但 `details` 回显**原始**输入
///
/// 上游 `normalized = resolution.strip().lower()` 用于匹配，而报错时
/// `{"resolution": resolution}` 塞的是**未归一**的原值。客户端高亮控件时
/// 拿到的是用户实际敲进去的东西（`" 4K "`），不是服务端的判断依据。
/// 这个不一致是契约的一部分。
///
/// # 匹配用 `eq_ignore_ascii_case` 而不是 `to_lowercase()`
///
/// 标签只有 ASCII 字母与数字，两种方式结果相同。`eq_ignore_ascii_case`
/// 少一次分配，且**不会**把 `İ`（单点大写 I）之类的非 ASCII 字符折叠成
/// 匹配项 —— 上游 `.lower()` 会。实际标签里不会出现这些字符，但保持比较
/// 语义与「标签表是固定 ASCII 常量」这个前提一致更稳。
pub fn resolution_interval(
    resolution: Option<&str>,
    error_code: &str,
) -> Result<Option<ResolutionInterval>, ServiceError> {
    let Some(resolution) = resolution else {
        return Ok(None);
    };
    let normalized = resolution.trim();
    for (index, (label, threshold)) in RESOLUTION_LEVELS.iter().enumerate() {
        if label.eq_ignore_ascii_case(normalized) {
            // 倒序表里「上一项」就是更高的一档；index == 0 是 8K，没有上界。
            let upper = index.checked_sub(1).map(|prev| RESOLUTION_LEVELS[prev].1);
            return Ok(Some(ResolutionInterval {
                threshold: *threshold,
                upper,
            }));
        }
    }
    Err(ServiceError::validation_with(
        error_code,
        "Invalid resolution filter",
        details_of("resolution", resolution),
    ))
}

/// 把影片的最高档位序号归入唯一的档位标签。
///
/// 首个 `level >= threshold` 的标签即命中 —— 与 [`RESOLUTION_LEVELS`] 倒序
/// 配合，`level=5` 落进 `2K` 而不是 `1080P`。
///
/// 返回 `None` 的两种情况：`level <= 0`（无法解析，或有一维为 0 —— 不是
/// 真实档位）。
pub fn bucket_for_level(level: i32) -> Option<&'static str> {
    RESOLUTION_LEVELS
        .iter()
        .find(|(_, threshold)| level >= *threshold)
        .map(|(label, _)| *label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_are_half_open_so_levels_do_not_overlap() {
        // 4K -> [6, 7)：一部 8K(level 7) 落不进 4K 区间
        let four_k = resolution_interval(Some("4K"), "invalid_playlist_filter")
            .unwrap()
            .unwrap();
        assert_eq!(
            four_k,
            ResolutionInterval {
                threshold: 6,
                upper: Some(7)
            }
        );
        assert!(in_interval(four_k, 6), "6 是 4K 档位序号");
        assert!(!in_interval(four_k, 7), "7 是 8K，不该命中 4K");
    }

    /// 序号是否落在区间内。与 SQL 侧 `MAX(level) >= threshold AND MAX(level) < upper`
    /// 逐条对应，`upper` 为 `None` 时只有下界。
    fn in_interval(interval: ResolutionInterval, level: i32) -> bool {
        level >= interval.threshold && interval.upper.is_none_or(|upper| level < upper)
    }

    #[test]
    fn the_top_level_has_no_upper_bound() {
        let eight_k = resolution_interval(Some("8K"), "invalid_playlist_filter")
            .unwrap()
            .unwrap();
        assert_eq!(
            eight_k,
            ResolutionInterval {
                threshold: 7,
                upper: None
            }
        );
        // 无上界：7 以及任何更大的序号都命中
        assert!(in_interval(eight_k, 7));
        assert!(in_interval(eight_k, 99));
    }

    #[test]
    fn the_lowest_level_still_has_an_upper_bound() {
        // 360P -> (1, 2)：480P 影片(level 2) 不该落进 360P
        let lowest = resolution_interval(Some("360P"), "invalid_playlist_filter")
            .unwrap()
            .unwrap();
        assert_eq!(
            lowest,
            ResolutionInterval {
                threshold: 1,
                upper: Some(2)
            }
        );
    }

    #[test]
    fn every_level_lands_in_exactly_one_bucket() {
        // 每档的代表序号必须只命中它自己 —— 这就是「档位互斥」
        for (index, (label, threshold)) in RESOLUTION_LEVELS.iter().enumerate() {
            assert_eq!(
                bucket_for_level(*threshold),
                Some(*label),
                "{label} 的代表序号落错了桶"
            );
            // 低于本档阈值时，归入更低的一档
            if index + 1 < RESOLUTION_LEVELS.len() {
                let lower_label = RESOLUTION_LEVELS[index + 1].0;
                assert_eq!(bucket_for_level(threshold - 1), Some(lower_label));
            }
        }
    }

    #[test]
    fn level_zero_is_not_a_bucket() {
        // 「能解析但有一维为 0」不计入任何档位
        assert_eq!(bucket_for_level(0), None);
        assert_eq!(bucket_for_level(-1), None);
    }

    #[test]
    fn no_filter_yields_no_interval() {
        assert_eq!(
            resolution_interval(None, "invalid_playlist_filter").unwrap(),
            None
        );
        // 显式空串是**非法档位**，不是「不筛选」—— 与 None 语义不同
        assert!(resolution_interval(Some(""), "invalid_playlist_filter").is_err());
        assert!(resolution_interval(Some("   "), "invalid_playlist_filter").is_err());
    }

    #[test]
    fn labels_are_matched_case_insensitively_after_trimming() {
        // 上游 strip().lower()，所以 " 4k " 命中 4K
        for probe in ["4K", "4k", " 4K ", "4k\t"] {
            let got = resolution_interval(Some(probe), "invalid_playlist_filter")
                .unwrap()
                .unwrap();
            assert_eq!(got.threshold, 6, "{probe:?} 应命中 4K");
        }
    }

    #[test]
    fn an_unknown_label_is_422_with_the_original_value_echoed() {
        let err = resolution_interval(Some("8k2"), "invalid_playlist_filter").unwrap_err();
        assert_eq!(err.status, 422);
        assert_eq!(err.code(), "invalid_playlist_filter");
        // details 回显原始输入，不是归一后的值
        assert_eq!(
            err.api.details.as_ref().unwrap().get("resolution"),
            Some(&serde_json::json!("8k2"))
        );
    }

    #[test]
    fn the_error_code_is_a_parameter_not_a_constant() {
        // 上游 `resolution_exists_expression(..., error_code=...)` 让调用方
        // 决定错误码 —— 同一个非法值在不同端点上报不同的码。
        let err = resolution_interval(Some("nope"), "invalid_video_filter").unwrap_err();
        assert_eq!(err.code(), "invalid_video_filter");
    }
}
