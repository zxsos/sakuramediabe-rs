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
///
/// **这些是数据库列名**，不是 Peewee 字段名 —— 两者对多数列恰好相同，
/// 所以差别一直没被发现，直到 `merged_into` / `profile_image_override`
/// 这两列：Peewee 侧叫 `merged_into` / `profile_image_override`，而
/// Peewee 的 `ForeignKeyField(Model)` 在不写 `field=` 时落到
/// `<field>_id`，所以 DDL 里是 `merged_into_id` / `profile_image_override_id`。
///
/// 用错名字的后果不是「编译失败」而是「静默错」：
///
/// - `is_guarded("merged_into_id")` 返回 `false` —— 任何按**列名**询问的
///   调用方（护栏、未来的网关）都会得到错误答案；
/// - 一旦这些字段被纳入 patch，字段名会直接拼进 SQL，于是拼出一个
///   不存在的列。
///
/// `every_guarded_name_is_a_real_column` 这个单测把两者钉在一起。
pub const GUARDED_ACTOR_FIELDS: [&str; 5] = [
    "field_owners",
    "mutation_revision",
    "display_name_override",
    "profile_image_override_id",
    "merged_into_id",
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
    pub id: i32,

    /// JavDB ID。**NOT NULL**。
    ///
    /// 上游是 `CaseSensitiveCharField(max_length=64, unique=True)`，没有
    /// `null=True`，所以这一列不可空。此前声明成 `Option<String>` 并让
    /// 空白归一为 `None`，写入必然违反 NOT NULL 约束。
    pub javdb_id: String,
    pub name: String,
    /// 别名合并后的结果，格式 `"主名 / 别名1 / 别名2"`，去重且主名在首位。
    pub alias_name: String,
    /// 墓碑指针：指向合并后的保留记录。
    pub merged_into_id: Option<i32>,
    pub profile_image_id: Option<i32>,
    /// 本地头像覆盖。优先于 `profile_image_id`。
    pub profile_image_override_id: Option<i32>,
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
    pub fn effective_profile_image_id(&self) -> Option<i32> {
        self.profile_image_override_id.or(self.profile_image_id)
    }

    /// 年龄（周岁）。`birthday` 为空时返回 `None`。
    ///
    /// 与后端 `age` 属性一致：以 UTC 当天为基准，
    /// 且生日尚未到达时减 1。
    ///
    /// 委托给 [`age_for_birthday`] —— 年龄算法必须**只有一份**：列表筛选的
    /// `age_min`/`age_max` 走 [`years_before`]、筛选项聚合走那个函数、本方法是
    /// 第三处。三处各算一次的话，将来某处被「顺手优化」，筛出来的年龄就会
    /// 与筛选项显示的区间对不上，而两端都不报错。
    pub fn age_on(&self, today: NaiveDate) -> Option<i32> {
        self.birthday
            .map(|birthday| age_for_birthday(birthday, today))
    }
}

/// 年龄（周岁）。`today` 当天算满岁，生日未到则减 1。
///
/// 对应上游 `ActorService._age_for_birthday`。
pub fn age_for_birthday(birthday: NaiveDate, today: NaiveDate) -> i32 {
    let mut age = today.year() - birthday.year();
    if (today.month(), today.day()) < (birthday.month(), birthday.day()) {
        age -= 1;
    }
    age
}

/// `today` 往前推 `years` 年的同月同日，**日**钳位到该月最后一天。
///
/// 对应上游 `ActorService._years_before`。它是年龄筛选的逆运算：
///
/// ```text
/// age >= age_min   <=>   birthday <= years_before(today, age_min)
/// age <= age_max   <=>   birthday >  years_before(today, age_max + 1)
/// ```
///
/// 上游两条边界都写成「生日 ≤ 某个日期」，靠 `age_max + 1` 把上界变成
/// 开区间 —— 这样 2 月 29 日生日的人不会在 `age_max` 那一档凭空消失。
///
/// # 钳位不是边角情况
///
/// `2024-02-29` 往前一年是 `2023-02-28`。不钳位就会得到一个**不存在**的日期，
/// `birthday <= 2023-02-29` 恰好仍然成立（无害），但纯函数被直接断言时会
/// 得到 2 月 29 日 —— 与上游不一致，且这种偏差要到「筛选结果少了一行」才
/// 看得见。所以这里显式取 `min(day, 该月天数)`。
///
/// # `year <= 0` 不可达
///
/// `today` 来自 `utc_now_for_db().date()`，年份恒为正；调用方（列表筛选）
/// 传入的 `years` 是 `age_min` / `age_max + 1`，由端点的 `ge=0` 保证非负。
/// 真的传进来一个超大 `years` 会让 `from_ymd_opt` 返回 `None`，那时 panic
/// 比返回错误更诚实：那是编程错误，不是用户输入错误。
pub fn years_before(today: NaiveDate, years: i32) -> NaiveDate {
    let year = today.year() - years;
    let month = today.month();
    // `month` 来自合法日期，所以恒落在 1..=12。
    let day = today.day().min(days_in_month(year, month));
    NaiveDate::from_ymd_opt(year, month, day).unwrap_or_else(|| {
        panic!("{year}-{month}-{day} 不是合法日期：year/month 来自 today，day 已钳位到该月天数")
    })
}

/// 该年该月有几天。对应 Python `calendar.monthrange(year, month)[1]`。
fn days_in_month(year: i32, month: u32) -> u32 {
    // 第 0 项是「本月之前」的哨兵，让下标直接等于月份号。
    const LENGTHS: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let length = LENGTHS[month as usize];
    if length == 28 && is_leap_year(year) {
        29
    } else {
        length
    }
}

/// 闰年判定。`year == 0` 按公历视为闰年 —— 该分支不可达，理由同 [`years_before`]。
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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
///
/// 泛型而非写死 `i32`：墓碑链的遍历逻辑与 id 的具体宽度无关，
/// 调用方可以传闭包做数据库查询。
pub fn resolve_canonical_ids<T, F>(start_id: T, mut lookup: F) -> Option<T>
where
    T: Copy + PartialEq,
    F: FnMut(T) -> Option<(T, Option<T>)>,
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
            // 断链：指针指向的行不存在。
            //
            // 返回 `None` 而不是 `Some(current_id)` —— 与
            // `ActorRepository::resolve_canonical` 的行为一致。此前两处
            // 相反（仓储返回 None、这里返回最后一跳），而没有任何测试能
            // 发现：`foreign_key_forbids_a_dangling_tombstone_pointer`
            // 证明这种数据形状进不来，其余调用方也没走到这里。
            //
            // 「无法确认这是终点」与「这就是终点」必须区分。返回最后一跳
            // 会让调用方把一个墓碑当成保留记录 —— 那是数据损坏被当成
            // 正常结果，比报错更难排查。
            None => return None,
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
            javdb_id: "n".to_owned(),
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
        expected.sort_unstable();
        let mut actual = PROTECTED_ACTOR_FIELDS.to_vec();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn guarded_fields_extend_beyond_plugin_whitelist() {
        // 遍历**常量本身**，不要把名字重抄一遍。
        //
        // 此前这里硬编码了一份与常量相同的列表，于是两处**同时**是错的
        // （用的是 Peewee 字段名 `merged_into` 而不是列名
        // `merged_into_id`），测试照样通过。抄一份列表，就等于把
        // 「名字对不对」这个断言本身交给了同一份可能错的东西。
        for field in GUARDED_ACTOR_FIELDS {
            assert!(Actor::is_guarded(field), "field={field}");
            assert!(
                !Actor::is_protected(field),
                "{field} 受护栏约束但不在插件白名单内"
            );
        }
    }

    #[test]
    fn every_guarded_name_is_a_real_column() {
        // Peewee 字段名与 DDL 列名对多数列恰好相同，所以差别一直没暴露。
        // 这里逐个对照 DDL，确保常量里每一个名字都是真实存在的列。
        let ddl = include_str!("../../../../docker/schema.sql");
        let actor_ddl = ddl
            .split("CREATE TABLE IF NOT EXISTS actor (")
            .nth(1)
            .and_then(|rest| rest.split("\n);").next())
            .expect("schema.sql 里应有 actor 表");

        for field in GUARDED_ACTOR_FIELDS
            .iter()
            .chain(PROTECTED_ACTOR_FIELDS.iter())
        {
            assert!(
                actor_ddl
                    .lines()
                    .any(|line| line.trim_start().starts_with(field)),
                "{field} 不在 actor 表的 DDL 里 —— 护栏与网关会把字段名直接拼进 SQL，\
                 拼出不存在的列只在运行时才报错"
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
        assert_eq!(
            demo_actor(Some("1990-10-02")).age_on(today),
            Some(36),
            "生日当天算满岁"
        );
        assert_eq!(
            demo_actor(Some("1990-10-03")).age_on(today),
            Some(35),
            "生日未到减 1"
        );
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
        let resolved =
            resolve_canonical_ids(3, |id| table.iter().find(|(key, _)| *key == id).copied());
        assert_eq!(resolved, Some(1));
    }

    #[test]
    fn canonical_stops_on_cycle_instead_of_looping() {
        let table = [(1i64, Some(2i64)), (2, Some(1))];
        let resolved =
            resolve_canonical_ids(1, |id| table.iter().find(|(key, _)| *key == id).copied());
        assert!(resolved.is_some(), "成环也要返回结果，不能死循环");
    }

    #[test]
    fn canonical_returns_none_when_start_missing() {
        assert_eq!(resolve_canonical_ids(99, |_| None), None);
    }

    #[test]
    fn years_before_clamps_the_day_to_the_target_month_length() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        // 同月同日，最常见的情形
        assert_eq!(
            years_before(today, 1),
            NaiveDate::from_ymd_opt(2025, 10, 4).unwrap()
        );
        // 2 月 29 日往前一年 → 2 月 28 日（2025 不是闰年）
        assert_eq!(
            years_before(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(), 1),
            NaiveDate::from_ymd_opt(2023, 2, 28).unwrap()
        );
        // 3 月 31 日往前一个月等价物（years=0 时是恒等）
        assert_eq!(
            years_before(NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(), 0),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()
        );
        // 31 日往前推落到 30 天月份：2026-05-31 往前一年 → 2025-05-31（5 月有 31 天）
        assert_eq!(
            years_before(NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(), 1),
            NaiveDate::from_ymd_opt(2025, 5, 31).unwrap()
        );
        // 钳位**只在 2 月生效**：月份不随年份变化，所以唯一「这个月 28 天、
        // 那个月 29 天」的月份是 2 月。上游的 `min(day, monthrange(...)[1])`
        // 对其他月份是恒等操作 —— 这里把这条钉住，免得有人「简化」掉它
        // 之后以为漏掉了什么分支。
        for probe in ["2025-05-31", "2025-01-31", "2024-03-31"] {
            let today = NaiveDate::parse_from_str(probe, "%Y-%m-%d").unwrap();
            assert_eq!(
                years_before(today, 1),
                NaiveDate::parse_from_str(probe, "%Y-%m-%d")
                    .unwrap()
                    .with_year(today.year() - 1)
                    .unwrap(),
                "{probe} 往前一年不该动月与日"
            );
        }
    }

    #[test]
    fn years_before_zero_is_the_identity() {
        // age_max + 1 之类的边界会算出 years=0，此时必须原样返回
        for probe in ["2026-10-04", "2024-02-29", "2026-01-31"] {
            let today = NaiveDate::parse_from_str(probe, "%Y-%m-%d").unwrap();
            assert_eq!(years_before(today, 0), today, "{probe}");
        }
    }

    #[test]
    fn age_and_years_before_agree_on_the_filter_boundary() {
        // 这两个函数是**互逆**的：筛选项说「age_min = 30」时，一个 30 岁的人
        // 必须恰好落在边界上（<=），而 31 岁的人必须落在外面。
        //
        // 少测这一条的话，`years_before` 与 `age_for_birthday` 各改一处就会
        // 让「年龄筛选」与「筛选项区间」静默错开一档。
        let today = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        let cutoff = years_before(today, 30);

        // 恰好 30 岁：生日 == 边界当天，命中（上游是 <=）
        let exactly_30 = NaiveDate::from_ymd_opt(1996, 10, 4).unwrap();
        assert_eq!(age_for_birthday(exactly_30, today), 30);
        assert!(exactly_30 <= cutoff, "边界当天必须命中");

        // 30 岁零一天：生日比边界晚一天，不命中
        let just_under = NaiveDate::from_ymd_opt(1996, 10, 5).unwrap();
        assert_eq!(age_for_birthday(just_under, today), 29);
        assert!(just_under > cutoff);

        // 2 月 29 日生日在非闰年 3 月 1 日之后仍算其实际周岁
        let leap = NaiveDate::from_ymd_opt(2000, 2, 29).unwrap();
        assert_eq!(age_for_birthday(leap, today), 26);
    }
}
