//! 判定核心：纯函数，与上游 `plugin.py` 的判定分支逐行对齐。
//!
//! # 上游判定流程（`judge_movies` 内层循环）
//!
//! 对每部影片：
//! 1. 取 `duration_minutes`（缺省 0）、`movie_number`（归一化）。
//! 2. `matches_number_feature`：归一化番号以任一前缀特征开头，
//!    或以任一后缀特征结尾。
//! 3. `matches_tag`：任一标签名（casefold 后）在配置的标签集合里。
//! 4. 时长 `<` 阈值 **且** 不命中番号特征 **且** 不命中标签 → 不动。
//! 5. 已经是合集（`is_collection` 真）→ 不动。
//! 6. `is_collection` 的 owner 存在且不是本插件 → 跳过（不覆盖手动判定）。
//! 7. 否则 `patch(movie_id, {"is_collection": True}, expected_revision)`。

use std::collections::HashSet;

use crate::settings::DurationCollectionSettings;

/// 上游 `_normalize_movie_number`。
///
/// 1. `strip().upper()` 并去空格；
/// 2. 纯 `数字-数字` 形状（`123-456` / `123_456`）原样返回；
/// 3. 否则 `_` → `-`，并去掉 `PPV-` 前缀。
pub fn normalize_movie_number(value: &str) -> String {
    let v: String = value
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| *c != ' ')
        .collect();
    let is_digits_dash_digits = {
        let parts: Vec<&str> = v.split(['-', '_']).collect();
        parts.len() == 2
            && !parts[0].is_empty()
            && !parts[1].is_empty()
            && parts[0].chars().all(|c| c.is_ascii_digit())
            && parts[1].chars().all(|c| c.is_ascii_digit())
    };
    if is_digits_dash_digits {
        return v;
    }
    v.replace('_', "-").replace("PPV-", "")
}

/// 单部影片的判定输入（从 `MovieSnapshot` 提取后的形状）。
#[derive(Debug, Clone)]
pub struct MovieInput {
    pub movie_id: i64,
    pub revision: i64,
    pub duration_minutes: u64,
    pub movie_number: String,
    pub is_collection: bool,
    /// `is_collection` 字段的归属（`None` = 无人持有）。
    pub collection_owner: Option<String>,
    /// 标签名（原始大小写；匹配时按 casefold 比）。
    pub tag_names: Vec<String>,
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 标记为合集（需要 patch）。
    Mark,
    /// 不动：未命中任何规则。
    Unchanged,
    /// 已是合集，不动。
    AlreadyCollection,
    /// owner 是别人（手动判定），跳过。
    SkippedOwned,
}

/// 插件自己的 owner 标记：`plugin:<plugin_id>`。
pub fn plugin_owner(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

fn matches_number_feature(normalized: &str, config: &DurationCollectionSettings) -> bool {
    config
        .number_features
        .iter()
        .any(|f| normalized.starts_with(f.as_str()))
        || config
            .suffix_number_features
            .iter()
            .any(|f| normalized.ends_with(f.as_str()))
}

fn matches_tag(tag_names: &[String], config_tag_names: &HashSet<String>) -> bool {
    tag_names
        .iter()
        .any(|t| config_tag_names.contains(t.to_lowercase().as_str()))
}

/// 对单部影片做判定（不含 IO）。与上游分支顺序一致。
pub fn decide(
    movie: &MovieInput,
    config: &DurationCollectionSettings,
    plugin_id: &str,
) -> Decision {
    let normalized = normalize_movie_number(&movie.movie_number);
    let hit_number = matches_number_feature(&normalized, config);
    let hit_tag = matches_tag(&movie.tag_names, &config.tag_names);

    if movie.duration_minutes < config.duration_threshold_minutes && !hit_number && !hit_tag {
        return Decision::Unchanged;
    }
    if movie.is_collection {
        return Decision::AlreadyCollection;
    }
    if let Some(owner) = &movie.collection_owner {
        if *owner != plugin_owner(plugin_id) {
            return Decision::SkippedOwned;
        }
    }
    Decision::Mark
}

/// 扫描统计，与上游 `stats` 的五个 key 对齐。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub scanned: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub skipped_owned: u64,
    pub patch_failed: u64,
}

impl Stats {
    pub fn progress_text(&self) -> String {
        format!(
            "扫描 {} 部，已更新 {} 部，跳过 owner {} 部",
            self.scanned, self.updated, self.skipped_owned
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(id: i64, duration: u64, number: &str, is_collection: bool) -> MovieInput {
        MovieInput {
            movie_id: id,
            revision: 0,
            duration_minutes: duration,
            movie_number: number.to_owned(),
            is_collection,
            collection_owner: None,
            tag_names: vec![],
        }
    }

    #[test]
    fn normalize_cases_match_upstream() {
        // 纯数字-数字形状原样返回（含下划线分隔）。
        assert_eq!(normalize_movie_number("123-456"), "123-456");
        assert_eq!(normalize_movie_number("123_456"), "123_456");
        // 去空格、大写、下划线转横线。
        assert_eq!(normalize_movie_number(" abp_001 "), "ABP-001");
        // PPV- 前缀去掉。
        assert_eq!(normalize_movie_number("ppv-123456"), "123456");
        assert_eq!(normalize_movie_number("PPV-ABP-001"), "ABP-001");
    }

    #[test]
    fn short_movie_without_features_is_unchanged() {
        let config = DurationCollectionSettings::default();
        let m = movie(1, 60, "ABP-001", false);
        assert_eq!(decide(&m, &config, "x"), Decision::Unchanged);
    }

    #[test]
    fn long_movie_is_marked() {
        let config = DurationCollectionSettings::default();
        let m = movie(1, 400, "ABP-001", false);
        assert_eq!(decide(&m, &config, "x"), Decision::Mark);
    }

    #[test]
    fn number_prefix_feature_marks_short_movie() {
        let config = DurationCollectionSettings::default();
        // OFJE 是上游缺省前缀之一。
        let m = movie(1, 60, "ofje-001", false);
        assert_eq!(decide(&m, &config, "x"), Decision::Mark);
    }

    #[test]
    fn already_collection_is_unchanged() {
        let config = DurationCollectionSettings::default();
        let m = movie(1, 400, "ABP-001", true);
        assert_eq!(decide(&m, &config, "x"), Decision::AlreadyCollection);
    }

    #[test]
    fn foreign_owner_is_skipped() {
        let config = DurationCollectionSettings::default();
        let mut m = movie(1, 400, "ABP-001", false);
        m.collection_owner = Some("host:manual".to_owned());
        assert_eq!(decide(&m, &config, "x"), Decision::SkippedOwned);
        // 自己的 owner 可以覆盖（重入安全）。
        m.collection_owner = Some(plugin_owner("x"));
        assert_eq!(decide(&m, &config, "x"), Decision::Mark);
    }

    #[test]
    fn tag_match_marks_short_movie() {
        let config = DurationCollectionSettings {
            tag_names: ["合集".to_owned()].into_iter().collect(),
            ..Default::default()
        };
        let mut m = movie(1, 60, "ABP-001", false);
        m.tag_names = vec!["合集".to_owned()];
        assert_eq!(decide(&m, &config, "x"), Decision::Mark);
    }

    #[test]
    fn progress_text_format() {
        let s = Stats {
            scanned: 10,
            updated: 3,
            skipped_owned: 1,
            ..Default::default()
        };
        assert_eq!(
            s.progress_text(),
            "扫描 10 部，已更新 3 部，跳过 owner 1 部"
        );
    }
}
