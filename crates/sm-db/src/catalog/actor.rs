//! `actor` 表映射。
//!
//! 对应 `src/model/catalog/actors.py` 的 `Actor`。
//!
//! # 与 Movie 的差异
//!
//! | 项 | Movie | Actor |
//! |---|---|---|
//! | 生日 | 无 | `birthday` 是 **DateField**（日期非时间） |
//! | 受保护字段 | 6 个 | 9 个（全部资料字段） |
//! | 合并 | 无 | `merged_into` 墓碑指针链 |
//! | 别名 | 无 | `alias_name` 用 `" / "` 分隔存储 |

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde_json::Value as Json;
use sqlx::FromRow;

/// 受保护字段白名单。插件可写，宿主刷新与批量 UPDATE 均被运行时护栏拒绝。
pub const PROTECTED_ACTOR_FIELDS: [&str; 9] = [
    "gender",
    "birthday",
    "height_cm",
    "bust_cm",
    "waist_cm",
    "hips_cm",
    "cup",
    "birthplace",
    "blood_type",
];

/// 额外受护栏约束的字段（不在插件可写白名单内，但也禁止裸写）。
pub const GUARDED_ACTOR_FIELDS: [&str; 5] = [
    "field_owners",
    "mutation_revision",
    "display_name_override",
    "profile_image_override",
    "merged_into",
];

/// `gender` 的合法取值。
///
/// 插件写入时只接受 1 与 2；0 表示未知。
pub const GENDER_UNKNOWN: i32 = 0;
pub const GENDER_FEMALE: i32 = 1;
pub const GENDER_MALE: i32 = 2;

/// 性别字段的合法值集合。
pub const GENDER_ALLOWED: [i32; 2] = [GENDER_FEMALE, GENDER_MALE];

/// `actor` 表。
#[derive(Debug, Clone, FromRow)]
pub struct Actor {
    pub id: i64,

    /// JavDB ID。空串在 save 时归一为 NULL。
    pub javdb_id: Option<String>,
    pub name: String,
    /// 别名合并后的结果，格式 `"主名 / 别名1 / 别名2"`，去重且主名在首位。
    pub alias_name: String,
    /// 墓碑指针：指向合并后的保留记录。
    pub merged_into_id: Option<i64>,
    pub profile_image_id: Option<i64>,
    /// 本地头像覆盖。优先于 `profile_image_id`。
    pub profile_image_override_id: Option<i64>,
    /// 本地显示名覆盖。为空串时归一为 NULL。
    pub display_name_override: Option<String>,

    pub javdb_type: i32,
    /// 性别。1 = 女，2 = 男，0 = 未知。
    pub gender: i32,
    pub is_subscribed: bool,
    pub subscribed_at: Option<NaiveDateTime>,
    pub subscribed_movies_synced_at: Option<NaiveDateTime>,
    /// 订阅影片的**完全**同步时间；与增量同步时间分开。
    pub subscribed_movies_full_synced_at: Option<NaiveDateTime>,

    /// 生日。PostgreSQL 列类型是 `date`，不是 `timestamp`。
    pub birthday: Option<NaiveDate>,
    pub height_cm: Option<i32>,
    pub bust_cm: Option<i32>,
    pub waist_cm: Option<i32>,
    pub hips_cm: Option<i32>,
    pub cup: Option<String>,
    pub birthplace: Option<String>,
    pub blood_type: Option<String>,

    pub field_owners: Json,
    /// 只覆盖受保护字段，不是整行版本。
    pub mutation_revision: i64,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl Actor {
    /// 某字段是否受插件或人工主权保护。
    pub fn is_protected(field: &str) -> bool {
        PROTECTED_ACTOR_FIELDS.contains(&field)
    }

    /// 某字段是否受护栏约束（含受保护字段与额外受控字段）。
    pub fn is_guarded(field: &str) -> bool {
        PROTECTED_ACTOR_FIELDS.contains(&field) || GUARDED_ACTOR_FIELDS.contains(&field)
    }

    /// 性别取值是否合法。对应 `ACTOR_FIELD_ALLOWED_VALUES`。
    pub fn is_valid_gender(value: i32) -> bool {
        GENDER_ALLOWED.contains(&value)
    }

    /// 实际展示名：本地覆盖优先，为空则用 `name`。
    pub fn display_name(&self) -> &str {
        match self.display_name_override.as_deref() {
            Some(override_name) if !override_name.trim().is_empty() => override_name,
            _ => &self.name,
        }
    }

    /// 是否设置了本地头像覆盖。
    pub fn has_profile_image_override(&self) -> bool {
        self.profile_image_override_id.is_some()
    }

    /// 生效头像：覆盖优先。
    pub fn effective_profile_image_id(&self) -> Option<i64> {
        self.profile_image_override_id.or(self.profile_image_id)
    }

    /// 年龄（周岁）。`birthday` 为空时返回 `None`。
    ///
    /// 与后端 `age` 属性一致：以 UTC 当天为基准，
    /// 且生日尚未到达时减 1。
    pub fn age_on(&self, today: NaiveDate) -> Option<i32> {
        let birthday = self.birthday?;
        let mut age = today.year() - birthday.year();
        if (today.month(), today.day()) < (birthday.month(), birthday.day()) {
            age -= 1;
        }
        Some(age)
    }
}

/// 按 `/` 拆分已存的别名串。
///
/// 对应 `split_actor_alias_name`。注意读时用 `/`，写时用 ` / `。
pub fn split_alias_name(alias_name: &str) -> Vec<&str> {
    alias_name
        .split("/")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// 合并主名与别名，按 `主名 / 别名` 格式输出。
///
/// 对应 `merge_actor_alias_name`。规则：
///
/// 1. 候选顺序：主名 -> 新别名 -> 既有别名串拆分结果。
/// 2. 首尾空白一律去除，空候选跳过。
/// 3. **大小写不敏感去重**，保留首次出现的写法。
/// 4. 主名恒排第一位。
///
/// ```text
/// merge_alias_name("苍井空", &["Aoi", "aoi"], "苍井空 / 空")
/// // => "苍井空 / Aoi / 空"
/// ```
pub fn merge_alias_name(primary_name: &str, alias_names: &[&str], existing: &str) -> String {
    let mut merged: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let push = |candidate: &str, merged: &mut Vec<String>, seen: &mut Vec<String>| {
        let normalized = candidate.trim();
        if normalized.is_empty() {
            return;
        }
        let key = normalized.to_lowercase();
        if seen.iter().any(|existing_key| existing_key == &key) {
            return;
        }
        seen.push(key);
        merged.push(normalized.to_owned());
    };

    push(primary_name, &mut merged, &mut seen);
    for alias in alias_names {
        push(alias, &mut merged, &mut seen);
    }
    for alias in split_alias_name(existing) {
        push(alias, &mut merged, &mut seen);
    }
    merged.join(" / ")
}

/// 沿 `merged_into` 墓碑指针解析到最终保留记录。
///
/// 对应 `Actor.resolve_canonical`。带环检测：指针成环时停在当前记录，
/// 而不是死循环 —— 合并操作可能被并发打断而留下环。
pub fn resolve_canonical_ids<F>(start_id: i64, mut lookup: F) -> Option<i64>
where
    F: FnMut(i64) -> Option<(i64, Option<i64>)>,
{
    let (mut current_id, mut merged_into) = lookup(start_id)?;
    let mut seen = vec![start_id];
    while let Some(next_id) = merged_into {
        if seen.contains(&next_id) {
            return Some(current_id);
        }
        seen.push(next_id);
        match lookup(next_id) {
            Some((id, next)) => {
                current_id = id;
                merged_into = next;
            }
            None => return Some(current_id),
        }
    }
    Some(current_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_actor(birthday: Option<&str>) -> Actor {
        Actor {
            id: 1,
            javdb_id: None,
            name: "n".to_owned(),
            alias_name: String::new(),
            merged_into_id: None,
            profile_image_id: None,
            profile_image_override_id: None,
            display_name_override: None,
            javdb_type: 0,
            gender: 0,
            is_subscribed: false,
            subscribed_at: None,
            subscribed_movies_synced_at: None,
            subscribed_movies_full_synced_at: None,
            birthday: birthday.map(|t| NaiveDate::parse_from_str(t, "%Y-%m-%d").unwrap()),
            height_cm: None,
            bust_cm: None,
            waist_cm: None,
            hips_cm: None,
            cup: None,
            birthplace: None,
            blood_type: None,
            field_owners: serde_json::json!({}),
            mutation_revision: 0,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn protected_fields_match_backend_whitelist() {
        let mut expected = vec![
            "gender", "birthday", "height_cm", "bust_cm", "waist_cm",
            "hips_cm", "cup", "birthplace", "blood_type",
        ];
        expected.sort_unstable();
        let mut actual = PROTECTED_ACTOR_FIELDS.to_vec();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn guarded_fields_extend_beyond_plugin_whitelist() {
        for field in [
            "field_owners",
            "mutation_revision",
            "display_name_override",
            "profile_image_override",
            "merged_into",
        ] {
            assert!(Actor::is_guarded(field), "field={field}");
            assert!(
                !Actor::is_protected(field),
                "{field} 受护栏约束但不在插件白名单内"
            );
        }
    }

    #[test]
    fn gender_allows_only_one_and_two() {
        assert!(Actor::is_valid_gender(GENDER_FEMALE));
        assert!(Actor::is_valid_gender(GENDER_MALE));
        assert!(!Actor::is_valid_gender(GENDER_UNKNOWN));
        assert!(!Actor::is_valid_gender(3));
    }

    #[test]
    fn splits_alias_on_slash() {
        assert_eq!(split_alias_name("A / B / C"), vec!["A", "B", "C"]);
        assert_eq!(split_alias_name("A/B"), vec!["A", "B"]);
        assert_eq!(split_alias_name("   "), Vec::<&str>::new());
        assert_eq!(split_alias_name(""), Vec::<&str>::new());
    }

    #[test]
    fn merge_dedupes_case_insensitively_keeping_first_spelling() {
        assert_eq!(
            merge_alias_name("苍井空", &["Aoi", "aoi", "AOI"], ""),
            "苍井空 / Aoi"
        );
    }

    #[test]
    fn merge_keeps_primary_first_and_skips_blanks() {
        assert_eq!(merge_alias_name("主名", &[], ""), "主名");
        assert_eq!(merge_alias_name("  主名  ", &["别名"], ""), "主名 / 别名");
        assert_eq!(merge_alias_name("", &["", "  "], ""), "");
    }

    #[test]
    fn merge_appends_existing_alias_string() {
        assert_eq!(
            merge_alias_name("新名", &["新别名"], "旧名 / 更旧名"),
            "新名 / 新别名 / 旧名 / 更旧名"
        );
    }

    #[test]
    fn merge_matches_backend_docstring_example() {
        assert_eq!(
            merge_alias_name("苍井空", &["Aoi"], "苍井空 / 空"),
            "苍井空 / Aoi / 空"
        );
    }

    #[test]
    fn age_counts_whole_years_relative_to_today() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        assert_eq!(demo_actor(Some("1990-10-02")).age_on(today), Some(36), "生日当天算满岁");
        assert_eq!(demo_actor(Some("1990-10-03")).age_on(today), Some(35), "生日未到减 1");
        assert_eq!(demo_actor(Some("1990-01-01")).age_on(today), Some(36));
        assert_eq!(demo_actor(None).age_on(today), None);
    }

    #[test]
    fn display_name_prefers_override() {
        let mut actor = demo_actor(None);
        actor.name = "原名".to_owned();
        assert_eq!(actor.display_name(), "原名");
        actor.display_name_override = Some(" 覆盖名 ".to_owned());
        assert_eq!(actor.display_name(), " 覆盖名 ", "未 trim 的覆盖值原样返回");
        actor.display_name_override = Some("   ".to_owned());
        assert_eq!(actor.display_name(), "原名", "纯空白覆盖视为未设置");
    }

    #[test]
    fn effective_profile_image_prefers_override() {
        let mut actor = demo_actor(None);
        actor.profile_image_id = Some(1);
        assert_eq!(actor.effective_profile_image_id(), Some(1));
        assert!(!actor.has_profile_image_override());
        actor.profile_image_override_id = Some(2);
        assert_eq!(actor.effective_profile_image_id(), Some(2));
        assert!(actor.has_profile_image_override());
    }

    #[test]
    fn canonical_follows_merge_chain() {
        let table = [(3i64, Some(2i64)), (2, Some(1)), (1, None)];
        let resolved = resolve_canonical_ids(3, |id| {
            table.iter().find(|(key, _)| *key == id).copied()
        });
        assert_eq!(resolved, Some(1));
    }

    #[test]
    fn canonical_stops_on_cycle_instead_of_looping() {
        let table = [(1i64, Some(2i64)), (2, Some(1))];
        let resolved = resolve_canonical_ids(1, |id| {
            table.iter().find(|(key, _)| *key == id).copied()
        });
        assert!(resolved.is_some(), "成环也要返回结果，不能死循环");
    }

    #[test]
    fn canonical_returns_none_when_start_missing() {
        assert_eq!(resolve_canonical_ids(99, |_| None), None);
    }
}
