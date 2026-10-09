//! JavDB 元数据 provider —— 上游 `src/metadata/_providers/javdb.py`（975 行）
//! 里**本批需要的那三个方法**。
//!
//! # 为什么只做三个
//!
//! 上游那个类还带榜单（`get_rank_numbers` / `get_playback_rank_numbers`）、
//! 演员（`search_actors`）、系列（`search_series`）、 reviews、以及**登录态**
//! （`_ensure_logged_in` + 设备指纹 `_device_payload`）。本批只做：
//!
//! | 上游方法 | 行号 | 用途 |
//! |---|---|---|
//! | `_search_movie` | `:386` | 按番号搜到候选（`GET /api/v2/search`）|
//! | `get_movie_by_javdb_id` | `:416` | 取详情（`GET /api/v4/movies/{id}`）|
//! | `_normalize_image_url` | `:125` | 图片 URL 归一（纯函数）|
//!
//! 其余的**不是「漏了」，是还没接线**：见
//! `docs/handoff.md` §7.2f 的被阻塞端点清单。
//!
//! # ★ 返回**原始 JSON**，不做字段映射
//!
//! 上游的 `get_movie_by_javdb_id` 返回 `JavdbMovieDetailResource`（`_build_movie_detail`
//! 逐字段映射）。而本仓的 [`MetadataProvider`] 返回 `serde_json::Value` ——
//! `PluginDelivery.javdb_detail` 的文档写的是「provider 原文，宿主只搬运」。
//! 映射在 `catalog_import` 那一侧做，所以这里**不重复一遍**。
//!
//! # 不做登录态
//!
//! `_ensure_logged_in` 只在「需要登录的接口」上用（榜单的某些分区）。搜索与
//! 详情在未登录下可取，所以本批**不带 Cookie**。带上的话，「未登录也能用」
//! 这个性质会悄悄变成「登录态过期就开始报错」。
//!
//! # host 不带 scheme
//!
//! 上游 `_build_api_url` 拼的是 `https://{host}{path}`，而 `host` 来自配置
//! （`javdb.host`）。本仓照抄那个形状 —— 于是配置里写 `javdb.com` 而不是
//! `https://javdb.com`。**写错 scheme 的后果**：拼出 `https://https://…` 而
//! reqwest 报「无效 URL」，那是 502 而不是 404。

use serde_json::Value;

use crate::catalog::metadata_source::{
    JavdbMovieListItem, JavdbSeries, MetadataProvider, MetadataSourceError,
};

/// 搜索接口（上游 `API_PATH_SEARCH`）。
pub const API_PATH_SEARCH: &str = "/api/v2/search";
/// 影片详情（上游 `API_PATH_MOVIE_DETAIL`）。
pub const API_PATH_MOVIE_DETAIL: &str = "/api/v4/movies/{javdb_id}";
/// 影片评论（上游 `API_PATH_MOVIE_REVIEWS`）。**不需要登录** —— 上游
/// `get_movie_reviews_by_javdb_id` 直接 `request_json`，不碰 `_ensure_logged_in`。
pub const API_PATH_MOVIE_REVIEWS: &str = "/api/v1/movies/{javdb_id}/reviews";
/// 有码 / 无码 / FC2 榜单（上游 `API_PATH_RANKINGS`）。
pub const API_PATH_RANKINGS: &str = "/api/v1/rankings";
/// 播放榜（上游 `API_PATH_RANKINGS_PLAYBACK`）。
pub const API_PATH_RANKINGS_PLAYBACK: &str = "/api/v1/rankings/playback";
/// TOP250（上游 `API_PATH_MOVIES_TOP`）。**需登录**。
pub const API_PATH_MOVIES_TOP: &str = "/api/v1/movies/top";
/// 登录换 token（上游 `API_PATH_SESSIONS`）。
pub const API_PATH_SESSIONS: &str = "/api/v1/sessions";
/// 系列影片列表（上游 `API_PATH_MOVIES_TAGS`）。
pub const API_PATH_MOVIES_TAGS: &str = "/api/v1/movies/tags";

/// 系列搜索的固定参数（上游 `API_PARAMS_SERIES_SEARCH`，**不含 page** ——
/// 上游常量里的 `page: 1` 会被调用方的显式 page 覆盖，这里不产生重复键）。
const SERIES_SEARCH_PARAMS: [(&str, &str); 2] = [("from_recent", "false"), ("type", "series")];
/// 系列影片列表的固定参数（上游 `API_PARAMS_SERIES_MOVIES`）。
const SERIES_MOVIES_PARAMS: [(&str, &str); 2] = [("sort_by", "release"), ("order_by", "desc")];

/// 演员搜索的固定参数（上游 `API_PARAMS_ACTOR_SEARCH`，**不含 page/limit** ——
/// 与系列搜索同一处理：上游常量里的 `page: 1` 会被显式值覆盖，不产生重复键）。
///
/// ⚠️ `limit: 24` **不是**「随便取 24 个」：它决定「同名演员里能不能一次看全」。
/// JavDB 侧同名卡片（不同 id）很常见，调小它会让用户少看到几条候选。
const ACTOR_SEARCH_PARAMS: [(&str, &str); 2] = [("from_recent", "false"), ("type", "actor")];
/// 演员搜索每页条数（上游 `API_PARAMS_ACTOR_SEARCH["limit"]`）。
const ACTOR_SEARCH_LIMIT: i64 = 24;

/// 搜索的固定查询参数（上游 `API_PARAMS_MOVIE_SEARCH`）。
///
/// `limit=24` 是上游的值：**搜索结果按发行日期倒序后取第一个番号精确匹配**，
/// 而精确匹配通常落在前几条里。调大它只会让无关候选变多（而匹配逻辑是
/// 「番号归一后**完全相等**」，不是模糊）。
const MOVIE_SEARCH_PARAMS: [(&str, &str); 6] = [
    ("from_recent", "false"),
    ("type", "movie"),
    ("movie_type", "all"),
    ("movie_sort_by", "relevance"),
    ("movie_filter_by", "all"),
    ("page", "1"),
];

/// 图片 URL 归一（上游 `_normalize_image_url`，`:125-134`）。
///
/// JavDB 的 API 返回的图片路径是**相对片段**（`covers/abc.jpg`），要拼成
/// CDN 的完整 URL 才能下载。上游按 `covers` / `samples` / `avatars` 三个
/// 关键词分别拼，**都不含**就原样返回。
///
/// # 为什么不能用 `contains` 之外的判断
///
/// 上游是 `if "covers" in url`（**子串 anywhere**），不是 `startswith`。
/// 照抄子串判定：URL 里任意位置出现 `covers` 都会被重写 —— 那看起来像 bug，
/// 但改掉它就与上游对同一 URL 的处理不一致，而这种不一致会以「封面 404」
/// 的形式出现（某一类 URL 恰好只在一处不同）。
pub fn normalize_image_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    if url.is_empty() {
        return None;
    }
    for kind in ["covers", "samples", "avatars"] {
        if url.contains(kind) {
            let tail = url.rsplit(&format!("{kind}/")).next().unwrap_or(url);
            return Some(format!("https://c0.jdbstatic.com/{kind}/{tail}"));
        }
    }
    Some(url.to_owned())
}

/// `jdsignature` 里那段固定后缀（上游 `_get_sign`，`javdb.py:676`）。
const SIGN_SUFFIX: &str = "lpw6vgqzsp";

/// `_get_sign` 里与时间戳拼接的固定 secret（上游 `javdb.py:671-674`）。
///
/// ⚠️ 这**不是本仓的配置项**，而是上游**硬编码在客户端里的共享密钥**（官方
/// App 抓包所得）。它随上游发版而变；变了而这里没跟，JavDB 会对**所有**请求回
/// `{"success":0,"action":"ParameterInvalid"}`，而搜索路径不看 `success`
/// （上游亦然）—— 于是表现为「所有番号都查不到」，极易被误判成网络或番号问题。
const SIGN_SECRET: &str = "71cf27bb3c0bcdf207b64abecddc970098c7421ee7203b9cdae54478478a199e7d5a6e1a57691123c1a931c057842fb73ba3b3c83bcd69c17ccf174081e3d8aa";

/// 计算 `jdsignature` 头的值（上游 `JavdbProvider._get_sign`，`javdb.py:669-676`）。
///
/// 形状：`{timestamp}.lpw6vgqzsp.{md5(timestamp + SIGN_SECRET)}`。
/// `timestamp` 是 **Unix 秒**（上游 `int(time.time())`），服务端据此判新鲜度
/// —— 所以必须**每次请求重新计算**，不能缓存、不能跨请求复用。
///
/// 时间戳作为参数而不是内部取当前时间，是为了让测试能对固定时刻断言固定值。
pub fn signature_at(timestamp: i64) -> String {
    let sign = hashing::md5_hex(format!("{timestamp}{SIGN_SECRET}").as_bytes());
    format!("{timestamp}.{SIGN_SUFFIX}.{sign}")
}

/// 有码 / 无码 / FC2 的 `video_type`（上游 `SUPPORTED_RANK_VIDEO_TYPES`）。
///
/// 上游是 `set`，这里用数组 —— 只有三个成员，线性查找比建集合更便宜，且**顺序
/// 无关**（判的是成员资格，不依赖迭代序）。
pub const SUPPORTED_RANK_VIDEO_TYPES: [&str; 3] = ["0", "1", "3"];
/// 榜单周期（上游 `SUPPORTED_RANK_PERIODS`）。
pub const SUPPORTED_RANK_PERIODS: [&str; 3] = ["daily", "weekly", "monthly"];
/// 播放榜筛选（上游 `SUPPORTED_PLAYBACK_FILTERS`）：`all`=热播，`high_score`=高评分。
pub const SUPPORTED_PLAYBACK_FILTERS: [&str; 2] = ["all", "high_score"];
/// TOP250 的 `top_type`（上游 `SUPPORTED_TOP_TYPES`）。
pub const SUPPORTED_TOP_TYPES: [&str; 3] = ["all", "year", "video_type"];
/// TOP250 每页条数（上游 `TOP250_PAGE_LIMIT`）。
pub const TOP250_PAGE_LIMIT: i64 = 50;
/// TOP250 默认抓几页（上游 `TOP250_MAX_PAGES`）：5 页 × 50 条 = 满 250。
pub const TOP250_MAX_PAGES: i64 = 5;

/// 登录用的固定设备指纹（上游 `DEVICE`，`javdb.py:55-63`）。
///
/// ⚠️ **与官方 App 抓包一致，不要改动**。它不进任何配置项 —— 改一个字符就换了
/// 一个「设备」，而 JavDB 的风控是按设备认人的。
const DEVICE: [(&str, &str); 7] = [
    ("device_name", "meizu16sPro"),
    ("device_model", "meizu/16s Pro"),
    ("platform", "android"),
    ("system_version", "9"),
    ("app_channel", "official"),
    ("app_version", "official"),
    ("app_version_number", "1.9.29"),
];

/// 派生 `device_uuid` 的固定命名空间（上游 `DEVICE_UUID_NAMESPACE`，
/// `javdb.py:66` 的 `5374dc5e-0f98-5235-9845-76cbc0ced71f`）。
///
/// 上游注释写明它「沿用原硬编码值作为种子」—— 也就是说这个 UUID 本身没有语义，
/// 唯一的要求是**永远不变**：变了就等于所有账号换了一台设备。
const DEVICE_UUID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x53, 0x74, 0xdc, 0x5e, 0x0f, 0x98, 0x52, 0x35, 0x98, 0x45, 0x76, 0xcb, 0xc0, 0xce, 0xd7, 0x1f,
]);

/// JavDB 账号（上游 `JavdbProvider.__init__` 的 `username` / `password`，
/// `javdb.py:105-118`）。
///
/// 宿主**不保管**账号：它是插件配置里的东西，每次 rpc 由插件透传进来 —— 所以
/// 这个类型只在一次调用里活着，没有「记住 token」的位置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavdbAccount {
    pub username: String,
    pub password: String,
}

impl JavdbAccount {
    /// 「配了账号」的判据（上游 `_ensure_logged_in` 的
    /// `if not (self.username and self.password)`，`:514`）：**两个都非空**。
    ///
    /// 空串与缺省在这个判据下等价 —— 上游此刻是 `None` 或 `""` 都一样，
    /// 所以这里把空缺统一成空串，不再带一层 `Option`。
    pub fn is_configured(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }

    /// 登录载荷里的 `device_uuid`（上游 `_device_payload`，`:503-506`）：
    /// 按账号做 uuid5 派生，于是**同账号每次登录都是同一个设备**、不同账号互不
    /// 相同。全网共用一个设备标识是上游明确要避免的事。
    ///
    /// 未配账号时用**空串**派生 —— 上游 `self.username or ""` 同样如此。不过那
    /// 条路径到不了这里：`is_configured()` 会先拦下来。
    fn device_uuid(&self) -> String {
        uuid::Uuid::new_v5(&DEVICE_UUID_NAMESPACE, self.username.as_bytes()).to_string()
    }
}

/// 评论里嵌的影片摘要（上游 `JavdbReviewMovie`，字段逐个对齐）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JavdbReviewMovie {
    /// 上游 `str(movie.get("id") or "")` —— JavDB 的影片 id 在这里当**字符串**用。
    pub id: String,
    pub number: String,
    pub title: String,
    pub origin_title: Option<String>,
    pub score: Option<f64>,
    pub thumb_url: Option<String>,
    pub release_date: Option<String>,
}

/// 一条影片评论（上游 `JavdbMovieReview`）。
///
/// # 序列化面与 pydantic 一致
///
/// 所有字段**恒出现**（缺省值 `0` / 空串 / `null`）—— pydantic 的
/// `model_validate` + 默认序列化就是这个形状，客户端按这些键读，少一个键
/// 都可能让它把「没有点赞数」误判成「响应坏了」。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JavdbMovieReview {
    pub id: i64,
    pub score: i64,
    pub content: String,
    /// 上游 `parse_external_datetime` 的产物（pydantic 序列化成 ISO8601 或
    /// `null`）。解析失败的**原文丢弃** —— 上游同样返回 `None`，静默丢掉
    /// 一个坏时间戳比让整条评论消失温和得多。
    pub created_at: Option<String>,
    pub username: String,
    pub like_count: i64,
    pub watch_count: i64,
    pub movie: Option<JavdbReviewMovie>,
}

/// JavDB 演员卡片（上游 `JavdbMovieActor`，`metadata/_providers/models.py:48-54`）。
///
/// 键名与上游逐字一致 —— 它会被序列化成 `Value` 交给
/// [`CatalogImportService::upsert_actor_from_javdb_resource`]（那里按同一组
/// 键读），所以改名要同时看两边。
///
/// [`CatalogImportService::upsert_actor_from_javdb_resource`]: super::catalog_import::CatalogImportService::upsert_actor_from_javdb_resource
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct JavdbMovieActor {
    pub javdb_id: String,
    pub javdb_type: i64,
    pub name: String,
    /// 候选名集合（`name` + `name_zht` + `other_name` 拆分去重，见
    /// [`collect_actor_candidate_names`]）。上游 `alias_names` 默认空列表。
    pub alias_names: Vec<String>,
    pub avatar_url: Option<String>,
    /// 本地枚举：未知 `0` / 女性 `1` / 男性 `2`（见 [`map_actor_gender`]）。
    pub gender: i64,
}

/// JavDB 性别值 → 本地枚举（上游 `_map_actor_gender`，`javdb.py:949-967`）。
///
/// # ★ 两套枚举是**反的**，不是同一套
///
/// | JavDB 原始 | 本仓 |
/// |---|---|
/// | `0` | 女性 `1` |
/// | `1` | 男性 `2` |
/// | 其它 / `None` | 未知 `0` |
///
/// 上游先认字符串（`female` / `女` → 女性，`male` / `男` → 男性），再认
/// `"0"` / `"1"` 两个数字串，**其余字符串一律未知**（`"2"` 也是未知 —— 别按
/// 「数字就是本地枚举」想当然）。照抄这个顺序，否则男性会被写成女性。
pub fn map_actor_gender(raw: Option<&Value>) -> i64 {
    let Some(raw) = raw else {
        return 0;
    };
    let numeric = match raw {
        Value::String(text) => {
            let normalized = text.trim().to_lowercase();
            match normalized.as_str() {
                "female" | "女" => return 1,
                "male" | "男" => return 2,
                "0" | "1" => normalized.parse::<i64>().unwrap_or(0),
                // 认不出的字符串（含 "2"、"unknown"）→ 未知。
                _ => return 0,
            }
        }
        other => other.as_i64().unwrap_or(0),
    };
    match numeric {
        0 => 1,
        1 => 2,
        _ => 0,
    }
}

/// 演员的候选名集合（上游 `_collect_actor_candidate_names`，`javdb.py:968-995`）。
///
/// 顺序：`name` → `name_zht` → `other_name` 按 `,` 拆。去重按
/// `casefold`（Rust 的 `to_lowercase` 是它的近似 —— 两者对土耳其语 `İ`、
/// 德语 `ß` 这类边界的处理不同，而那些字符在演员名里不会出现），**保留首次
/// 出现的原形**（不是小写形）。
pub fn collect_actor_candidate_names(actor: &Value) -> Vec<String> {
    let mut candidate_names: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push = |candidate: &str, candidate_names: &mut Vec<String>| {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            return;
        }
        if !seen.insert(candidate.to_lowercase()) {
            return;
        }
        candidate_names.push(candidate.to_owned());
    };

    for key in ["name", "name_zht"] {
        push(
            actor.get(key).and_then(Value::as_str).unwrap_or(""),
            &mut candidate_names,
        );
    }
    let other_name = actor
        .get("other_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    for candidate in other_name.split(',') {
        push(candidate, &mut candidate_names);
    }
    candidate_names
}

/// 一条搜索候选 → 演员卡片。`id` 缺失或为空 → `None`（上游同样跳过）。
pub fn actor_from_search_entry(actor: &Value) -> Option<JavdbMovieActor> {
    let javdb_id = actor
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?
        .to_owned();
    Some(JavdbMovieActor {
        javdb_id,
        javdb_type: actor.get("type").and_then(Value::as_i64).unwrap_or(0),
        name: actor
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        alias_names: collect_actor_candidate_names(actor),
        avatar_url: normalize_image_url(actor.get("avatar_url").and_then(Value::as_str)),
        gender: map_actor_gender(actor.get("gender")),
    })
}

/// 搜索载荷 → 演员卡片（纯函数，**去重在这里**）。
///
/// 上游同一段（`javdb.py:349-369`）：逐条 `model_validate`、按 `id` 去重、
/// 缺 `id` 的跳过。抽成纯函数是为了让「重复卡片只出一条」「缺 id 的条目不算
/// 候选」这两条能在不起 HTTP 的情况下断言。
pub fn actors_from_search_payload(payload: &Value) -> Vec<JavdbMovieActor> {
    let entries = payload
        .get("data")
        .and_then(|data| data.get("actors"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut resources: Vec<JavdbMovieActor> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in &entries {
        let Some(actor) = actor_from_search_entry(entry) else {
            continue;
        };
        if !seen_ids.insert(actor.javdb_id.clone()) {
            continue;
        }
        resources.push(actor);
    }
    resources
}

/// 外部时间戳 → ISO8601 字符串。上游 `parse_external_datetime` 的对位物：
/// RFC3339（含尾随 `Z`）之外还认 `YYYY-MM-DD HH:MM:SS` 与纯日期两种
/// 「JavDB 自己的格式」，后者按 UTC 补时区。认不出返回 `None`。
fn parse_external_datetime(value: &serde_json::Value) -> Option<String> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    // RFC3339（chrono 的解析器原生吃 `Z` 后缀）。
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.to_rfc3339());
    }
    use chrono::TimeZone as _;
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(chrono::Utc.from_utc_datetime(&naive).to_rfc3339());
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text, format) {
            return Some(
                chrono::Utc
                    .from_utc_datetime(&date.and_hms_opt(0, 0, 0)?)
                    .to_rfc3339(),
            );
        }
    }
    None
}

/// 数值兜底。上游 `safe_int(value, default)`：非数值一律回落默认。
fn safe_i64(value: Option<&serde_json::Value>, default: i64) -> i64 {
    value.and_then(serde_json::Value::as_i64).unwrap_or(default)
}

/// 评论的嵌套影片。非对象 → `None`（上游 `_build_review_movie` 的第一道判）。
fn review_movie_from(movie: Option<&serde_json::Value>) -> Option<JavdbReviewMovie> {
    let movie = movie?;
    let object = movie.as_object()?;
    let string_or_empty = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(JavdbReviewMovie {
        id: {
            // `str(movie.get("id") or "")` —— 数字 id 也转成字符串。
            match object.get("id") {
                Some(serde_json::Value::Number(number)) => number.to_string(),
                Some(serde_json::Value::String(text)) => text.clone(),
                _ => String::new(),
            }
        },
        number: string_or_empty("number"),
        title: string_or_empty("title"),
        origin_title: object
            .get("origin_title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        score: object.get("score").and_then(serde_json::Value::as_f64),
        thumb_url: object
            .get("thumb_url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        release_date: object
            .get("release_date")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

/// 单条评论的映射。上游 `_build_movie_review`：键名换算只有两处
/// （`likes_count` → `like_count`、`watched_count` → `watch_count`），
/// 其余逐字。非对象返回 `None`（调用方跳过并 warn —— 上游同样如此）。
pub fn movie_review_from(review: &serde_json::Value) -> Option<JavdbMovieReview> {
    let object = review.as_object()?;
    Some(JavdbMovieReview {
        id: safe_i64(object.get("id"), 0),
        score: safe_i64(object.get("score"), 0),
        content: object
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: object.get("created_at").and_then(parse_external_datetime),
        username: object
            .get("username")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        like_count: safe_i64(object.get("likes_count"), 0),
        watch_count: safe_i64(object.get("watched_count"), 0),
        movie: review_movie_from(object.get("movie")),
    })
}

/// 整份载荷 → 评论列表。上游 `_extract_movie_reviews`（缺 `data.reviews` 是
/// **错误**不是空列表 —— 那说明响应形状变了，当成「没有评论」会让分页静默
/// 断流）+ 逐条映射（非对象的条目跳过，与上游一致）。
pub fn movie_reviews_from(
    payload: &serde_json::Value,
) -> Result<Vec<JavdbMovieReview>, MetadataSourceError> {
    let reviews = payload
        .get("data")
        .and_then(|data| data.get("reviews"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            MetadataSourceError::RequestFailed("JavDB 评论载荷缺 data.reviews".to_owned())
        })?;
    Ok(reviews
        .iter()
        .filter_map(|review| match movie_review_from(review) {
            Some(mapped) => Some(mapped),
            // 类型不对的条目跳过 —— 一条脏数据不该让整页 502。
            None => {
                tracing::warn!("JavDB 评论条目类型异常，已跳过");
                None
            }
        })
        .collect())
}

/// 榜单取数失败。
///
/// # 为什么不复用 [`MetadataSourceError`]
///
/// 那一套是**元数据 provider** 的语义（`NotFound` / `InvalidDelivery` /
/// `Disabled`），而榜单只有「参数不在白名单 / 账号没配 / 登录被拒 / 请求失败」
/// 四种，且**没有 `NotFound`** —— 一个空榜单是**成功**（上游返回 `[]`），不是
/// 「没找到」。混用会让调用方分不清「这个榜今天没数据」与「JavDB 没收录」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavdbRankError {
    /// 参数不在上游白名单里（上游 `ValueError`，`:484-487` 等）。
    ///
    /// 这是**调用方的 bug**，不是 JavDB 的问题 —— 所以它该映射成
    /// `INVALID_ARGUMENT`，而不是「服务不可用」。
    Unsupported(String),
    /// 这个榜需要登录，而账号没配（上游 `JavdbAuthError("javdb account is not
    /// configured")`，`:516`）。
    AccountRequired,
    /// 账号配了但没换成 token（上游 `JavdbAuthError`，`:533` / `:540`）。
    Auth(String),
    /// 请求失败：网络、非 2xx、`success != 1`、响应缺 `data.movies`。
    Request(String),
}

impl std::fmt::Display for JavdbRankError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(detail) => write!(formatter, "榜单参数不受支持：{detail}"),
            Self::AccountRequired => {
                write!(formatter, "该榜单需要登录，而 JavDB 账号没有配置")
            }
            Self::Auth(detail) => write!(formatter, "JavDB 登录失败：{detail}"),
            Self::Request(detail) => write!(formatter, "JavDB 榜单请求失败：{detail}"),
        }
    }
}

impl std::error::Error for JavdbRankError {}

/// JavDB provider。
#[derive(Debug, Clone)]
pub struct JavdbProvider {
    client: reqwest::Client,
    /// 拼 URL 用的前缀，**末尾无斜杠**。生产是 `https://{host}`。
    base: String,
}

impl JavdbProvider {
    /// 构造。`host` **不带** `https://`（上游形状）。
    ///
    /// 客户端刻意用 `no_proxy()`：JavDB 是**直连公网**的目标，走环境里的
    /// HTTP 代理会让「能不能搜」取决于代理配置 —— 那是一个与本服务无关的
    /// 故障源。同样的理由见 `TorznabClient`。
    pub fn new(host: &str) -> Result<Self, MetadataSourceError> {
        let host = host.trim().trim_end_matches('/');
        if host.is_empty() {
            return Err(MetadataSourceError::RequestFailed(
                "javdb.host 为空".to_owned(),
            ));
        }
        Self::with_base_url(&format!("https://{host}"))
    }

    /// 直接给 base URL（**带 scheme**）。测试打桩与「走反向代理」用。
    ///
    /// # 为什么需要这个缝
    ///
    /// 上游把 `https://` 写死在 `_build_api_url` 里，于是「本机起一个假 JavDB」
    /// 不可能 —— 而那正是唯一能覆盖「候选里挑番号**完全相等**的那个」与
    /// 「`success != 1` 不是 404」这两条实现级断言的办法。它也是**代理**场景
    /// 唯一说得通的入口：把 base 指到内网网关，其余代码一行不改。
    pub fn with_base_url(base: &str) -> Result<Self, MetadataSourceError> {
        let base = base.trim().trim_end_matches('/').to_owned();
        if base.is_empty() {
            return Err(MetadataSourceError::RequestFailed(
                "JavDB base URL 为空".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| {
                MetadataSourceError::RequestFailed(format!("构造 JavDB 客户端失败：{error}"))
            })?;
        Ok(Self { client, base })
    }

    /// 按名字搜演员（上游 `search_actors`，`javdb.py:326-372`）。
    ///
    /// # 三条与「搜影片」不同的规矩
    ///
    /// 1. **不查 `success`** —— 上游这条路径同样只看 `data.actors`（搜影片那条
    ///    也是），业务失败会**表现为空列表**，进而 [`MetadataSourceError::NotFound`]。
    /// 2. **候选为空即 NotFound**，不是空列表：调用方（演员 SSE）要据此发
    ///    `actor_not_found` 的 `completed` 帧 —— 返回空 Vec 会变成
    ///    「导入 0 个」的谎报（「搜不了」与「没搜到」在用户眼里是两回事）。
    /// 3. **这里就按 id 去重**（上游同一处 `seen_actor_ids`）：JavDB 的同名卡片
    ///    会重复出现，重复项进 SSE 就是两次下载、两次入库。
    ///
    /// `q` 在最前、其余按 `ACTOR_SEARCH_PARAMS` 声明序 —— 与上游字典序一致
    /// （理由见 `api_url` 方法）。
    pub async fn search_actor_resources(
        &self,
        actor_name: &str,
    ) -> Result<Vec<JavdbMovieActor>, MetadataSourceError> {
        let mut query: Vec<(&str, String)> = vec![("q", actor_name.to_owned())];
        for (key, value) in ACTOR_SEARCH_PARAMS {
            query.push((key, value.to_owned()));
        }
        query.push(("page", "1".to_owned()));
        query.push(("limit", ACTOR_SEARCH_LIMIT.to_string()));
        let url = self.api_url(API_PATH_SEARCH, &query);
        let payload = self.request_json(&url).await?;

        let resources = actors_from_search_payload(&payload);
        if resources.is_empty() {
            // 上游两处 `raise MetadataNotFoundError("actor", actor_name)`：
            // 原始候选为空、或全都被过滤掉（缺 id），对调用方是同一件事。
            tracing::warn!("JavDB 演员搜索无有效候选");
            return Err(MetadataSourceError::NotFound);
        }
        Ok(resources)
    }

    /// 拼完整 URL（上游 `_build_api_url`）。
    ///
    /// 查询参数的顺序**照抄上游的字典序**（`q` 在最前，其余按
    /// `API_PARAMS_MOVIE_SEARCH` 的声明序）。请求 URL 会被 JavDB 记进日志，
    /// 顺序不一致会让「同一请求」在两边对不上，而排查时那是最先要排除的
    /// 变量。
    fn api_url(&self, path: &str, query: &[(&str, String)]) -> String {
        let base = format!("{}{}", self.base, path);
        if query.is_empty() {
            return base;
        }
        let encoded: Vec<String> = query
            .iter()
            .map(|(key, value)| format!("{key}={}", encode_component(value)))
            .collect();
        format!("{base}?{}", encoded.join("&"))
    }

    /// 每次请求都要带的两个头（上游 `JavdbProvider.build_request_headers`，
    /// `javdb.py:661-667`）。
    ///
    /// ★ `jdsignature` 必须**当场算**，不能缓存：`signature_at` 的参数是 Unix
    /// 秒，服务端据此判新鲜度 —— 复用一个旧签名会拿到 HTTP **200** 的
    /// `{"success":0,"action":"ParameterInvalid"}`（见 [`signature_at`]）。
    ///
    /// 上游那份头里还有 `connection` 与 `host`，本函数**不搬**（理由见
    /// [`Self::request_json`] 的文档）：它们由客户端托管，显式设置会与连接复用打架。
    fn common_headers(&self) -> Result<reqwest::header::HeaderMap, MetadataSourceError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("zh-TW"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("jdsignature"),
            reqwest::header::HeaderValue::from_str(&signature_at(chrono::Utc::now().timestamp()))
                .map_err(|error| {
                MetadataSourceError::RequestFailed(format!("构造 JavDB 签名头失败：{error}"))
            })?,
        );
        Ok(headers)
    }

    /// 发一次请求并解析 JSON；`extra` 里的头**覆盖**默认头。
    ///
    /// 上游 `MetadataRequestClient._request`（`http_client.py:44-46`）先在
    /// `build_request_headers()` 之后 `update(headers)` —— 所以登录那次的
    /// `user-agent` 会盖掉客户端的浏览器 UA，而 `jdsignature` 仍在。顺序照抄。
    ///
    /// # 单次尝试，不重试
    ///
    /// 上游最多重试 4 次（`http_client.py:48`：408/429/500/502/503/504 与网络
    /// 错误，退避 `backoff_delay`）。本仓的 JavDB 客户端**一直**是单次（既有的
    /// 搜索 / 详情路径亦然），这里不为榜单单独加一层 —— 榜单是定时任务，一次
    /// 失败留到下一轮比在这里占住 worker 更合适。要改就连同既有路径一起改。
    async fn send_json(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
        extra: &[(&str, &str)],
    ) -> Result<Value, MetadataSourceError> {
        let mut headers = self.common_headers()?;
        for (name, value) in extra {
            let header_name =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    MetadataSourceError::RequestFailed(format!(
                        "构造 JavDB 请求头 {name} 失败：{error}"
                    ))
                })?;
            let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                MetadataSourceError::RequestFailed(format!(
                    "构造 JavDB 请求头 {name} 失败：{error}"
                ))
            })?;
            headers.insert(header_name, header_value);
        }
        let mut request = self.client.request(method, url).headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(|error| {
            MetadataSourceError::RequestFailed(format!("请求 JavDB 失败 {url}: {error}"))
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            MetadataSourceError::RequestFailed(format!("读 JavDB 响应失败 {url}: {error}"))
        })?;
        if !status.is_success() {
            return Err(MetadataSourceError::RequestFailed(format!(
                "JavDB 返回 {status}：{}",
                truncate(&body)
            )));
        }
        serde_json::from_str(&body).map_err(|error| {
            MetadataSourceError::RequestFailed(format!("JavDB 响应不是 JSON：{error}"))
        })
    }

    /// 发一次 GET 并解析 JSON。上游 `request_json`（`http_client.py:37-44`）。
    ///
    /// # 每个请求都必须带 `jdsignature`
    ///
    /// 上游 `MetadataRequestClient._request:44` 在**每次请求**都调
    /// `build_request_headers()`，而 `JavdbProvider` 覆盖它，补上
    /// `jdsignature` + `accept-language`（`javdb.py:661-667`）。少了这个头，
    /// JavDB 一律回 `{"success":0,"action":"ParameterInvalid","message":
    /// "參數不能爲空: jdsignature"}`，且 **HTTP 仍是 200** —— 于是：
    ///
    /// - 详情路径查了 `success` → 报「请求失败」（还算诚实）；
    /// - 搜索路径**不查** `success`（上游 `_search_movie` 同样不查）→ 候选为空
    ///   → 报 `NotFound`，看起来像「JavDB 没收录这部片」。
    ///
    /// 后者就是「**所有**番号都查不到」这个症状的来源，见 [`signature_at`]。
    ///
    /// 上游那份头里还有 `connection` 与 `host`，本函数**不搬**：它们是客户端
    /// 托管的 —— HTTP/1.1 默认即 keep-alive，`Host` 由 hyper 按 URL 自动填，
    /// 显式设置反而会与连接复用打架。只搬真正承载语义的那两个。
    async fn request_json(&self, url: &str) -> Result<Value, MetadataSourceError> {
        self.send_json(reqwest::Method::GET, url, None, &[]).await
    }

    /// 按番号搜候选（上游 `_search_movie`，`:386-414`）。
    ///
    /// 候选按 `release_date` **倒序**后，取第一个「番号归一后完全相等」的。
    /// 精确相等而不是包含：JavDB 的模糊搜索会把 `ABC-123` 与 `ABC-1234`、
    /// `ABC-123B` 混在一起，而导入要的是**同一部片**。
    async fn search_movie(&self, movie_number: &str) -> Result<Value, MetadataSourceError> {
        let normalized = crate::movie_numbers::normalize_movie_number(movie_number);
        let mut query: Vec<(&str, String)> = vec![("q", normalized.clone())];
        for (key, value) in MOVIE_SEARCH_PARAMS {
            query.push((key, value.to_owned()));
        }
        let url = self.api_url(API_PATH_SEARCH, &query);
        let payload = self.request_json(&url).await?;

        let mut candidates: Vec<&Value> = payload
            .get("data")
            .and_then(|data| data.get("movies"))
            .and_then(Value::as_array)
            .map(|movies| movies.iter().collect())
            .unwrap_or_default();
        if candidates.is_empty() {
            return Err(MetadataSourceError::NotFound);
        }
        // `release_date` 缺失当空串 —— 与上游 `m.get("release_date") or ""` 一致，
        // 且排序必须是**稳定**的：同日期的候选要保持 API 返回的次序。
        candidates.sort_by(|left, right| release_date(right).cmp(release_date(left)));
        candidates
            .into_iter()
            .find(|movie| {
                let number = movie
                    .get("number")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                crate::movie_numbers::normalize_movie_number(number) == normalized
            })
            .cloned()
            .ok_or(MetadataSourceError::NotFound)
    }

    /// 取详情 payload（上游 `_get_movie_detail_payload`，`:699-`）。
    async fn movie_detail_payload(&self, javdb_id: &str) -> Result<Value, MetadataSourceError> {
        let path = API_PATH_MOVIE_DETAIL.replace("{javdb_id}", javdb_id);
        let url = self.api_url(&path, &[("from_rankings", "true".to_owned())]);
        let payload = self.request_json(&url).await?;
        // `success != 1` 是 JavDB 的「业务失败」标志（HTTP 仍是 200）。
        // 不看它的话，一个 `{"success":0,"message":"..."}` 会被当成「没有这部片」
        // → 404，而真实原因是服务端拒绝了请求。
        if payload.get("success").and_then(Value::as_i64) != Some(1) {
            let detail = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unexpected success")
                .to_owned();
            return Err(MetadataSourceError::RequestFailed(format!(
                "JavDB 详情返回失败：{detail}"
            )));
        }
        payload
            .get("data")
            .and_then(|data| data.get("movie"))
            .filter(|movie| !movie.is_null())
            .cloned()
            .ok_or(MetadataSourceError::NotFound)
    }

    /// 播放榜番号（上游 `get_playback_rank_numbers`，`:611-659`）。
    ///
    /// `filter_by` = `all`（热播）/ `high_score`（高评分）；`period` =
    /// `daily` / `weekly` / `monthly`。**不需要登录** —— 上游这个方法直接
    /// `request_json`，不碰 `_ensure_logged_in`。
    pub async fn playback_rank_numbers(
        &self,
        filter_by: &str,
        period: &str,
    ) -> Result<Vec<String>, JavdbRankError> {
        if !SUPPORTED_PLAYBACK_FILTERS.contains(&filter_by) {
            return Err(JavdbRankError::Unsupported(format!(
                "不支持的 filter_by：{filter_by}"
            )));
        }
        if !SUPPORTED_RANK_PERIODS.contains(&period) {
            return Err(JavdbRankError::Unsupported(format!(
                "不支持的 period：{period}"
            )));
        }
        let url = self.api_url(
            API_PATH_RANKINGS_PLAYBACK,
            &[
                ("filter_by", filter_by.to_owned()),
                ("period", period.to_owned()),
            ],
        );
        let payload = self
            .send_json(reqwest::Method::GET, &url, None, &[])
            .await
            .map_err(rank_request_error)?;
        rank_numbers_from(&payload)
    }

    /// 影片评论（上游 `get_movie_reviews_by_javdb_id`，`:444-481`）。
    ///
    /// `sort_by` 缺省或空串**不进查询串**（上游 `if sort_by:`）—— 服务层给的
    /// `recently` / `hotly` 之外的原样透传，**这里不设白名单**：排序枚举的
    /// 校验是调用方（路由层）的事，provider 只做传输。
    ///
    /// # 不需要登录
    ///
    /// 上游这条直接 `request_json`，不碰 `_ensure_logged_in` —— 评论接口在
    /// 未登录态可用。别顺手加登录：多一次登录请求 = 多一次风控暴露。
    pub async fn movie_reviews(
        &self,
        javdb_id: &str,
        page: i64,
        limit: i64,
        sort_by: Option<&str>,
    ) -> Result<Vec<JavdbMovieReview>, MetadataSourceError> {
        let path = API_PATH_MOVIE_REVIEWS.replace("{javdb_id}", javdb_id);
        let mut query: Vec<(&str, String)> =
            vec![("page", page.to_string()), ("limit", limit.to_string())];
        if let Some(sort) = sort_by.map(str::trim).filter(|sort| !sort.is_empty()) {
            query.push(("sort_by", sort.to_owned()));
        }
        let url = self.api_url(&path, &query);
        let payload = self.request_json(&url).await?;
        // `success != 1` 是业务失败（HTTP 仍是 200）—— 与详情路径同一处理。
        if payload.get("success").and_then(serde_json::Value::as_i64) != Some(1) {
            let detail = payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unexpected success")
                .to_owned();
            return Err(MetadataSourceError::RequestFailed(format!(
                "JavDB 评论返回失败：{detail}"
            )));
        }
        movie_reviews_from(&payload)
    }

    /// 有码 / 无码 / FC2 榜番号（上游 `get_rank_numbers`，`:483-501`）。
    ///
    /// `video_type` = `0`（有码）/ `1`（无码）/ `3`（FC2）。同样是免费榜，
    /// 不需要登录。注意上游把 `video_type` 放在查询参数 **`type`** 上（`:810`），
    /// 不是 `video_type` —— 写错参数名会拿到一个 HTTP 200 的空榜。
    pub async fn rank_numbers(
        &self,
        video_type: &str,
        period: &str,
    ) -> Result<Vec<String>, JavdbRankError> {
        if !SUPPORTED_RANK_VIDEO_TYPES.contains(&video_type) {
            return Err(JavdbRankError::Unsupported(format!(
                "不支持的 video_type：{video_type}"
            )));
        }
        if !SUPPORTED_RANK_PERIODS.contains(&period) {
            return Err(JavdbRankError::Unsupported(format!(
                "不支持的 period：{period}"
            )));
        }
        let url = self.api_url(
            API_PATH_RANKINGS,
            &[
                ("type", video_type.to_owned()),
                ("period", period.to_owned()),
            ],
        );
        let payload = self
            .send_json(reqwest::Method::GET, &url, None, &[])
            .await
            .map_err(rank_request_error)?;
        rank_numbers_from(&payload)
    }

    /// TOP250 番号（上游 `get_top_numbers`，`:546-609`）。**需登录**。
    ///
    /// `top_type` = `all` / `year` / `video_type`；`type_value` 在 `year` 下是
    /// 年份、在 `video_type` 下是 `0`/`1`/`3`、`all` 下是空串。逐页抓到
    /// `max_pages`（缺省 [`TOP250_MAX_PAGES`]），**空页即到底**并提前停
    /// （上游 `if not movies: break`，`:596-598`）—— 所以数据不满 250 的历史
    /// 年份不会白跑后面几页。
    pub async fn top_numbers(
        &self,
        account: &JavdbAccount,
        top_type: &str,
        type_value: &str,
        max_pages: Option<i32>,
    ) -> Result<Vec<String>, JavdbRankError> {
        if !SUPPORTED_TOP_TYPES.contains(&top_type) {
            return Err(JavdbRankError::Unsupported(format!(
                "不支持的 top_type：{top_type}"
            )));
        }
        let page_count = max_pages.map_or(TOP250_MAX_PAGES, i64::from);
        // 登录**先于**翻页：`max_pages = 0` 时上游也先登录（`_ensure_logged_in`
        // 在循环之前），于是一个「账号不对」的调用会以登录错误收场，而不是
        // 静悄悄返回空榜。
        let token = self.login_token(account).await?;
        let authorization = format!("Bearer {token}");

        let mut numbers: Vec<String> = Vec::new();
        for page in 1..=page_count {
            let url = self.api_url(
                API_PATH_MOVIES_TOP,
                &[
                    ("start_rank", "1".to_owned()),
                    ("type", top_type.to_owned()),
                    ("type_value", type_value.to_owned()),
                    ("ignore_watched", "false".to_owned()),
                    ("page", page.to_string()),
                    ("limit", TOP250_PAGE_LIMIT.to_string()),
                ],
            );
            let payload = self
                .send_json(
                    reqwest::Method::GET,
                    &url,
                    None,
                    &[("authorization", authorization.as_str())],
                )
                .await
                .map_err(rank_request_error)?;
            let page_numbers = rank_numbers_from(&payload)?;
            if page_numbers.is_empty() {
                break;
            }
            numbers.extend(page_numbers);
        }
        Ok(numbers)
    }

    /// 换一个登录 token（上游 `_ensure_logged_in`，`:508-544`）。
    ///
    /// # 为什么这里没有「记住 token」与「失败后不再重试」
    ///
    /// 上游把 token 缓存在**实例**上（`self._token` / `self._login_failed`），
    /// 两者的前提都是「一个 provider 跨多次抓取存活」——`per_run_provider_holder`
    /// 正是为这个前提写的。本仓的 provider 是**按 rpc 调用就地构造**的（宿主不
    /// 保管账号），登录与使用在同一次调用里，所以那两个状态位没有存在的意义；
    /// 「一次失败不再重试」由调用方在下一轮重新登录自然满足。
    async fn login_token(&self, account: &JavdbAccount) -> Result<String, JavdbRankError> {
        if !account.is_configured() {
            return Err(JavdbRankError::AccountRequired);
        }
        let url = self.api_url(API_PATH_SESSIONS, &[]);
        // 字段顺序照抄上游 `{"username", "password", **device_payload}`（`:519-523`）。
        let mut fields: Vec<(String, String)> = vec![
            ("username".to_owned(), account.username.clone()),
            ("password".to_owned(), account.password.clone()),
            ("device_uuid".to_owned(), account.device_uuid()),
        ];
        for (key, value) in DEVICE {
            fields.push((key.to_owned(), value.to_owned()));
        }
        let payload = self
            .send_json(
                reqwest::Method::POST,
                &url,
                Some(encode_form(&fields)),
                // 上游登录额外头（`:524-528`）。★ 两处「看起来不对但服务端就认它」：
                // UA 换成 `Dart/3.5`，`Content-Type` 声明 multipart 而**实体是
                // form-urlencoded**（上游注释原话：「服务端虽声明 multipart，但
                // 接受 form-urlencoded 提交」）。照抄，别改。
                &[
                    ("user-agent", LOGIN_USER_AGENT),
                    ("content-type", LOGIN_CONTENT_TYPE),
                ],
            )
            .await
            .map_err(|error| JavdbRankError::Auth(request_detail(error)))?;
        payload
            .get("data")
            .and_then(|data| data.get("token"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                // 上游 `body.get("message") or "login response missing token"`（`:538`）
                // —— 服务端给了原因就用原因，没给才是这句兜底。
                let detail = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("login response missing token");
                JavdbRankError::Auth(detail.to_owned())
            })
    }
}

#[tonic::async_trait]
impl MetadataProvider for JavdbProvider {
    /// 上游 `get_movie_by_number`（`:430-439`）：先搜到 id，再取详情。
    async fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<Value>, MetadataSourceError> {
        let movie = self.search_movie(movie_number).await?;
        let javdb_id = movie
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(MetadataSourceError::NotFound)?;
        self.movie_detail_payload(javdb_id).await.map(Some)
    }

    /// 上游 `get_movie_by_javdb_id`（`:416-428`）。
    async fn get_movie_by_javdb_id(
        &self,
        javdb_id: &str,
    ) -> Result<Option<Value>, MetadataSourceError> {
        self.movie_detail_payload(javdb_id).await.map(Some)
    }

    /// ★ **本批未实现**，显式报错而不是返回空列表。
    ///
    /// 上游 `search_actors`（`:326-372`）要另一个 API 形状（`type=actor`）。
    /// 返回空列表会让调用方（演员 SSE，`metadata_source.rs:478`）报
    /// 「导入 0 个」—— 那是**谎报**：用户看到「没搜到」而不是「搜不了」。
    async fn search_series(
        &self,
        series_name: &str,
    ) -> Result<Vec<JavdbSeries>, MetadataSourceError> {
        // 系列搜索的固定参数（上游 `API_PARAMS_SERIES_SEARCH`）。⚠️ 上游常量里
        // 的 `page: 1` 会被调用方的显式 page **覆盖**（dict 后写胜出）—— 这里
        // 不把 page 放进常量，直接追加，语义相同且不产生重复键。
        let mut query: Vec<(&str, String)> = vec![("q", series_name.to_owned())];
        for (key, value) in SERIES_SEARCH_PARAMS {
            query.push((key, value.to_owned()));
        }
        query.push(("page", "1".to_owned()));
        let url = self.api_url(API_PATH_SEARCH, &query);
        let payload = self.request_json(&url).await?;

        let series_list = payload
            .get("data")
            .and_then(|data| data.get("series"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(series_list
            .iter()
            .filter_map(|series| {
                let javdb_id = series.get("id").and_then(Value::as_str)?;
                if javdb_id.is_empty() {
                    return None;
                }
                Some(JavdbSeries {
                    javdb_id: javdb_id.to_owned(),
                    javdb_type: series.get("type").and_then(Value::as_i64).unwrap_or(0),
                    name: series
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    videos_count: series
                        .get("videos_count")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                })
            })
            .collect())
    }

    async fn get_series_movies(
        &self,
        series_id: &str,
        series_type: i64,
    ) -> Result<Vec<JavdbMovieListItem>, MetadataSourceError> {
        // 逐页拉到空页为止（上游 `get_series_movies:229-255` 同一循环）。
        let mut movies: Vec<JavdbMovieListItem> = Vec::new();
        let mut page = 1_i64;
        loop {
            let mut query: Vec<(&str, String)> =
                vec![("filter_by", format!("{series_type}:s:{series_id}"))];
            for (key, value) in SERIES_MOVIES_PARAMS {
                query.push((key, value.to_owned()));
            }
            query.push(("page", page.to_string()));
            let url = self.api_url(API_PATH_MOVIES_TAGS, &query);
            let payload = self.request_json(&url).await?;

            let batch = payload
                .get("data")
                .and_then(|data| data.get("movies"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for movie in batch {
                movies.push(JavdbMovieListItem {
                    javdb_id: movie
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    movie_number: movie
                        .get("number")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    title: movie
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    cover_image: normalize_image_url(
                        movie.get("cover_url").and_then(Value::as_str),
                    ),
                });
            }
            page += 1;
        }
        Ok(movies)
    }

    /// 搜演员（上游 `search_actors`，`javdb.py:326-372`）。返回 `Vec<Value>`
    /// 只是为了满足 [`MetadataProvider`] 的窄缝（`match_actors` 拿它喂
    /// `upsert_actor(&Value)`）；真正的映射在
    /// [`JavdbProvider::search_actor_resources`]，这里只做一次搬运。
    async fn search_actors(&self, keyword: &str) -> Result<Vec<Value>, MetadataSourceError> {
        let resources = self.search_actor_resources(keyword).await?;
        resources
            .iter()
            .map(|actor| {
                serde_json::to_value(actor).map_err(|error| {
                    // 自造的结构序列化不会失败；真失败说明结构写错了 ——
                    // 报出来，别静默丢一位演员。
                    MetadataSourceError::RequestFailed(format!("演员资源序列化失败：{error}"))
                })
            })
            .collect()
    }
}

/// 出网的 UA。上游走官方 App 的接口，带一个浏览器 UA 即可（不带登录态）。
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0 Safari/537.36";

/// 候选的 `release_date`（缺失当空串，与上游 `or ""` 一致）。
fn release_date(movie: &Value) -> &str {
    movie
        .get("release_date")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// 查询参数的百分号编码（上游 `urlencode(..., safe=':-')`）。
///
/// 保留 `:` 与 `-` 是为了番号 `ABC-123` 与带冒号的值不被编码成 `%3A` ——
/// JavDB 按字面量匹配，写 `%3A` 会搜不到。
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 错误消息里的响应体截断（上游日志只打摘要）。
fn truncate(body: &str) -> String {
    const LIMIT: usize = 200;
    if body.chars().count() <= LIMIT {
        return body.to_owned();
    }
    let head: String = body.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// 登录请求的 UA（上游 `:526` 硬编码 `Dart/3.5 (dart:io)`）。
const LOGIN_USER_AGENT: &str = "Dart/3.5 (dart:io)";
/// 登录请求声明的 `Content-Type`（上游 `:527`）。**实体其实是
/// form-urlencoded** —— 见 [`JavdbProvider::login_token`] 里的说明。
const LOGIN_CONTENT_TYPE: &str = "multipart/form-data";

/// 从榜单响应里取番号。
///
/// 上游三处是**同一段循环**（`:489-494` / `:599-602` / `:648-652`），所以这里
/// 收成一个函数；三种响应的信封也一致（播放榜的注释原话：「playback 响应与
/// `/api/v1/rankings` 同构，带 `success` 包络」）。
///
/// 三件事照抄上游：
/// 1. `success != 1` 是**请求失败**（HTTP 仍是 200），不是「这个榜是空的」；
/// 2. 缺 `data.movies`（或它不是数组）同样是失败（上游 `isinstance(movies, list)`）；
/// 3. **空数组不是失败** —— 它表示这个榜此刻没有条目，TOP250 的历史年份就会这样。
///
/// 一处刻意收窄：上游 `if number:` 收**任意非空真值**，本函数只认字符串
/// （JavDB 的 `number` 要么是字符串要么是 `null`；其余类型按「没有这个字段」处理）。
fn rank_numbers_from(payload: &Value) -> Result<Vec<String>, JavdbRankError> {
    if payload.get("success").and_then(Value::as_i64) != Some(1) {
        let detail = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unexpected success");
        return Err(JavdbRankError::Request(format!(
            "榜单请求返回失败：{detail}"
        )));
    }
    let movies = payload
        .get("data")
        .and_then(|data| data.get("movies"))
        .and_then(Value::as_array)
        .ok_or_else(|| JavdbRankError::Request("榜单响应缺 data.movies".to_owned()))?;
    Ok(movies
        .iter()
        .filter_map(|movie| movie.get("number").and_then(Value::as_str))
        .filter(|number| !number.is_empty())
        .map(str::to_owned)
        .collect())
}

/// `send_json` 的错误细节。
///
/// 它只会产出 `RequestFailed`（唯一的失败形态），所以这里取那一支的串；
/// 其余分支在当前实现下不可达，兜底给 `Debug` 而不是 `unreachable!()` ——
/// 多一个错误分支不会崩，少一个 `panic` 却可能让一次同步整批失败。
fn request_detail(error: MetadataSourceError) -> String {
    match error {
        MetadataSourceError::RequestFailed(detail) => detail,
        other => format!("{other:?}"),
    }
}

/// 榜单路径上的传输错误：细节串原样带过去（见 [`request_detail`]）。
fn rank_request_error(error: MetadataSourceError) -> JavdbRankError {
    JavdbRankError::Request(request_detail(error))
}

/// `application/x-www-form-urlencoded` 的实体编码。
///
/// httpx 传 `data=dict` 时走 `urlencode` → `quote_plus`：**空格编成 `+`**、
/// 安全集是 `_.-~`（不含查询串里保留的 `:`）。这跟 [`encode_component`] 的
/// 规则**不同**（那边 `:` 与 `-` 都保留，是给番号用的），所以两个函数不能合并
/// —— 合并任一个都会在另一处编错。
fn encode_form(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_form_component(key),
                encode_form_component(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 表单串里的单个分量（`quote_plus` 的规则）。
fn encode_form_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ JavDB 与本地是**两套反的**性别枚举，逐个取值对拍。
    ///
    /// 这张表的价值在于「`0` 不是未知、`1` 不是女性」这两条反直觉的事实 ——
    /// 按本地枚举的直觉写一次，全部男性演员会变成女性。
    #[test]
    fn actor_gender_mapping_is_inverted_on_purpose() {
        let cases: [(Option<serde_json::Value>, i64); 9] = [
            (None, 0),
            (Some(serde_json::json!(0)), 1),
            (Some(serde_json::json!(1)), 2),
            (Some(serde_json::json!(2)), 0),
            (Some(serde_json::json!("female")), 1),
            (Some(serde_json::json!("女")), 1),
            (Some(serde_json::json!("MALE")), 2),
            (Some(serde_json::json!(" 男 ")), 2),
            // 认不出的字符串（含 "2"）→ 未知，**不是**按数字理解。
            (Some(serde_json::json!("2")), 0),
        ];
        for (raw, expected) in cases {
            assert_eq!(map_actor_gender(raw.as_ref()), expected, "raw = {raw:?}");
        }
    }

    /// 候选名：`name` → `name_zht` → `other_name` 按 `,` 拆，**大小写不敏感去重**、
    /// 保留首次出现的原形。
    #[test]
    fn candidate_names_are_collected_and_deduplicated() {
        let actor = serde_json::json!({
            "name": "Alice",
            "name_zht": " 艾丽丝 ",
            "other_name": "alice, Bob ,, 艾丽丝",
        });
        assert_eq!(
            collect_actor_candidate_names(&actor),
            vec!["Alice".to_owned(), "艾丽丝".to_owned(), "Bob".to_owned()]
        );
        // 三个键都缺 → 空集合，不 panic。
        assert!(collect_actor_candidate_names(&serde_json::json!({})).is_empty());
    }

    /// 一条搜索候选的映射：`id` 字符串化、头像走 CDN 归一、`type` 缺省 0。
    #[test]
    fn a_search_entry_maps_into_an_actor_card() {
        let actor = actor_from_search_entry(&serde_json::json!({
            "id": "Act123",
            "type": 2,
            "name": "上原",
            "name_zht": "上原",
            "avatar_url": "avatars/a1.jpg",
            "gender": 1,
        }))
        .expect("合法候选");
        assert_eq!(actor.javdb_id, "Act123");
        assert_eq!(actor.javdb_type, 2);
        assert_eq!(actor.name, "上原");
        assert_eq!(actor.alias_names, vec!["上原".to_owned()], "同名去重");
        assert_eq!(
            actor.avatar_url.as_deref(),
            Some("https://c0.jdbstatic.com/avatars/a1.jpg")
        );
        assert_eq!(actor.gender, 2, "JavDB 的 1 = 男性");
        // 缺 id / 空 id → 不是候选（上游同一处跳过）。
        assert!(actor_from_search_entry(&serde_json::json!({"name": "x"})).is_none());
        assert!(actor_from_search_entry(&serde_json::json!({"id": "  "})).is_none());
    }

    /// 载荷解析：**按 id 去重**（重复卡片只出一条）、缺 id 的条目不算候选。
    ///
    /// 去重必须在进 SSE 之前完成：重复项会让同一位演员被下载/入库两次。
    #[test]
    fn the_search_payload_deduplicates_by_javdb_id() {
        let payload = serde_json::json!({
            "data": { "actors": [
                {"id": "A1", "name": "甲"},
                {"id": "A1", "name": "甲的重复卡片"},
                {"name": "没有 id"},
                {"id": "A2", "name": "乙"},
            ]}
        });
        let actors = actors_from_search_payload(&payload);
        assert_eq!(actors.len(), 2);
        assert_eq!(actors[0].javdb_id, "A1");
        assert_eq!(actors[0].name, "甲", "保留首次出现的那张卡片");
        assert_eq!(actors[1].javdb_id, "A2");
        // 形状不对（缺 data.actors）→ 空，由调用方转成 NotFound。
        assert!(actors_from_search_payload(&serde_json::json!({"data": {}})).is_empty());
    }

    /// 单条评论映射的键名换算（上游 `_build_movie_review` 逐字段对拍）：
    /// `likes_count` → `like_count`、`watched_count` → `watch_count`，
    /// 缺省值 `0` / 空串，嵌套影片带字符串化 id。
    #[test]
    fn a_review_entry_maps_field_names_and_defaults() {
        let review = movie_review_from(&serde_json::json!({
            "id": 42,
            "score": 8,
            "content": "好片",
            "created_at": "2024-01-02 03:04:05",
            "username": "alice",
            "likes_count": 3,
            "watched_count": 9,
            "movie": { "id": 123, "number": "SSNI-888", "title": "标题" },
        }))
        .expect("合法条目");
        assert_eq!(review.id, 42);
        assert_eq!(review.score, 8);
        assert_eq!(review.content, "好片");
        // JavDB 自己的 `YYYY-MM-DD HH:MM:SS` 格式按 UTC 补时区。
        assert_eq!(
            review.created_at.as_deref(),
            Some("2024-01-02T03:04:05+00:00")
        );
        assert_eq!(review.username, "alice");
        assert_eq!(review.like_count, 3, "键名换算：likes_count");
        assert_eq!(review.watch_count, 9, "键名换算：watched_count");
        let movie = review.movie.expect("嵌套影片在");
        assert_eq!(movie.id, "123", "数字 id 字符串化");
        assert_eq!(movie.number, "SSNI-888");
        assert_eq!(movie.title, "标题");
        assert_eq!(movie.origin_title, None);
        assert_eq!(movie.score, None);
    }

    /// 类型异常的输入**逐项兜底**而不是报错：坏时间戳丢弃（不是整条丢弃），
    /// 非对象的嵌套影片变 `None`，数值型缺省 `0`。上游 pydantic 验不过会炸，
    /// 那是因为它的模型校验在「逐条构建」之后；本仓的映射器同时承担
    /// 「容错」职责 —— 一条脏数据不该让整页 502。
    #[test]
    fn malformed_fields_fall_back_instead_of_failing() {
        let review = movie_review_from(&serde_json::json!({
            "id": "not-a-number",
            "content": null,
            "created_at": "完全不是时间",
            "movie": "不是对象",
        }))
        .expect("条目本身是对象就映射");
        assert_eq!(review.id, 0, "非数值 id 回落默认");
        assert_eq!(review.content, "", "null content 变空串");
        assert_eq!(review.created_at, None, "认不出的时间戳丢弃");
        assert_eq!(review.movie, None, "非对象嵌套影片变 None");
    }

    /// 缺 `data.reviews` 是**错误**不是空列表 —— 那说明响应形状变了，
    /// 当成「没有评论」会让分页静默断流。
    #[test]
    fn a_payload_without_the_reviews_list_is_an_error() {
        assert!(movie_reviews_from(&serde_json::json!({ "success": 1 })).is_err());
        assert!(movie_reviews_from(&serde_json::json!({
            "success": 1, "data": { "reviews": "不是数组" }
        }))
        .is_err());
    }

    /// 非对象的条目**跳过**（上游同样 `continue`），其余照常返回。
    #[test]
    fn invalid_entries_are_skipped_not_failing() {
        let reviews = movie_reviews_from(&serde_json::json!({
            "success": 1,
            "data": { "reviews": [ { "id": 1 }, "脏数据", 3 ] }
        }))
        .expect("合法载荷");
        assert_eq!(reviews.len(), 1, "只留下对象条目");
        assert_eq!(reviews[0].id, 1);
    }

    #[test]
    fn image_urls_are_rewritten_to_the_cdn() {
        assert_eq!(
            normalize_image_url(Some("covers/abc.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/covers/abc.jpg")
        );
        assert_eq!(
            normalize_image_url(Some("pics/samples/x.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/samples/x.jpg")
        );
        assert_eq!(
            normalize_image_url(Some("avatars/y.jpg")).as_deref(),
            Some("https://c0.jdbstatic.com/avatars/y.jpg")
        );
        // 不含那三个关键词的**原样返回**（上游 `:134`）。
        assert_eq!(
            normalize_image_url(Some("https://other.example/z.jpg")).as_deref(),
            Some("https://other.example/z.jpg")
        );
        // 空 / 缺值 → None，而不是 `Some("")`。
        assert_eq!(normalize_image_url(None), None);
        assert_eq!(normalize_image_url(Some("")), None);
    }

    #[test]
    fn the_url_is_https_plus_host_plus_path() {
        let provider = JavdbProvider::new("javdb.com").expect("构造");
        assert_eq!(
            provider.api_url("/api/v2/search", &[]),
            "https://javdb.com/api/v2/search"
        );
        // 末尾斜杠被去掉（`new` 里 trim）—— 否则会拼出 `//api/...`。
        let trimmed = JavdbProvider::new("javdb.com/").expect("构造");
        assert_eq!(
            trimmed.api_url("/x", &[]),
            "https://javdb.com/x",
            "host 末尾的斜杠要归一"
        );
    }

    #[test]
    fn the_signature_matches_the_upstream_algorithm() {
        // 期望值是**跨实现**得到的（另一实现按上游 `hashlib.md5(f"{ts}{SECRET}")
        // .hexdigest()` 算出），所以这一条能同时抓住「算法抄错」与「secret 抄错」
        // —— 用本仓自己的 md5 去验本仓自己的签名会漏掉后者。
        assert_eq!(
            signature_at(1_700_000_000),
            "1700000000.lpw6vgqzsp.dacaffcd8b4e1b35c2752f065e906f3a"
        );
        // 时间戳必须真的参与运算，而不是被丢掉。
        assert_ne!(signature_at(1_700_000_001), signature_at(1_700_000_000));
    }

    #[test]
    fn the_secret_is_the_upstream_literal() {
        // 长度 128（512 bit）。抄漏一段时先在这里红，而不是在生产里表现为
        // 「所有番号都查不到」。
        assert_eq!(SIGN_SECRET.len(), 128);
        assert!(SIGN_SECRET.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn movie_numbers_keep_their_colons_and_dashes() {
        // JavDB 按字面量匹配番号：编码成 %3A / %2D 会搜不到。
        assert_eq!(encode_component("ABC-123"), "ABC-123");
        assert_eq!(encode_component("A:B"), "A:B");
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn the_search_query_carries_the_upstream_parameters() {
        let provider = JavdbProvider::new("javdb.com").expect("构造");
        let query: Vec<(&str, String)> = vec![
            ("q", "ABC-123".to_owned()),
            ("from_recent", "false".to_owned()),
            ("type", "movie".to_owned()),
            ("movie_type", "all".to_owned()),
            ("movie_sort_by", "relevance".to_owned()),
            ("movie_filter_by", "all".to_owned()),
            ("page", "1".to_owned()),
        ];
        let url = provider.api_url(API_PATH_SEARCH, &query);
        for expected in [
            "q=ABC-123",
            "from_recent=false",
            "type=movie",
            "movie_type=all",
            "movie_sort_by=relevance",
            "movie_filter_by=all",
            "page=1",
        ] {
            assert!(url.contains(expected), "{url} 缺 {expected}");
        }
    }

    #[test]
    fn the_detail_path_substitutes_the_javdb_id() {
        assert_eq!(
            API_PATH_MOVIE_DETAIL.replace("{javdb_id}", "A123"),
            "/api/v4/movies/A123"
        );
    }

    #[test]
    fn an_empty_host_is_refused_rather_than_producing_a_broken_url() {
        let error = JavdbProvider::new("   ").expect_err("空 host 该拒");
        assert!(matches!(error, MetadataSourceError::RequestFailed(_)));
    }

    #[test]
    fn long_response_bodies_are_truncated_in_errors() {
        let long = "x".repeat(500);
        let short = truncate(&long);
        assert!(short.chars().count() <= 201, "不该把整个响应体塞进错误消息");
        assert!(short.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn an_account_counts_as_configured_only_when_both_fields_are_set() {
        // 上游 `if not (self.username and self.password)`（`:514`）—— 只看「非空」，
        // 空串与缺省等价。
        assert!(JavdbAccount {
            username: "u".to_owned(),
            password: "p".to_owned(),
        }
        .is_configured());
        assert!(!JavdbAccount::default().is_configured());
        assert!(!JavdbAccount {
            username: "u".to_owned(),
            password: String::new(),
        }
        .is_configured());
        assert!(!JavdbAccount {
            username: String::new(),
            password: "p".to_owned(),
        }
        .is_configured());
    }

    #[test]
    fn the_device_uuid_is_derived_per_account_and_stable() {
        // ★ 期望值来自**另一实现**（Python `uuid.uuid5(NAMESPACE, name)`），所以
        // 这一条同时抓「命名空间抄错」与「用了 uuid4 / 自己拼 sha1」—— 用自己的
        // 实现验自己的实现会漏掉后者。
        let account = JavdbAccount {
            username: "sakura".to_owned(),
            password: "x".to_owned(),
        };
        assert_eq!(
            account.device_uuid(),
            "01f1b765-45e0-55df-b2a9-253fb94cd452"
        );
        // 同账号每次必须一样：变了就等于每次登录都换一台设备。
        assert_eq!(account.device_uuid(), account.device_uuid());
        // 不同账号必须不同（上游 `:64-66`：「避免全网共用一个设备标识」）。
        let other = JavdbAccount {
            username: "other".to_owned(),
            password: "x".to_owned(),
        };
        assert_ne!(other.device_uuid(), account.device_uuid());
        // 账号名真的参与派生，而不是「所有账号同一个 uuid」。
        assert_eq!(
            JavdbAccount::default().device_uuid(),
            "734f0a92-5541-5a78-92b2-59649355e896"
        );
    }

    #[tokio::test]
    async fn unsupported_rank_arguments_are_refused_before_any_request() {
        // 域名是个**不存在的地址**：真发请求会是超时/连接错误，所以「立刻返回
        // Unsupported」本身就证明校验在发请求之前。
        let provider = JavdbProvider::new("javdb.invalid").expect("构造");
        let cases = [
            provider.rank_numbers("2", "daily").await,
            provider.rank_numbers("0", "yearly").await,
            provider.playback_rank_numbers("recent", "daily").await,
            provider.playback_rank_numbers("all", "yearly").await,
            provider
                .top_numbers(&JavdbAccount::default(), "decade", "", None)
                .await,
        ];
        for result in cases {
            let error = result.expect_err("不在白名单里的参数该被拒");
            assert!(matches!(error, JavdbRankError::Unsupported(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn top250_without_an_account_fails_instead_of_returning_an_empty_board() {
        let provider = JavdbProvider::new("javdb.invalid").expect("构造");
        let error = provider
            .top_numbers(&JavdbAccount::default(), "all", "", None)
            .await
            .expect_err("没配账号该拒");
        // ★ 不能返回空列表：那会让 TOP250 显示成「同步成功但一条都没有」。
        assert_eq!(error, JavdbRankError::AccountRequired);
    }

    #[test]
    fn a_rank_payload_yields_numbers_in_order_and_skips_blanks() {
        let payload = serde_json::json!({
            "success": 1,
            "data": { "movies": [
                { "number": "ABC-001" },
                { "number": "" },
                { "id": "no-number-field" },
                { "number": null },
                { "number": "ABC-002" },
            ]},
        });
        // 顺序即排名：不能排序、不能去重后重排。
        assert_eq!(
            rank_numbers_from(&payload).expect("解析"),
            vec!["ABC-001".to_owned(), "ABC-002".to_owned()]
        );
    }

    #[test]
    fn an_empty_rank_board_is_a_success_not_a_failure() {
        // TOP250 的历史年份、以及任何当日无数据的榜都会这样。判成失败会把它变成
        // 一个每天重试却永远好不了的错误。
        let payload = serde_json::json!({ "success": 1, "data": { "movies": [] } });
        assert_eq!(
            rank_numbers_from(&payload).expect("空榜单是成功"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn success_other_than_one_and_a_missing_movies_key_are_request_failures() {
        // HTTP 200 + `success: 0` 是 JavDB 的「业务失败」。不看它 = 把服务端拒绝
        // 当成「这个榜是空的」。
        let refused = serde_json::json!({ "success": 0, "message": "ParameterInvalid" });
        match rank_numbers_from(&refused).expect_err("success=0 该报错") {
            JavdbRankError::Request(detail) => {
                assert!(detail.contains("ParameterInvalid"), "{detail}");
            }
            other => panic!("应当是 Request，实际 {other:?}"),
        }
        let envelope_only = serde_json::json!({ "success": 1, "data": {} });
        assert!(
            matches!(
                rank_numbers_from(&envelope_only).expect_err("缺 data.movies 该报错"),
                JavdbRankError::Request(_)
            ),
            "缺 data.movies 是请求失败，不是空榜单"
        );
    }

    #[test]
    fn the_rank_urls_carry_the_upstream_parameter_names() {
        let provider = JavdbProvider::new("javdb.com").expect("构造");
        // ★ `video_type` 挂在查询参数 **`type`** 上（上游 `:810`），不是 `video_type`
        // —— 写错参数名会拿到一个 HTTP 200 的空榜。
        assert_eq!(
            provider.api_url(
                API_PATH_RANKINGS,
                &[("type", "0".to_owned()), ("period", "daily".to_owned())]
            ),
            "https://javdb.com/api/v1/rankings?type=0&period=daily"
        );
        assert_eq!(
            provider.api_url(
                API_PATH_RANKINGS_PLAYBACK,
                &[
                    ("filter_by", "high_score".to_owned()),
                    ("period", "weekly".to_owned())
                ]
            ),
            "https://javdb.com/api/v1/rankings/playback?filter_by=high_score&period=weekly"
        );
        // TOP250 的六个参数与顺序照抄上游 `:562-569`。
        assert_eq!(
            provider.api_url(
                API_PATH_MOVIES_TOP,
                &[
                    ("start_rank", "1".to_owned()),
                    ("type", "year".to_owned()),
                    ("type_value", "2024".to_owned()),
                    ("ignore_watched", "false".to_owned()),
                    ("page", "2".to_owned()),
                    ("limit", TOP250_PAGE_LIMIT.to_string()),
                ]
            ),
            "https://javdb.com/api/v1/movies/top?start_rank=1&type=year&type_value=2024\
             &ignore_watched=false&page=2&limit=50"
        );
    }

    #[test]
    fn form_encoding_and_query_encoding_are_different_rules() {
        // 表单体是 `quote_plus`：空格编成 `+`。
        assert_eq!(encode_form_component("a b"), "a+b");
        // ★ 与查询串**相反**：那边 `:` 是安全字符（番号与带冒号的值不能被编码），
        // 这边必须编码成 `%3A`。合并这两个函数必然在某一处编错。
        assert_eq!(encode_form_component("a:b"), "a%3Ab");
        assert_eq!(encode_component("a:b"), "a:b");
        assert_eq!(
            encode_form(&[
                ("username".to_owned(), "sa kura".to_owned()),
                (
                    "device_uuid".to_owned(),
                    "01f1b765-45e0-55df-b2a9-253fb94cd452".to_owned()
                ),
            ]),
            "username=sa+kura&device_uuid=01f1b765-45e0-55df-b2a9-253fb94cd452"
        );
    }
}
