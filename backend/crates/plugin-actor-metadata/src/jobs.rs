//! 每天筛选到期演员；完成、耗尽次数的演员不再访问来源。
//!
//! # 上游对应：`jobs.py`
//!
//! 逐条照搬：`missing_fields` / `is_empty_field_value` / `writable_fields` /
//! `iter_actor_snapshot_batches` / `prioritize_actor_snapshots` / `process`。
//!
//! # 两处与上游不同
//!
//! 1. **宿主调用走 gRPC**。上游是进程内 `context.actors` / `context.movies`；
//!    这里是 [`ActorHost`] trait —— 生产实现连 `SAKURAMEDIA_HOST_GRPC_ADDR`
//!    的 `PluginHost`（见 `service.rs`），测试用内存假实现。
//! 2. **`writable_fields` 的归属检查读 `field_owners`**。「缺键 = 无人接管 =
//!    可写、记着别人 = 受保护」照上游 `snapshot.owners.get(key)` 的语义来。
//!    （早期契约只给「去重后的 owner 列表」，字段级归属拿不到，当时退化成
//!    「只判值为空」；现在 `ActorSnapshot.field_owners` 把映射带出来了。）

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use prost_types::Value as ProtoValue;

use crate::settings::Settings;
use crate::sources::{
    normalize_fields, FieldValue, Profile, Raw, SourceError, Sources, PROFILE_FIELDS,
};
use crate::state::{Provenance, State, StateError};

/// 连续网络异常几次后停掉本轮（上游 `MAX_CONSECUTIVE_NETWORK_ERRORS`）。
pub const MAX_CONSECUTIVE_NETWORK_ERRORS: i64 = 3;
/// 分页大小（上游 `limit=500`）。
pub const PAGE_LIMIT: i32 = 500;

/// 宿主侧演员/影片快照的最小视图（`PluginHost` 返回的子集）。
#[derive(Debug, Clone, Default)]
pub struct HostActor {
    pub actor_id: i64,
    pub revision: i64,
    pub values: BTreeMap<String, HostValue>,
    /// 字段级归属（`{字段: owner}`）。**无主的字段不在这里** —— 那正是「可写」。
    pub field_owners: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct HostMovie {
    pub movie_id: i64,
    pub values: BTreeMap<String, HostValue>,
    pub actor_ids: Vec<i64>,
}

/// 快照里的字段值（`google.protobuf.Value` 的子集）。
#[derive(Debug, Clone, PartialEq)]
pub enum HostValue {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

impl HostValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn from_proto(value: &ProtoValue) -> Self {
        use prost_types::value::Kind;
        match &value.kind {
            Some(Kind::StringValue(s)) => Self::Str(s.clone()),
            Some(Kind::NumberValue(n)) => Self::Num(*n),
            Some(Kind::BoolValue(b)) => Self::Bool(*b),
            _ => Self::Null,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Str(s) => s.is_empty(),
            Self::Null => true,
            _ => false,
        }
    }
}

/// 宿主能力（上游 `context.actors` / `context.movies` 的 gRPC 对等物）。
#[async_trait]
pub trait ActorHost: Send {
    async fn list_actors(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostActor>, Option<i64>), HostError>;
    async fn get_actor(&mut self, actor_id: i64) -> Result<Option<HostActor>, HostError>;
    async fn patch_actor(
        &mut self,
        actor_id: i64,
        fields: &BTreeMap<String, FieldValue>,
        expected_revision: i64,
    ) -> Result<bool, HostError>;
    async fn list_movies(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostMovie>, Option<i64>), HostError>;
}

/// 资料来源（上游 `Sources` 的 trait 化：测试用假实现）。
#[async_trait]
pub trait ProfileSource: Send {
    async fn javdb(&mut self, javdb_id: &str) -> Result<Profile, SourceError>;
    async fn minnanoav(
        &mut self,
        names: &[String],
        reference: &str,
    ) -> Result<Profile, SourceError>;
}

#[async_trait]
impl ProfileSource for Sources {
    async fn javdb(&mut self, javdb_id: &str) -> Result<Profile, SourceError> {
        Sources::javdb(self, javdb_id).await
    }

    async fn minnanoav(
        &mut self,
        names: &[String],
        reference: &str,
    ) -> Result<Profile, SourceError> {
        Sources::minnanoav(self, names, reference).await
    }
}

/// 宿主调用失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError(pub String);

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostError {}

/// 任务统计（上游 `process` 返回的 stats dict）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub scanned: i64,
    pub attempted: i64,
    pub updated: i64,
    pub completed: i64,
    pub stopped: i64,
    pub waiting: i64,
    pub skipped: i64,
    pub errors: i64,
    pub aborted: i64,
}

impl Stats {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "scanned": self.scanned,
            "attempted": self.attempted,
            "updated": self.updated,
            "completed": self.completed,
            "stopped": self.stopped,
            "waiting": self.waiting,
            "skipped": self.skipped,
            "errors": self.errors,
            "aborted": self.aborted,
        })
    }
}

/// 进度回调（上游 `reporter.progress_callback`）。
pub trait ProgressReporter: Send {
    fn report(&mut self, current: i64, total: i64, text: String);
}

fn report<R: ProgressReporter>(
    reporter: &mut Option<&mut R>,
    current: i64,
    total: i64,
    text: String,
) {
    if let Some(r) = reporter {
        r.report(current, total, text);
    }
}

/// 上游 `missing_fields`：归一化后仍缺的统一字段。
fn missing_fields(values: &BTreeMap<String, HostValue>) -> Vec<String> {
    // 先把数值转成字符串（上游 `str(raw)` 的对等物），再统一走文本归一化。
    let owned: BTreeMap<String, String> = values
        .iter()
        .filter_map(|(k, v)| match v {
            HostValue::Str(s) => Some((k.clone(), s.clone())),
            HostValue::Num(n) if n.fract() == 0.0 => Some((k.clone(), (*n as i64).to_string())),
            HostValue::Num(n) => Some((k.clone(), n.to_string())),
            _ => None,
        })
        .collect();
    let raws: BTreeMap<&'static str, Raw<'_>> = PROFILE_FIELDS
        .iter()
        .map(|key| {
            let raw = match owned.get(*key) {
                Some(s) => Raw::Text(s.as_str()),
                None => match values.get(*key) {
                    Some(HostValue::Bool(b)) => Raw::Bool(*b),
                    _ => Raw::Missing,
                },
            };
            (*key, raw)
        })
        .collect();
    let valid = normalize_fields(&raws);
    PROFILE_FIELDS
        .iter()
        .filter(|key| !valid.contains_key(**key))
        .map(|key| key.to_string())
        .collect()
}

/// 上游 `is_empty_field_value`。
fn is_empty_field_value(name: &str, value: Option<&HostValue>) -> bool {
    match value {
        None => true,
        Some(v) if v.is_empty() => true,
        Some(HostValue::Num(n)) if name == "gender" && *n == 0.0 => true,
        _ => false,
    }
}

/// 上游 `writable_fields`：值还空着 + 字段没被别人接管。
fn writable_fields(snapshot: &HostActor, missing: &[String], owner: &str) -> Vec<String> {
    missing
        .iter()
        .filter(|key| is_empty_field_value(key, snapshot.values.get(*key)))
        // 归属：缺键 = 无人接管（可写）；记着自己 = 可写；记着别人 = 受保护。
        .filter(|key| match snapshot.field_owners.get(*key) {
            None => true,
            Some(current) => current == owner,
        })
        .cloned()
        .collect()
}

/// 把 [`FieldValue`] 转成 protobuf `Value`（`PatchActor` 用）。
pub fn field_to_proto(value: &FieldValue) -> ProtoValue {
    use prost_types::value::Kind;
    let kind = match value {
        FieldValue::Text(s) => Kind::StringValue(s.clone()),
        FieldValue::Int(i) => Kind::NumberValue(*i as f64),
    };
    ProtoValue { kind: Some(kind) }
}

/// 任务主循环（上游 `process`）。
///
/// `owner` 是 `plugin:{plugin_id}`（字段归属，见模块文档）。
/// `now` 是秒级时间戳（测试可注入）。
#[allow(clippy::too_many_arguments)]
pub async fn process<H, S, R>(
    host: &mut H,
    sources: &mut S,
    state: &State,
    settings: &Settings,
    owner: &str,
    mut reporter: Option<&mut R>,
    now: f64,
) -> Result<Stats, JobError>
where
    H: ActorHost,
    S: ProfileSource,
    R: ProgressReporter,
{
    let mut stats = Stats::default();
    let mut consecutive_network_errors: i64 = 0;

    // 全量演员快照。
    let mut snapshots = Vec::new();
    let mut after_id = 0;
    loop {
        let (items, next) = host
            .list_actors(after_id, PAGE_LIMIT)
            .await
            .map_err(|e| JobError::Host(e.0))?;
        snapshots.extend(items);
        match next {
            Some(cursor) => after_id = cursor,
            None => break,
        }
    }
    let total = snapshots.len() as i64;

    // 优先级：已订阅优先，影片数多的优先（上游 `prioritize_actor_snapshots`）。
    let mut movie_counts: HashMap<i64, i64> = snapshots.iter().map(|s| (s.actor_id, 0)).collect();
    let mut scanned_movies = 0;
    let mut movie_after = 0;
    loop {
        let (movies, next) = host
            .list_movies(movie_after, PAGE_LIMIT)
            .await
            .map_err(|e| JobError::Host(e.0))?;
        for movie in &movies {
            if movie
                .values
                .get("is_collection")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            for actor_id in &movie.actor_ids {
                if let Some(count) = movie_counts.get_mut(actor_id) {
                    *count += 1;
                }
            }
        }
        scanned_movies += movies.len();
        report(
            &mut reporter,
            0,
            total,
            format!("正在计算演员优先级，已扫描 {scanned_movies} 部影片"),
        );
        match next {
            Some(cursor) => movie_after = cursor,
            None => break,
        }
    }
    snapshots.sort_by(|a, b| {
        let a_sub = a
            .values
            .get("is_subscribed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let b_sub = b
            .values
            .get("is_subscribed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        (!a_sub)
            .cmp(&(!b_sub))
            .then_with(|| {
                movie_counts
                    .get(&b.actor_id)
                    .unwrap_or(&0)
                    .cmp(movie_counts.get(&a.actor_id).unwrap_or(&0))
            })
            .then_with(|| a.actor_id.cmp(&b.actor_id))
    });

    let report_progress = |rep: &mut Option<&mut R>, stats: &Stats| {
        report(
            rep,
            stats.scanned,
            total,
            format!(
                "已扫描 {}/{} 位，尝试 {} 位，补齐 {} 位，停止 {} 位",
                stats.scanned, total, stats.attempted, stats.completed, stats.stopped
            ),
        );
    };
    report(
        &mut reporter,
        0,
        total,
        format!("开始扫描演员，共 {total} 位"),
    );

    for snapshot in &snapshots {
        stats.scanned += 1;
        let gender = snapshot.values.get("gender").and_then(|v| v.as_f64());
        let subscribed = snapshot
            .values
            .get("is_subscribed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if gender == Some(2.0) || (settings.subscribed_only && !subscribed) {
            stats.skipped += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        let javdb_id = snapshot
            .values
            .get("javdb_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let mut row = state
            .get(snapshot.actor_id, &javdb_id)
            .map_err(JobError::State)?;
        if matches!(row.status.as_str(), "completed" | "stopped") {
            stats.skipped += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        let missing = missing_fields(&snapshot.values);
        row.missing_fields = missing.clone();
        if missing.is_empty() {
            row.status = "completed".to_owned();
            row.next_attempt_at = None;
            row.reason = "complete".to_owned();
            state.save(&row).map_err(JobError::State)?;
            stats.completed += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        if row.attempts >= settings.max_attempts as i64 {
            row.status = "stopped".to_owned();
            row.next_attempt_at = None;
            if row.reason.is_empty() {
                row.reason = "attempt_limit".to_owned();
            }
            state.save(&row).map_err(JobError::State)?;
            stats.stopped += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        if row.next_attempt_at.is_some_and(|t| t > now) {
            stats.skipped += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        let writable = writable_fields(snapshot, &missing, owner);
        if writable.is_empty() {
            row.status = "stopped".to_owned();
            row.next_attempt_at = None;
            row.reason = "fields_not_writable".to_owned();
            state.save(&row).map_err(JobError::State)?;
            stats.stopped += 1;
            report_progress(&mut reporter, &stats);
            continue;
        }
        // 请求前提交次数，进程被终止后也不能绕开上限。
        row.status = "waiting".to_owned();
        row.attempts += 1;
        row.last_attempt_at = Some(now);
        row.next_attempt_at = Some(now + settings.retry_days as f64 * 86400.0);
        row.reason = "interrupted".to_owned();
        state.save(&row).map_err(JobError::State)?;
        stats.attempted += 1;

        let mut updates: BTreeMap<String, FieldValue> = BTreeMap::new();
        let mut provenance: BTreeMap<String, Provenance> = BTreeMap::new();
        let mut outcomes: Vec<String> = Vec::new();
        let mut javdb_male = false;
        let mut aborted = false;
        let mut names: Vec<String> = Vec::new();
        if let Some(name) = snapshot.values.get("name").and_then(|v| v.as_str()) {
            if !name.trim().is_empty() {
                names.push(name.trim().to_owned());
            }
        }
        if let Some(alias) = snapshot.values.get("alias_name").and_then(|v| v.as_str()) {
            for part in alias.split([',', '，', '、', ';', '；', '/', '／']) {
                let part = part.trim();
                if !part.is_empty() && !names.contains(&part.to_owned()) {
                    names.push(part.to_owned());
                }
            }
        }

        let fetch: Result<(), SourceError> = async {
            if !javdb_id.is_empty() {
                let profile = sources.javdb(&javdb_id).await?;
                javdb_male = profile.fields.get("gender").and_then(|v| v.as_i64()) == Some(2);
                for name in profile.names.iter().rev() {
                    if !names.contains(name) {
                        names.insert(0, name.clone());
                    }
                }
                outcomes.push(format!("javdb:{}", profile.outcome));
                for (key, value) in &profile.fields {
                    if writable.contains(key) {
                        updates.insert(key.clone(), value.clone());
                        provenance.insert(
                            key.clone(),
                            Provenance {
                                url: profile.reference.clone(),
                                fetched_at: now,
                            },
                        );
                    }
                }
            }
            let still_missing = writable.iter().any(|k| !updates.contains_key(k));
            if !javdb_male && still_missing {
                let profile = sources.minnanoav(&names, &row.minnanoav_ref).await?;
                outcomes.push(format!("minnanoav:{}", profile.outcome));
                match profile.outcome.as_str() {
                    "ok" => {
                        row.minnanoav_ref = profile.reference.clone();
                        for (key, value) in &profile.fields {
                            if writable.contains(key) && !updates.contains_key(key) {
                                updates.insert(key.clone(), value.clone());
                                provenance.insert(
                                    key.clone(),
                                    Provenance {
                                        url: format!(
                                            "https://www.minnano-av.com{}",
                                            profile.reference
                                        ),
                                        fetched_at: now,
                                    },
                                );
                            }
                        }
                    }
                    "not_found" | "identity_mismatch" => {
                        row.minnanoav_ref = String::new();
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        match fetch {
            Ok(()) => consecutive_network_errors = 0,
            Err(e) => {
                outcomes.push(e.0.clone());
                stats.errors += 1;
                if e.0.ends_with(":network_error") {
                    consecutive_network_errors += 1;
                    aborted = consecutive_network_errors >= MAX_CONSECUTIVE_NETWORK_ERRORS;
                } else {
                    consecutive_network_errors = 0;
                }
            }
        }

        // 网络请求期间宿主资料可能被其他任务更新，重新读取再执行乐观写入。
        let latest = host
            .get_actor(snapshot.actor_id)
            .await
            .map_err(|e| JobError::Host(e.0))?;
        if latest.as_ref().is_none_or(|a| {
            a.values
                .get("javdb_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                != javdb_id
        }) {
            row.status = "stopped".to_owned();
            row.reason = "actor_changed_or_deleted".to_owned();
            row.next_attempt_at = None;
            stats.stopped += 1;
        } else if let Some(latest) = latest {
            let allowed = writable_fields(&latest, &missing_fields(&latest.values), owner);
            let patch: BTreeMap<String, FieldValue> = updates
                .iter()
                .filter(|(k, _)| allowed.contains(*k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut latest_snapshot = latest;
            if !patch.is_empty() {
                let revision = latest_snapshot.revision;
                if host
                    .patch_actor(snapshot.actor_id, &patch, revision)
                    .await
                    .map_err(|e| JobError::Host(e.0))?
                {
                    stats.updated += 1;
                    for key in patch.keys() {
                        if let Some(p) = provenance.get(key) {
                            row.sources.insert(key.clone(), p.clone());
                        }
                    }
                } else {
                    outcomes.push("patch_conflict".to_owned());
                }
                latest_snapshot = host
                    .get_actor(snapshot.actor_id)
                    .await
                    .map_err(|e| JobError::Host(e.0))?
                    .unwrap_or(latest_snapshot);
            }
            row.missing_fields = missing_fields(&latest_snapshot.values);
            row.reason = if outcomes.is_empty() {
                "no_data".to_owned()
            } else {
                outcomes.join(";")
            };
            if row.missing_fields.is_empty() {
                row.status = "completed".to_owned();
                row.next_attempt_at = None;
                row.reason = "complete".to_owned();
                stats.completed += 1;
            } else if row.attempts >= settings.max_attempts as i64 {
                row.status = "stopped".to_owned();
                row.next_attempt_at = None;
                stats.stopped += 1;
            } else {
                row.next_attempt_at = Some(now + settings.retry_days as f64 * 86400.0);
                stats.waiting += 1;
            }
        }
        state.save(&row).map_err(JobError::State)?;
        report_progress(&mut reporter, &stats);
        if aborted {
            stats.aborted = 1;
            return Ok(stats);
        }
    }
    Ok(stats)
}

/// 任务失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    Host(String),
    State(StateError),
    Sources(SourceError),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(e) => write!(f, "宿主调用失败：{e}"),
            Self::State(e) => write!(f, "状态文件失败：{e}"),
            Self::Sources(e) => write!(f, "资料来源失败：{e}"),
        }
    }
}

impl std::error::Error for JobError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::Profile;
    use std::collections::HashMap;

    /// 内存假宿主。
    struct FakeHost {
        actors: HashMap<i64, HostActor>,
        movies: Vec<HostMovie>,
        patched: Vec<(i64, BTreeMap<String, FieldValue>)>,
    }

    fn actor(id: i64, values: &[(&str, HostValue)]) -> HostActor {
        HostActor {
            actor_id: id,
            revision: 1,
            values: values
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            field_owners: BTreeMap::new(),
        }
    }

    #[async_trait]
    impl ActorHost for FakeHost {
        async fn list_actors(
            &mut self,
            after_id: i64,
            limit: i32,
        ) -> Result<(Vec<HostActor>, Option<i64>), HostError> {
            let mut items: Vec<HostActor> = self
                .actors
                .values()
                .filter(|a| a.actor_id > after_id)
                .cloned()
                .collect();
            items.sort_by_key(|a| a.actor_id);
            items.truncate(limit as usize);
            let next = items.last().map(|a| a.actor_id);
            let done = items.len() < limit as usize;
            Ok((items, if done { None } else { next }))
        }

        async fn get_actor(&mut self, actor_id: i64) -> Result<Option<HostActor>, HostError> {
            Ok(self.actors.get(&actor_id).cloned())
        }

        async fn patch_actor(
            &mut self,
            actor_id: i64,
            fields: &BTreeMap<String, FieldValue>,
            expected_revision: i64,
        ) -> Result<bool, HostError> {
            let Some(actor) = self.actors.get_mut(&actor_id) else {
                return Ok(false);
            };
            if actor.revision != expected_revision {
                return Ok(false);
            }
            for (k, v) in fields {
                actor.values.insert(
                    k.clone(),
                    match v {
                        FieldValue::Text(s) => HostValue::Str(s.clone()),
                        FieldValue::Int(i) => HostValue::Num(*i as f64),
                    },
                );
            }
            actor.revision += 1;
            self.patched.push((actor_id, fields.clone()));
            Ok(true)
        }

        async fn list_movies(
            &mut self,
            after_id: i64,
            limit: i32,
        ) -> Result<(Vec<HostMovie>, Option<i64>), HostError> {
            let mut items: Vec<HostMovie> = self
                .movies
                .iter()
                .filter(|m| m.movie_id > after_id)
                .cloned()
                .collect();
            items.sort_by_key(|m| m.movie_id);
            items.truncate(limit as usize);
            let next = items.last().map(|m| m.movie_id);
            let done = items.len() < limit as usize;
            Ok((items, if done { None } else { next }))
        }
    }

    /// 内存假来源。
    struct FakeSources {
        javdb: HashMap<String, Profile>,
        minnanoav: HashMap<String, Profile>,
    }

    #[async_trait]
    impl ProfileSource for FakeSources {
        async fn javdb(&mut self, javdb_id: &str) -> Result<Profile, SourceError> {
            Ok(self
                .javdb
                .get(javdb_id)
                .cloned()
                .unwrap_or_else(|| Profile::outcome("not_found")))
        }

        async fn minnanoav(
            &mut self,
            names: &[String],
            _reference: &str,
        ) -> Result<Profile, SourceError> {
            let key = names.first().cloned().unwrap_or_default();
            Ok(self
                .minnanoav
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Profile::outcome("not_found")))
        }
    }

    struct NoopReporter;
    impl ProgressReporter for NoopReporter {
        fn report(&mut self, _current: i64, _total: i64, _text: String) {}
    }

    fn test_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let state = State::open(&dir.path().join("actor_metadata.sqlite3")).unwrap();
        (state, dir)
    }

    fn settings() -> Settings {
        Settings {
            retry_days: 3,
            max_attempts: 3,
            subscribed_only: false,
            request_interval_seconds: 0.0,
            timeout_seconds: 20,
            javdb_base_url: url::Url::parse("https://javdb.example/").unwrap(),
            minnanoav_base_url: url::Url::parse("https://minnanoav.example/").unwrap(),
        }
    }

    #[tokio::test]
    async fn full_profile_from_javdb_is_patched_and_completed() {
        let (state, _dir) = test_state();
        let mut host = FakeHost {
            actors: HashMap::from([(
                1,
                actor(
                    1,
                    &[
                        ("name", HostValue::Str("河北彩花".to_owned())),
                        ("javdb_id", HostValue::Str("abc".to_owned())),
                    ],
                ),
            )]),
            movies: vec![],
            patched: vec![],
        };
        let mut fields = BTreeMap::new();
        fields.insert(
            "birthday".to_owned(),
            FieldValue::Text("1999-04-19".to_owned()),
        );
        fields.insert("height_cm".to_owned(), FieldValue::Int(169));
        fields.insert("bust_cm".to_owned(), FieldValue::Int(88));
        fields.insert("waist_cm".to_owned(), FieldValue::Int(58));
        fields.insert("hips_cm".to_owned(), FieldValue::Int(89));
        fields.insert("cup".to_owned(), FieldValue::Text("D".to_owned()));
        fields.insert(
            "birthplace".to_owned(),
            FieldValue::Text("東京都".to_owned()),
        );
        fields.insert("blood_type".to_owned(), FieldValue::Text("A".to_owned()));
        fields.insert("gender".to_owned(), FieldValue::Int(1));
        let mut sources = FakeSources {
            javdb: HashMap::from([(
                "abc".to_owned(),
                Profile {
                    fields,
                    names: vec!["河北彩花".to_owned()],
                    reference: "https://javdb.example/api/v1/actors/abc".to_owned(),
                    outcome: "ok".to_owned(),
                },
            )]),
            minnanoav: HashMap::new(),
        };
        let stats = process(
            &mut host,
            &mut sources,
            &state,
            &settings(),
            "plugin:sakuramedia_actor_metadata",
            Some(&mut NoopReporter),
            1_700_000_000.0,
        )
        .await
        .unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.attempted, 1);
        assert_eq!(stats.updated, 1);
        assert_eq!(stats.completed, 1);
        // 9 个字段一次写进。
        assert_eq!(host.patched.len(), 1);
        assert_eq!(host.patched[0].1.len(), 9);
        // 状态落终态。
        let row = state.get(1, "abc").unwrap();
        assert_eq!(row.status, "completed");
    }

    #[tokio::test]
    async fn male_actors_are_skipped_without_consuming_attempts() {
        let (state, _dir) = test_state();
        let mut host = FakeHost {
            actors: HashMap::from([(
                2,
                actor(
                    2,
                    &[
                        ("name", HostValue::Str("男優".to_owned())),
                        ("gender", HostValue::Num(2.0)),
                    ],
                ),
            )]),
            movies: vec![],
            patched: vec![],
        };
        let mut sources = FakeSources {
            javdb: HashMap::new(),
            minnanoav: HashMap::new(),
        };
        let stats = process(
            &mut host,
            &mut sources,
            &state,
            &settings(),
            "plugin:sakuramedia_actor_metadata",
            Some(&mut NoopReporter),
            1_700_000_000.0,
        )
        .await
        .unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.attempted, 0);
        assert!(host.patched.is_empty());
    }

    #[tokio::test]
    async fn attempt_limit_stops_the_actor() {
        let (state, _dir) = test_state();
        let mut host = FakeHost {
            actors: HashMap::from([(3, actor(3, &[("name", HostValue::Str("無名".to_owned()))]))]),
            movies: vec![],
            patched: vec![],
        };
        // 来源永远 not_found：每次都算一次尝试。
        let mut sources = FakeSources {
            javdb: HashMap::new(),
            minnanoav: HashMap::new(),
        };
        let mut settings = settings();
        settings.max_attempts = 2;
        // 时间推进：每次调用都要越过上次的 next_attempt_at（retry_days=3 天）。
        let day = 86400.0;
        for i in 0..2 {
            let stats = process(
                &mut host,
                &mut sources,
                &state,
                &settings,
                "plugin:sakuramedia_actor_metadata",
                Some(&mut NoopReporter),
                1_700_000_000.0 + i as f64 * 4.0 * day,
            )
            .await
            .unwrap();
            assert_eq!(stats.attempted, 1);
        }
        let row = state.get(3, "").unwrap();
        assert_eq!(row.attempts, 2);
        assert_eq!(row.status, "stopped");
        // 第三次：已是终态，直接跳过，不再请求。
        let stats = process(
            &mut host,
            &mut sources,
            &state,
            &settings,
            "plugin:sakuramedia_actor_metadata",
            Some(&mut NoopReporter),
            1_800_000_000.0,
        )
        .await
        .unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.attempted, 0);
    }

    #[tokio::test]
    async fn consecutive_network_errors_abort_the_round() {
        struct FailingSources;
        #[async_trait]
        impl ProfileSource for FailingSources {
            async fn javdb(&mut self, _id: &str) -> Result<Profile, SourceError> {
                Err(SourceError("javdb:network_error".to_owned()))
            }
            async fn minnanoav(&mut self, _n: &[String], _r: &str) -> Result<Profile, SourceError> {
                Err(SourceError("minnanoav:network_error".to_owned()))
            }
        }
        let (state, _dir) = test_state();
        let mut host = FakeHost {
            actors: (1..=5)
                .map(|id| {
                    (
                        id,
                        actor(id, &[("name", HostValue::Str(format!("演员{id}")))]),
                    )
                })
                .collect(),
            movies: vec![],
            patched: vec![],
        };
        let mut sources = FailingSources;
        let stats = process(
            &mut host,
            &mut sources,
            &state,
            &settings(),
            "plugin:sakuramedia_actor_metadata",
            Some(&mut NoopReporter),
            1_700_000_000.0,
        )
        .await
        .unwrap();
        // 3 次连续网络异常后停掉本轮。
        assert_eq!(stats.aborted, 1);
        assert_eq!(stats.attempted, 3);
        assert_eq!(stats.errors, 3);
    }

    #[tokio::test]
    async fn subscribed_only_filters_unsubscribed_actors() {
        let (state, _dir) = test_state();
        let mut host = FakeHost {
            actors: HashMap::from([
                (
                    1,
                    actor(
                        1,
                        &[
                            ("name", HostValue::Str("A".to_owned())),
                            ("is_subscribed", HostValue::Bool(false)),
                        ],
                    ),
                ),
                (
                    2,
                    actor(
                        2,
                        &[
                            ("name", HostValue::Str("B".to_owned())),
                            ("is_subscribed", HostValue::Bool(true)),
                        ],
                    ),
                ),
            ]),
            movies: vec![],
            patched: vec![],
        };
        let mut sources = FakeSources {
            javdb: HashMap::new(),
            minnanoav: HashMap::new(),
        };
        let mut settings = settings();
        settings.subscribed_only = true;
        let stats = process(
            &mut host,
            &mut sources,
            &state,
            &settings,
            "plugin:sakuramedia_actor_metadata",
            Some(&mut NoopReporter),
            1_700_000_000.0,
        )
        .await
        .unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.attempted, 1);
    }

    #[test]
    fn field_to_proto_converts_types() {
        use prost_types::value::Kind;
        let v = field_to_proto(&FieldValue::Text("東京".to_owned()));
        assert!(matches!(v.kind, Some(Kind::StringValue(_))));
        let v = field_to_proto(&FieldValue::Int(169));
        assert!(matches!(v.kind, Some(Kind::NumberValue(_))));
    }

    #[test]
    fn missing_fields_detects_what_is_absent() {
        let values: BTreeMap<String, HostValue> = [
            ("birthday", HostValue::Str("1999-04-19".to_owned())),
            ("gender", HostValue::Num(1.0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let missing = missing_fields(&values);
        assert!(!missing.contains(&"birthday".to_owned()));
        assert!(!missing.contains(&"gender".to_owned()));
        assert!(missing.contains(&"height_cm".to_owned()));
        assert_eq!(missing.len(), PROFILE_FIELDS.len() - 2);
    }

    #[test]
    fn a_field_owned_by_someone_else_is_not_writable() {
        let mut snapshot = actor(1, &[]);
        snapshot
            .field_owners
            .insert("birthday".to_owned(), "plugin:other".to_owned());
        let missing = missing_fields(&snapshot.values);
        let writable = writable_fields(&snapshot, &missing, "plugin:me");
        assert!(!writable.contains(&"birthday".to_owned()));
        // 其余字段无主，照旧可写。
        assert!(writable.contains(&"height_cm".to_owned()));
    }

    #[test]
    fn a_field_we_own_or_nobody_owns_is_writable() {
        let mut snapshot = actor(1, &[]);
        snapshot
            .field_owners
            .insert("birthday".to_owned(), "plugin:me".to_owned());
        let missing = missing_fields(&snapshot.values);
        let writable = writable_fields(&snapshot, &missing, "plugin:me");
        assert!(writable.contains(&"birthday".to_owned()));
        assert!(writable.contains(&"height_cm".to_owned()));
        assert_eq!(writable.len(), PROFILE_FIELDS.len());
    }

    #[test]
    fn an_ownership_record_does_not_resurrect_a_filled_field() {
        let mut snapshot = actor(1, &[("birthday", HostValue::Str("1999-04-19".to_owned()))]);
        snapshot
            .field_owners
            .insert("birthday".to_owned(), "plugin:me".to_owned());
        let missing = missing_fields(&snapshot.values);
        let writable = writable_fields(&snapshot, &missing, "plugin:me");
        // 值不空 → 本来就不在 `missing` 里（上游只补空字段）。
        assert!(!writable.contains(&"birthday".to_owned()));
        assert_eq!(writable.len(), PROFILE_FIELDS.len() - 1);
    }
}
