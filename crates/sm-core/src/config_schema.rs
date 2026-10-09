//! 配置模式：把上游 `Settings` 的 10 个子节 / 62 个字段搬成**声明式表**。
//!
//! 对应上游 `src/config/config.py`（10 个 pydantic 子模型 + `Settings`）与
//! `config_service.py` 的 `_reject_unknown_fields` / `_strict_validate_sections`。
//!
//! # 为什么是「表」而不是 Rust 结构体
//!
//! `ConfigService` 只需要三件事：判断某个 key 是不是**已知**、给出它的**默认值**、
//! 校验它的**值**。这三件事都不需要编译期类型 —— 用结构体反而会带来两个问题：
//!
//! 1. **只落地一部分节时，错误码就错了。** 未知字段在上游是
//!    `unknown_config_field`，而「本节还没移植」不是未知字段。如果只声明已
//!    移植的节，一个 PATCH 写 `metadata.gfriends_filetree_url` 会得到
//!    `unknown_config_field` —— 契约与上游不一致，而客户端无从分辨。表覆盖
//!    全部 62 个字段，于是白名单是完整的。
//! 2. **默认值要能变成 JSON。** 上游 `_public_values` 走
//!    `model_dump_json()` 往返（保证 `set→list`、`enum→value`、
//!    `datetime→str` 全部 JSON 安全）。表天然产出 `serde_json::Value`。
//!
//! # 本模块只做纯函数，不碰文件
//!
//! 读盘 / 合并 / 原子写都在 `sm_service::system::config`。这里只有模式与校验，
//! 于是 `sm-server` 也能复用而不产生第二个配置来源。
//!
//! # 键名是 snake_case
//!
//! 上游这些节继承 `BaseModel` / `BaseSettings`（**不是** `SchemaModel`），
//! 没有 alias generator，所以 `model_dump_json()` 出来就是 snake_case，
//! 而 `ConfigResource.values` 是原样透传的 dict。改 camelCase 会与客户端不匹配。

use std::sync::LazyLock;

use serde_json::{json, Map, Value};

/// 字段的值类型。
///
/// 只区分**校验与默认值生成**需要的粒度：`StrList` / `StrMap` / `StrAnyMap`
/// 决定默认值与「集合类校验」的形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    Int,
    Float,
    Str,
    /// `list[str]`，如 `plugins.enabled`。
    StrList,
    /// `dict[str, str]`，如 `plugins.job_crons`。
    StrMap,
    /// `dict[str, dict[str, Any]]`，如 `plugins.settings`。
    StrAnyMap,
}

/// 数值字段的取值范围（对应 pydantic 的 `Field(ge=, le=)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntRange {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

/// 一个字段的声明。
#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub name: &'static str,
    pub kind: FieldKind,
    /// `None` 且 `default_fn` 也是 `None` 是**编码错误**（客户端会少拿到
    /// 一个键），[`default_of`] 与 `every_field_has_a_default` 各守一道。
    pub default: Option<Value>,
    /// 默认值依赖运行环境时用它（如 `max_thumbnail_process_count` 取
    /// `max(1, ceil(cpu_count/2))`）。
    pub default_fn: Option<fn() -> Value>,
    pub range: Option<IntRange>,
    /// 是否允许显式 `null`（对应 `str | None`）。全表只有
    /// `image_search.inference_api_key` 一个。
    pub nullable: bool,
}

const fn field(name: &'static str, kind: FieldKind, default: Value) -> FieldSpec {
    FieldSpec {
        name,
        kind,
        default: Some(default),
        default_fn: None,
        range: None,
        nullable: false,
    }
}

/// 允许显式 `null` 的字段（`str | None`）。
const fn field_nullable(name: &'static str, default: Value) -> FieldSpec {
    FieldSpec {
        name,
        kind: FieldKind::Str,
        default: Some(default),
        default_fn: None,
        range: None,
        nullable: true,
    }
}

/// `int` + 取值范围。
fn ranged(name: &'static str, default: i64, min: i64, max: Option<i64>) -> FieldSpec {
    FieldSpec {
        name,
        kind: FieldKind::Int,
        default: Some(json!(default)),
        default_fn: None,
        range: Some(IntRange {
            min: Some(min),
            max,
        }),
        nullable: false,
    }
}

/// 一个配置节。
#[derive(Debug, Clone)]
pub struct SectionSpec {
    pub name: &'static str,
    /// 用 `Vec` 而非 `&'static [FieldSpec]`：表在 `LazyLock` 的闭包里构建，
    /// 闭包内的数组字面量拿不到 `'static` 生命周期。
    pub fields: Vec<FieldSpec>,
}

/// **不**通过配置 API 暴露的顶层键。
///
/// 上游 `READONLY_KEYS = {"auth", "enable_docs", "plugins"}`。三个键各自的
/// 理由（`config_service.py:19-26`）值得抄下来，因为它们**不是**「暂时没实现」：
///
/// | 键 | 为什么只读 |
/// |---|---|
/// | `auth` | `username`/`password` 由 `/account` 管（运行时账号在 DB，改配置无效）；`secret_key`/`file_signature_secret` 由首启自举，改它会连带作废所有 access token 与已发签名 URL |
/// | `enable_docs` | Swagger/ReDoc 开关，改它要重启，不该经通用接口 |
/// | `plugins` | 可信代码启用清单 + 插件私有配置，import 阶段读取且**可能含凭据**，只允许手工改 toml 后重启 |
pub const READONLY_KEYS: [&str; 3] = ["auth", "enable_docs", "plugins"];

/// 除子节与只读键之外，另外两个可 PATCH 的顶层键。
///
/// # 为什么含 `updates` 与 `existing_config`
///
/// 上游 `Settings` 把这两个也声明成了字段（`updates: dict[str, str]` 与
/// `existing_config: dict[str, Any]`，都是运行期产物），所以它们**在**
/// `Settings.model_fields` 里，而 `_reject_unknown_fields` 就是拿
/// `model_fields` 当白名单 —— 于是这两个键**是**可 PATCH 的。
///
/// 看起来像上游的疏漏，但照抄是对齐：收紧白名单会让原本 200 的请求变 422，
/// 客户端看到的分支就与上游不同了。真正的风险（把运行期状态写进配置）由
/// 「PATCH 只写盘、不改内存快照」挡住 —— 值要重启才生效。
pub const EXTRA_WRITABLE_KEYS: [&str; 2] = ["updates", "existing_config"];

/// 上游 `Scheduler` 里的 16 个 `*_cron` 字段。
///
/// 上游校验器按「名字以 `_cron` 结尾」筛出来逐个解析。把它们列成常量，
/// 于是「哪些字段参与 cron 校验」是一份可读清单而不是一个后缀判断 ——
/// 而 `media_clip_ffmpeg_timeout_seconds` 之类的字段名不会误伤。
pub const CRON_FIELDS: [&str; 16] = [
    "actor_subscription_sync_cron",
    "subscribed_movie_auto_download_cron",
    "download_task_sync_cron",
    "download_task_auto_import_cron",
    "movie_heat_cron",
    "movie_interaction_sync_cron",
    "movie_javdb_backfill_cron",
    "media_file_hash_backfill_cron",
    "media_file_scan_cron",
    "media_thumbnail_cron",
    "image_search_index_cron",
    "movie_similarity_recompute_cron",
    "moment_recommendation_generate_cron",
    "daily_recommendation_generate_cron",
    "activity_cleanup_cron",
    "gfriends_filetree_refresh_cron",
];

/// 全部配置节。顺序按上游 `Settings` 的字段声明顺序，便于逐节对照。
pub static SECTIONS: LazyLock<Vec<SectionSpec>> = LazyLock::new(|| {
    vec![
        SectionSpec {
            name: "database",
            fields: vec![
                field("engine", FieldKind::Str, json!("postgres")),
                field(
                    "url",
                    FieldKind::Str,
                    json!("postgresql://sakuramedia:sakuramedia@postgres:5432/sakuramediabe"),
                ),
            ],
        },
        SectionSpec {
            name: "auth",
            fields: vec![
                field("username", FieldKind::Str, json!("account")),
                field("password", FieldKind::Str, json!("account")),
                // 空串是「未初始化」哨兵：首启自举生成随机值并落盘。
                field("secret_key", FieldKind::Str, json!("")),
                field("algorithm", FieldKind::Str, json!("HS256")),
                field(
                    "access_token_expire_minutes",
                    FieldKind::Int,
                    json!(60 * 24 * 30),
                ),
                field(
                    "refresh_token_expire_minutes",
                    FieldKind::Int,
                    json!(60 * 24 * 7),
                ),
                field("file_signature_secret", FieldKind::Str, json!("")),
            ],
        },
        SectionSpec {
            name: "media",
            fields: vec![
                field(
                    "allowed_min_video_file_size",
                    FieldKind::Int,
                    json!(268_435_456),
                ),
                field(
                    "import_image_root_path",
                    FieldKind::Str,
                    json!("/data/cache/assets"),
                ),
                FieldSpec {
                    name: "max_thumbnail_process_count",
                    kind: FieldKind::Int,
                    // 上游 `max(1, ceil(cpu_count/2))` —— 依赖运行环境，所以是函数。
                    default: None,
                    default_fn: Some(default_thumbnail_process_count),
                    range: None,
                    nullable: false,
                },
                field(
                    "media_clip_root_path",
                    FieldKind::Str,
                    json!("/data/media-clips"),
                ),
                field(
                    "media_clip_max_duration_seconds",
                    FieldKind::Int,
                    json!(900),
                ),
                field(
                    "media_clip_ffmpeg_timeout_seconds",
                    FieldKind::Int,
                    json!(120),
                ),
            ],
        },
        SectionSpec {
            name: "metadata",
            fields: vec![
                field(
                    "gfriends_filetree_url",
                    FieldKind::Str,
                    json!("https://cdn.jsdelivr.net/gh/xinxin8816/gfriends/Filetree.json"),
                ),
                field(
                    "gfriends_cdn_base_url",
                    FieldKind::Str,
                    json!("https://cdn.jsdelivr.net/gh/xinxin8816/gfriends"),
                ),
                field(
                    "gfriends_filetree_cache_path",
                    FieldKind::Str,
                    json!("/data/cache/gfriends/gfriends-filetree.json"),
                ),
                field(
                    "gfriends_filetree_cache_ttl_hours",
                    FieldKind::Int,
                    json!(24 * 7),
                ),
                field("import_metadata_max_workers", FieldKind::Int, json!(3)),
            ],
        },
        SectionSpec {
            name: "plugins",
            fields: vec![
                field("root_dir", FieldKind::Str, json!("/data/plugins")),
                field("enabled", FieldKind::StrList, json!([])),
                field("job_crons", FieldKind::StrMap, json!({})),
                field("settings", FieldKind::StrAnyMap, json!({})),
            ],
        },
        SectionSpec {
            name: "scheduler",
            fields: vec![
                field("enabled", FieldKind::Bool, json!(true)),
                ranged("worker_default_concurrency", 4, 1, Some(32)),
                field("log_dir", FieldKind::Str, json!("/data/logs")),
                field(
                    "actor_subscription_sync_cron",
                    FieldKind::Str,
                    json!("0 2 * * *"),
                ),
                field(
                    "subscribed_movie_auto_download_cron",
                    FieldKind::Str,
                    json!("30 2 * * *"),
                ),
                field(
                    "download_task_sync_cron",
                    FieldKind::Str,
                    json!("* * * * *"),
                ),
                field(
                    "download_task_auto_import_cron",
                    FieldKind::Str,
                    json!("* * * * *"),
                ),
                field("movie_heat_cron", FieldKind::Str, json!("15 0 * * *")),
                field(
                    "movie_interaction_sync_cron",
                    FieldKind::Str,
                    json!("0 5 * * *"),
                ),
                field(
                    "movie_javdb_backfill_cron",
                    FieldKind::Str,
                    json!("30 5 * * *"),
                ),
                field(
                    "media_file_hash_backfill_cron",
                    FieldKind::Str,
                    json!("0 3 * * *"),
                ),
                field("media_file_scan_cron", FieldKind::Str, json!("0 4 * * *")),
                field(
                    "media_thumbnail_cron",
                    FieldKind::Str,
                    json!("*/30 * * * *"),
                ),
                field(
                    "image_search_index_cron",
                    FieldKind::Str,
                    json!("*/5 * * * *"),
                ),
                field(
                    "movie_similarity_recompute_cron",
                    FieldKind::Str,
                    json!("30 3 * * *"),
                ),
                field(
                    "moment_recommendation_generate_cron",
                    FieldKind::Str,
                    json!("0 4 * * *"),
                ),
                field(
                    "daily_recommendation_generate_cron",
                    FieldKind::Str,
                    json!("0 5 * * *"),
                ),
                field("activity_cleanup_cron", FieldKind::Str, json!("30 5 * * *")),
                field(
                    "gfriends_filetree_refresh_cron",
                    FieldKind::Str,
                    json!("0 4 * * 1"),
                ),
                field(
                    "activity_task_run_retention_per_key",
                    FieldKind::Int,
                    json!(200),
                ),
                field(
                    "activity_notification_read_retention_days",
                    FieldKind::Int,
                    json!(3),
                ),
            ],
        },
        SectionSpec {
            name: "downloads",
            fields: vec![
                ranged("subscription_search_fresh_days", 90, 1, None),
                ranged("subscription_search_stale_attempt_limit", 3, 1, None),
            ],
        },
        SectionSpec {
            name: "logging",
            fields: vec![field("level", FieldKind::Str, json!("INFO"))],
        },
        SectionSpec {
            name: "image_search",
            fields: vec![
                field("enabled", FieldKind::Bool, json!(false)),
                field(
                    "inference_base_url",
                    FieldKind::Str,
                    json!("http://siglip2-embed:8080"),
                ),
                field("inference_timeout_seconds", FieldKind::Float, json!(120.0)),
                field(
                    "inference_connect_timeout_seconds",
                    FieldKind::Float,
                    json!(3.0),
                ),
                field_nullable("inference_api_key", Value::Null),
                field("inference_batch_size", FieldKind::Int, json!(16)),
                field("session_ttl_seconds", FieldKind::Int, json!(600)),
                field("default_page_size", FieldKind::Int, json!(20)),
                field("max_page_size", FieldKind::Int, json!(100)),
                field("search_scan_batch_size", FieldKind::Int, json!(100)),
                ranged("index_upsert_batch_size", 100, 1, None),
            ],
        },
        SectionSpec {
            name: "qdrant",
            fields: vec![
                field("enabled", FieldKind::Bool, json!(false)),
                field("url", FieldKind::Str, json!("http://qdrant:6333")),
                field("api_key", FieldKind::Str, json!("")),
            ],
        },
    ]
});

/// 该字段是否要过 cron 校验。
fn is_cron_field(field_name: &str) -> bool {
    CRON_FIELDS.contains(&field_name)
}

/// 插件 ID 正则，等价于上游 `PLUGIN_ID_PATTERN = ^[a-z][a-z0-9_]*$`
/// （`src/plugins/manifest.py:13`）。
///
/// 不用 `regex` crate：这个模式用字符类就能逐字符判，避免为一个正则引依赖。
pub fn is_valid_plugin_id(value: &str) -> bool {
    let mut chars = value.chars();
    // `^[a-z]` 之后必须还有字符，所以空串直接不合格。
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// 必须是带 netloc 的 http/https URL。
///
/// 上游 `_check_http_url` 用 `urlparse` 后判 `scheme in {http, https}`
/// 且 `netloc` 非空。这里手写而不引 `url` crate：这个判据只用到 scheme 与
/// `//` 之后那一段，而一个错误的 URL 恰恰是这里要拦住的东西 —— 引入完整
/// 解析器反而要处理它自己的容错。
pub fn is_http_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !netloc.is_empty()
}

/// `image_search.enabled` 依赖 `qdrant.enabled`。
///
/// 上游 `config_service.update_config` 里是一条显式的跨节断言：「启用图片与
/// 文字搜图需要先启用 Qdrant」。放在模式层是因为单节内部看不到 `qdrant`，
/// 而调用点只负责把错误翻译成 422。
pub fn image_search_requires_qdrant(values: &Map<String, Value>) -> bool {
    let enabled = |section: &str| -> bool {
        values
            .get(section)
            .and_then(Value::as_object)
            .and_then(|s| s.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    enabled("image_search") && !enabled("qdrant")
}

fn default_thumbnail_process_count() -> Value {
    // 上游 `max(1, math.ceil(os.cpu_count() or 1) / 2)`。
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    json!((cpus.div_ceil(2)).max(1))
}

/// 按名字取节。
pub fn section(name: &str) -> Option<&'static SectionSpec> {
    let sections: &'static Vec<SectionSpec> = &SECTIONS;
    sections.iter().find(|s| s.name == name)
}

/// 节内按名字取字段。
pub fn field_of(section_name: &str, field_name: &str) -> Option<&'static FieldSpec> {
    section(section_name)?
        .fields
        .iter()
        .find(|f| f.name == field_name)
}

/// 顶层键是否**已知**（含只读键）。
pub fn is_known_top_level_key(key: &str) -> bool {
    key == "enable_docs" || EXTRA_WRITABLE_KEYS.contains(&key) || section(key).is_some()
}

/// 顶层键是否**是子节**。
pub fn is_section_key(key: &str) -> bool {
    section(key).is_some()
}

/// 顶层键是否**只读**。
pub fn is_readonly_key(key: &str) -> bool {
    READONLY_KEYS.contains(&key)
}

/// 字段的默认值。
pub fn default_of(spec: &FieldSpec) -> Value {
    match (spec.default.clone(), spec.default_fn) {
        (Some(value), _) => value,
        (None, Some(f)) => f(),
        (None, None) => Value::Null,
    }
}

/// 一条字段级校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// `section.field`，与上游 pydantic `loc` 的形状一致。
    pub loc: String,
    pub reason: String,
}

impl FieldError {
    fn new(section: &str, field: &str, reason: impl Into<String>) -> Self {
        Self {
            loc: format!("{section}.{field}"),
            reason: reason.into(),
        }
    }
}

/// 校验整个 values 快照（**严格档**）。
///
/// 上游有严格 / 宽松两档：严格档（配置 API 写入）遇非法值直接拒；宽松档
/// （启动加载）只 warn 并原样保留，因为存量非法配置不该让进程起不来。本函数
/// 是**严格档** —— 它服务于「写盘之前拦住非法值」。
///
/// 覆盖上游 5 个校验器 + 2 处 `Field(ge/le)`：
///
/// | 来源 | 规则 |
/// |---|---|
/// | `Metadata._check_gfriends_urls` | 两个 gfriends URL 必须是 http/https |
/// | `Plugins._validate_enabled_plugin_ids` | `enabled` 无重复、每项是合法插件 ID |
/// | `Plugins._validate_plugin_config_namespaces` | `job_crons`/`settings` 的键是合法插件 ID |
/// | `ImageSearch._check_inference_base_url` | 推理服务 URL 必须是 http/https |
/// | `Qdrant._check_url` | Qdrant URL 必须是 http/https |
/// | `Scheduler._validate_cron_expressions` | 16 个 `*_cron` 是合法 5 段 crontab |
/// | `Field(ge/le)` | 4 个数值字段的区间 |
pub fn validate_strict(values: &Map<String, Value>) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let sections: &'static Vec<SectionSpec> = &SECTIONS;
    for spec in sections {
        let Some(section_value) = values.get(spec.name) else {
            continue;
        };
        let Some(object) = section_value.as_object() else {
            // 类型不匹配由调用方在更早一层拦成 `invalid_config_value`。
            continue;
        };
        for field_spec in &spec.fields {
            let Some(value) = object.get(field_spec.name) else {
                continue;
            };
            if let Some(err) = validate_field(spec.name, field_spec, value) {
                errors.push(err);
            }
        }
    }
    errors
}

fn validate_field(section: &str, spec: &FieldSpec, value: &Value) -> Option<FieldError> {
    // `inference_api_key: str | None` 是全表**唯一**允许显式 null 的字段。
    if value.is_null() {
        return if spec.nullable {
            None
        } else {
            Some(FieldError::new(section, spec.name, "不能为 null"))
        };
    }
    // 类型不符统一记在这里。上游由 pydantic 的类型校验报同一个错误码，消息
    // 文本不同 —— 错误码与 details.loc 才是客户端真正读的。
    match spec.kind {
        FieldKind::Bool if !value.is_boolean() => {
            return Some(FieldError::new(section, spec.name, "必须是布尔值"));
        }
        FieldKind::Int if !value.is_i64() && !value.is_u64() => {
            return Some(FieldError::new(section, spec.name, "必须是整数"));
        }
        FieldKind::Float if !value.is_number() => {
            return Some(FieldError::new(section, spec.name, "必须是数字"));
        }
        FieldKind::Str if !value.is_string() => {
            return Some(FieldError::new(section, spec.name, "必须是字符串"));
        }
        _ => {}
    }
    if let Some(range) = spec.range {
        if let Some(number) = value.as_i64() {
            let below = range.min.is_some_and(|min| number < min);
            let above = range.max.is_some_and(|max| number > max);
            if below || above {
                let bound = match (range.min, range.max) {
                    (Some(min), Some(max)) => format!("必须在 {min}..={max} 之间"),
                    (Some(min), None) => format!("必须 ≥ {min}"),
                    (None, Some(max)) => format!("必须 ≤ {max}"),
                    (None, None) => String::new(),
                };
                return Some(FieldError::new(section, spec.name, bound));
            }
        }
    }
    if is_cron_field(spec.name) {
        if let Some(text) = value.as_str() {
            if !crate::crontab::is_valid_crontab(text) {
                return Some(FieldError::new(
                    section,
                    spec.name,
                    format!("不是合法的 cron 表达式: {text}"),
                ));
            }
        }
    }
    match spec.kind {
        FieldKind::Str if is_http_url_field(section, spec.name) => {
            let text = value.as_str().unwrap_or_default();
            if !is_http_url(text) {
                return Some(FieldError::new(
                    section,
                    spec.name,
                    "必须是 http 或 https URL",
                ));
            }
            None
        }
        FieldKind::StrList => {
            let Some(items) = value.as_array() else {
                return Some(FieldError::new(section, spec.name, "必须是数组"));
            };
            validate_plugin_list(section, spec.name, items)
        }
        FieldKind::StrMap | FieldKind::StrAnyMap => {
            let Some(object) = value.as_object() else {
                return Some(FieldError::new(section, spec.name, "必须是对象"));
            };
            for key in object.keys() {
                if !is_valid_plugin_id(key) {
                    return Some(FieldError::new(
                        section,
                        spec.name,
                        format!("包含非法插件 ID: {key}"),
                    ));
                }
            }
            None
        }
        // Bool / Int / Float 的专属规则已在上面查过（类型、范围）。
        _ => None,
    }
}

fn validate_plugin_list(section: &str, field: &str, items: &[Value]) -> Option<FieldError> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let Some(id) = item.as_str() else {
            return Some(FieldError::new(section, field, "只能包含字符串"));
        };
        if !seen.insert(id) {
            return Some(FieldError::new(section, field, "不允许包含重复插件 ID"));
        }
        if !is_valid_plugin_id(id) {
            return Some(FieldError::new(
                section,
                field,
                format!("插件 ID 只能包含小写字母、数字、下划线且必须以字母开头: {id}"),
            ));
        }
    }
    None
}

/// 这四个字段要过 http/https 校验。
fn is_http_url_field(section: &str, field: &str) -> bool {
    matches!(
        (section, field),
        ("metadata", "gfriends_filetree_url")
            | ("metadata", "gfriends_cdn_base_url")
            | ("image_search", "inference_base_url")
            | ("qdrant", "url")
    )
}

/// 全部默认值构成的快照（等价上游 `_json_safe_values(Settings())`）。
///
/// 含只读键 —— 调用方要公开快照时用 [`public_json`] 剔除。
pub fn defaults_json() -> Value {
    let sections: &'static Vec<SectionSpec> = &SECTIONS;
    let mut root = Map::new();
    root.insert("enable_docs".into(), json!(false));
    root.insert("updates".into(), json!({}));
    root.insert("existing_config".into(), json!({}));
    for spec in sections {
        let mut object = Map::new();
        for field in &spec.fields {
            object.insert(field.name.to_owned(), default_of(field));
        }
        root.insert(spec.name.to_owned(), Value::Object(object));
    }
    Value::Object(root)
}

/// 剔除只读键后的公开快照（等价上游 `_public_values`）。
pub fn public_json(values: &Value) -> Value {
    let mut values = values.clone();
    if let Some(object) = values.as_object_mut() {
        for key in READONLY_KEYS {
            object.remove(key);
        }
    }
    values
}

/// 以默认值为底，用磁盘上的值覆盖。
///
/// 上游 `Settings` 的每个字段都有默认值，配置文件只需写差异项。所以读到的
/// 快照总是**完整**的 —— 缺项由默认值补齐，而不是缺项就报错。
///
/// 磁盘上出现表里没有的键时**保留**（不清掉）：那可能是由更新版本写入的，
/// 本进程不认识但不该删。严格校验只查表内的字段。
///
/// 节内也是逐字段覆盖而不是整节替换，所以一个只写
/// `[scheduler] log_dir = "..."` 的配置文件不会把 `scheduler` 的其余 20 个
/// 字段清成 null。
pub fn overlay_defaults(from_disk: Value) -> Value {
    let mut base = defaults_json();
    let (Some(base_map), Some(disk_map)) = (base.as_object_mut(), from_disk.as_object()) else {
        return from_disk;
    };
    for (key, value) in disk_map {
        // 磁盘上一节是对象、默认值那节也是对象 → 逐字段覆盖，缺的字段留默认值。
        let merged = match (value.as_object(), base_map.get_mut(key)) {
            (Some(incoming), Some(existing)) if existing.is_object() => {
                let mut section = existing.as_object().cloned().unwrap_or_default();
                for (sub_key, sub_value) in incoming {
                    section.insert(sub_key.clone(), sub_value.clone());
                }
                Value::Object(section)
            }
            _ => value.clone(),
        };
        base_map.insert(key.clone(), merged);
    }
    base
}

/// 只读的类型化视图：按 `节.字段` 取值，取不到就回默认值。
///
/// # 为什么不是 `ServerConfig` 那样的结构体
///
/// 因为**默认值只应该有一份**，而在 [`SECTIONS`] 里。调用方写
/// `view.str("database", "url")` 时不需要重复「默认连接串是什么」这个字面量 ——
/// 那串东西在表里，出现第二次就意味着它会漂移。
///
/// 取不到的键返回 `None` 而不是编一个值：键名写错会得到 `None`，而写一个
/// 假的默认值会让「配置没生效」和「配置写了但没读到」长得一模一样。
pub struct View<'a> {
    values: &'a Map<String, Value>,
}

impl<'a> View<'a> {
    /// 建视图。传入的快照应当已经过 [`overlay_defaults`]。
    pub fn new(values: &'a Map<String, Value>) -> Self {
        Self { values }
    }

    /// 取字符串字段。`None` = 键不存在或不是字符串。
    pub fn str(&self, section: &str, field: &str) -> Option<&'a str> {
        self.raw(section, field)?.as_str()
    }

    /// 取字符串字段，缺失或类型不对时回落到模式里的默认值。
    pub fn str_or_default(&self, section: &str, field: &str) -> Option<String> {
        self.str(section, field).map(str::to_owned).or_else(|| {
            field_of(section, field).map(|f| default_of(f).as_str().map(str::to_owned))?
        })
    }

    /// 取布尔字段。
    pub fn bool(&self, section: &str, field: &str) -> Option<bool> {
        self.raw(section, field)?.as_bool()
    }

    /// 取整数字段。
    pub fn int(&self, section: &str, field: &str) -> Option<i64> {
        self.raw(section, field)?.as_i64()
    }

    fn raw(&self, section: &str, field: &str) -> Option<&'a Value> {
        self.values.get(section)?.as_object()?.get(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 节内字段的总数，用来让「改了表却忘了同步白名单」这类改动暴露出来。
    fn section_field_count() -> usize {
        SECTIONS.iter().map(|s| s.fields.len()).sum()
    }

    #[test]
    fn the_schema_covers_every_upstream_field() {
        // 上游 10 个子节共 62 个字段，另加 Settings 自身的 13 个键
        // （10 个节引用 + enable_docs + updates + existing_config）。
        assert_eq!(SECTIONS.len(), 10, "10 个子节（auth / plugins 也是节）");
        assert_eq!(section_field_count(), 62, "节内字段总数");
        assert_eq!(
            section_field_count() + SECTIONS.len() + 3,
            75,
            "节内字段 + Settings 自身的 13 个键 = 75"
        );
    }

    #[test]
    fn every_field_has_a_default() {
        for spec in SECTIONS.iter() {
            for field in &spec.fields {
                assert!(
                    field.default.is_some() || field.default_fn.is_some(),
                    "{}.{} 缺少默认值 —— 客户端会少拿到一个键",
                    spec.name,
                    field.name
                );
            }
        }
    }

    #[test]
    fn every_cron_default_actually_parses() {
        // 表里写错的 cron 在**读表**时就被发现，而不是等运维 PATCH。
        for spec in SECTIONS.iter() {
            for field in &spec.fields {
                if is_cron_field(field.name) {
                    let text = default_of(field);
                    let text = text.as_str().unwrap_or_default();
                    assert!(
                        crate::crontab::is_valid_crontab(text),
                        "{}.{} 的默认 cron {text:?} 无法解析",
                        spec.name,
                        field.name
                    );
                }
            }
        }
        assert_eq!(CRON_FIELDS.len(), 16, "16 个 cron 字段");
        // 这两个字段名字里带 `_cron` 之外的东西，不该被 cron 校验抓。
        assert!(!is_cron_field("activity_task_run_retention_per_key"));
        assert!(!is_cron_field("media_clip_ffmpeg_timeout_seconds"));
    }

    #[test]
    fn readonly_keys_are_known_but_not_sections() {
        assert!(section("auth").is_some(), "auth 是子节");
        assert!(section("plugins").is_some(), "plugins 是子节");
        assert!(!is_section_key("enable_docs"), "enable_docs 是顶层标量");
        for key in READONLY_KEYS {
            assert!(is_known_top_level_key(key), "{key} 必须是已知键");
            assert!(is_readonly_key(key));
        }
        assert!(!is_known_top_level_key("scheduler_typo"));
        assert!(is_known_top_level_key("updates"));
        assert!(is_known_top_level_key("existing_config"));
    }

    #[test]
    fn http_urls_are_checked_by_scheme_and_netloc() {
        for ok in [
            "http://qdrant:6333",
            "https://cdn.jsdelivr.net/gh/x",
            "http://siglip2-embed:8080",
        ] {
            assert!(is_http_url(ok), "{ok} 应当合法");
        }
        for bad in [
            "",
            "qdrant:6333",  // 无 scheme
            "ftp://host",   // scheme 不对
            "http://",      // 无 netloc
            "http:///path", // netloc 为空
        ] {
            assert!(!is_http_url(bad), "{bad:?} 应当不合法");
        }
    }

    #[test]
    fn plugin_ids_follow_the_upstream_pattern() {
        for ok in ["a", "ab", "a1", "a_b", "local_ref", "abc_123_x"] {
            assert!(is_valid_plugin_id(ok), "{ok:?} 应当合法");
        }
        for bad in ["", "1a", "_a", "A", "a-b", "a.b", "插件", "a b", "aB"] {
            assert!(!is_valid_plugin_id(bad), "{bad:?} 应当不合法");
        }
    }

    #[test]
    fn plugin_lists_reject_duplicates_and_bad_ids() {
        let list = FieldSpec {
            name: "enabled",
            kind: FieldKind::StrList,
            default: Some(json!([])),
            default_fn: None,
            range: None,
            nullable: false,
        };
        assert!(validate_field("plugins", &list, &json!(["a", "b_1"])).is_none());
        assert!(
            validate_field("plugins", &list, &json!(["a", "a"])).is_some(),
            "重复插件 ID 必须被拒"
        );
        assert!(
            validate_field("plugins", &list, &json!(["A"])).is_some(),
            "大写 ID 必须被拒"
        );
    }

    #[test]
    fn image_search_needs_qdrant() {
        let mut values = Map::new();
        values.insert("image_search".into(), json!({"enabled": true}));
        values.insert("qdrant".into(), json!({"enabled": false}));
        assert!(image_search_requires_qdrant(&values));

        values.insert("qdrant".into(), json!({"enabled": true}));
        assert!(!image_search_requires_qdrant(&values));

        values.insert("image_search".into(), json!({"enabled": false}));
        values.insert("qdrant".into(), json!({"enabled": false}));
        assert!(!image_search_requires_qdrant(&values));
    }

    #[test]
    fn defaults_snapshot_has_every_section() {
        let values = defaults_json();
        let object = values.as_object().expect("对象");
        for spec in SECTIONS.iter() {
            assert!(object.contains_key(spec.name), "缺少节 {}", spec.name);
        }
        assert_eq!(
            object["scheduler"]["media_thumbnail_cron"],
            json!("*/30 * * * *")
        );
        assert_eq!(object["qdrant"]["url"], json!("http://qdrant:6333"));
        assert_eq!(object["scheduler"]["worker_default_concurrency"], json!(4));
    }

    #[test]
    fn public_json_drops_exactly_the_readonly_keys() {
        let public = public_json(&defaults_json());
        let object = public.as_object().expect("对象");
        for key in READONLY_KEYS {
            assert!(!object.contains_key(key), "{key} 不该出现在公开快照里");
        }
        assert!(object.contains_key("scheduler"));
        assert!(object.contains_key("database"));
    }

    #[test]
    fn strict_validation_accepts_the_defaults() {
        // 全部默认值必须自洽 —— 否则就成了「启动即非法配置」。
        let values = defaults_json();
        let errors = validate_strict(values.as_object().expect("对象"));
        assert!(errors.is_empty(), "默认值应当通过严格校验：{errors:?}");
    }

    #[test]
    fn strict_validation_catches_each_upstream_validator() {
        let mut values = defaults_json();
        let object = values.as_object_mut().expect("对象");
        object["scheduler"]["movie_heat_cron"] = json!("不是 cron");
        object["scheduler"]["worker_default_concurrency"] = json!(99);
        object["qdrant"]["url"] = json!("not-a-url");
        object["plugins"]["enabled"] = json!(["a", "a"]);
        object["plugins"]["job_crons"] = json!({"Bad-ID": {}});
        object["logging"]["level"] = json!(123);
        object["scheduler"]["enabled"] = json!(null);
        object["image_search"]["inference_api_key"] = json!(null);

        let locs: Vec<String> = validate_strict(object).into_iter().map(|e| e.loc).collect();
        for expected in [
            "scheduler.movie_heat_cron",            // cron 校验器
            "scheduler.worker_default_concurrency", // Field(le=32)
            "qdrant.url",                           // _check_url
            "plugins.enabled",                      // 重复插件 ID
            "plugins.job_crons",                    // 非法插件命名空间
            "logging.level",                        // 类型
            "scheduler.enabled",                    // 非 nullable 字段给了 null
        ] {
            assert!(
                locs.contains(&expected.to_owned()),
                "缺少 {expected}，实际：{locs:?}"
            );
        }
        assert!(
            !locs.contains(&"image_search.inference_api_key".to_owned()),
            "`str | None` 字段允许显式 null"
        );
    }

    #[test]
    fn metadata_urls_go_through_the_same_http_check() {
        let mut values = defaults_json();
        let object = values.as_object_mut().expect("对象");
        object["metadata"]["gfriends_cdn_base_url"] = json!("ftp://x");
        let locs: Vec<String> = validate_strict(object).into_iter().map(|e| e.loc).collect();
        assert_eq!(locs, vec!["metadata.gfriends_cdn_base_url".to_owned()]);
    }
}
