//! 可选能力的开关，对应上游 `src/service/system/optional_services.py`（38 行）。
//!
//! # 这个文件解决的是「为什么这个功能/任务没跑」
//!
//! 两个能力依赖**外部向量服务**，没有它们时接口与任务都必须给出可读的
//! 理由，而不是 404 或静默跳过：
//!
//! | 能力 | 依赖 | 配置 |
//! |---|---|---|
//! | 相似影片 | Qdrant | `qdrant.enabled` |
//! | 图搜 | Qdrant **且** 推理服务 | `qdrant.enabled && image_search.enabled` |
//!
//! `job_disabled_reason` 是 worker 的必需件：任务被跳过时，任务中心要显示
//! 「图片与文字搜图未启用」而不是一个空原因。缺了它，队列里会出现
//! **没有任何解释**的 `skipped` 行。
//!
//! # 只读启动配置，**不做网络探测**
//!
//! 上游的模块 docstring 写明「只读启动配置，不做网络探测」—— 即便 Qdrant
//! 配好了但实际连不上，这里也返回「已启用」。理由：探测会把每次调用变成
//! 一个网络往返，而 `/config` 已经把这些值暴露给前端了；真要区分
//! 「配置开了」与「服务在线」，那是健康检查的职责，不是能力开关的。
//!
//! 跟着上游保持这个边界。擅自加探测会让同一个接口在网络抖动时给出不同的
//! 答案，而客户端无从判断是配置变了还是服务挂了。
//!
//! # `image_search` 依赖 `qdrant` 这条跨节约束已在校验层
//!
//! [`sm_core::config_schema::image_search_requires_qdrant`] 拒绝
//! 「开了图搜但没开 Qdrant」的组合，所以本模块的 `&&` 在正常路径上是冗余的。
//! 保留它有两个理由：一是**存量配置**可能已经有这种组合（老版本没这条校验），
//! 二是读到的值可能来自绕过校验的路径（手改 toml）。宁可多判一次。

use crate::system::config::ConfigService;

/// 错误码。上游三处 `require_*` 都用同一个 `feature_disabled`。
pub const FEATURE_DISABLED: &str = "feature_disabled";

/// 相似影片能力是否启用。
pub fn movie_similarity_enabled(values: &serde_json::Value) -> bool {
    enabled_flag(values, "qdrant")
}

/// 图搜能力是否启用 —— **两个开关都要开**。
pub fn image_search_enabled(values: &serde_json::Value) -> bool {
    movie_similarity_enabled(values) && enabled_flag(values, "image_search")
}

/// 能力清单。对应上游 `capabilities()`。
///
/// 键名是**客户端契约**（前端按它们决定显示哪些入口），照抄上游。
pub fn capabilities(values: &serde_json::Value) -> Capabilities {
    Capabilities {
        movie_similarity: movie_similarity_enabled(values),
        image_search: image_search_enabled(values),
    }
}

/// 图搜未启用时返回 409。与上游 `require_image_search` 逐字一致。
pub fn require_image_search(values: &serde_json::Value) -> Result<(), FeatureDisabled> {
    if image_search_enabled(values) {
        return Ok(());
    }
    Err(FeatureDisabled {
        message: "当前服务器未启用图片与文字搜图".to_owned(),
    })
}

/// 某任务因能力未启用而被禁用时的理由。`None` = 该任务不受能力开关约束。
///
/// 对应上游 `job_disabled_reason`。这是 worker 解释「为什么没跑」的唯一来源。
///
/// # 只对两个任务生效，其余一律 `None`
///
/// 上游就是两个 `if`，没有默认分支。写成「未知 task_key 也返回理由」会让
/// 一个拼错的 key 拿到一句无关的解释。
pub fn job_disabled_reason(task_key: &str, values: &serde_json::Value) -> Option<String> {
    match task_key {
        "image_search_index" if !image_search_enabled(values) => {
            Some("图片与文字搜图未启用".to_owned())
        }
        "movie_similarity_recompute" if !movie_similarity_enabled(values) => {
            Some("相似影片与向量服务未启用".to_owned())
        }
        _ => None,
    }
}

/// 任务被能力开关禁用时返回 409。
///
/// 注意**状态码是 409 而不是 422**：这不是「请求写错了」，而是「服务器
/// 当前配置不提供这个能力」—— 客户端该提示用户去开配置，而不是改请求。
pub fn require_job_enabled(
    task_key: &str,
    values: &serde_json::Value,
) -> Result<(), FeatureDisabled> {
    match job_disabled_reason(task_key, values) {
        Some(message) => Err(FeatureDisabled { message }),
        None => Ok(()),
    }
}

/// 能力开关的诊断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub movie_similarity: bool,
    pub image_search: bool,
}

/// 「能力未启用」。对应上游的 `ApiError(409, "feature_disabled", ...)`。
///
/// 单独一个类型而不是直接返回 [`crate::error::ServiceError`]：这个错误**只**由
/// 能力开关产生，而它的状态码（409）有一个专门的语义 —— 「换个配置就能用」。
/// 让 service 层自己构造 `ServiceError::conflict` 的话，调用方就分不清
/// 「能力没开」与「名称冲突」了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureDisabled {
    pub message: String,
}

impl std::fmt::Display for FeatureDisabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FeatureDisabled {}

impl From<FeatureDisabled> for crate::error::ServiceError {
    /// 409 `feature_disabled` —— 与上游一致。
    fn from(value: FeatureDisabled) -> Self {
        Self {
            status: 409,
            api: Box::new(sm_core::ApiError::new(FEATURE_DISABLED, value.message)),
        }
    }
}

/// 读某个节的 `enabled`。缺节或类型不对一律 `false`。
///
/// 缺节 → `false` 而不是「按默认值」：默认就是 `false`，而配置文件损坏
/// 导致的「读不出 enabled」也应当按「没开」处理 —— 让功能**意外启用**的
/// 后果远大于意外停用。
fn enabled_flag(values: &serde_json::Value, section: &str) -> bool {
    values
        .get(section)
        .and_then(serde_json::Value::as_object)
        .and_then(|s| s.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 便捷入口：从配置服务读快照再判断。
///
/// 读盘一次，三个能力判定共用 —— 三个开关来自同一个快照，逐个读会读到
/// 三个**不同时刻**的配置，而 `PATCH /config` 可以在中间生效。
pub fn capabilities_of(config: &ConfigService) -> Result<Capabilities, crate::error::ServiceError> {
    Ok(capabilities(&config.snapshot()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sm_core::config_schema;

    /// 造一份只含两个相关节的配置片段。
    fn values(qdrant: bool, image_search: bool) -> serde_json::Value {
        json!({
            "qdrant": { "enabled": qdrant, "url": "http://qdrant:6333" },
            "image_search": { "enabled": image_search },
        })
    }

    #[test]
    fn both_capabilities_default_to_off() {
        // schema 的默认值就是 false。默认部署不该声称自己能做图搜。
        let defaults = config_schema::defaults_json();
        assert!(!movie_similarity_enabled(&defaults));
        assert!(!image_search_enabled(&defaults));
    }

    #[test]
    fn image_search_needs_both_switches() {
        // 四种组合逐条断言 —— `&&` 的四种真值表
        assert!(!image_search_enabled(&values(false, false)));
        assert!(
            !image_search_enabled(&values(false, true)),
            "开了图搜但没开 Qdrant 时图搜**不可用** —— 依赖的是向量库"
        );
        assert!(!image_search_enabled(&values(true, false)));
        assert!(image_search_enabled(&values(true, true)));
    }

    #[test]
    fn movie_similarity_only_needs_qdrant() {
        assert!(movie_similarity_enabled(&values(true, false)));
        assert!(!movie_similarity_enabled(&values(false, true)));
    }

    #[test]
    fn a_missing_or_malformed_section_reads_as_off() {
        // 宁可「意外停用」也不要「意外启用」—— 后者会走到不存在的服务
        for bad in [
            json!({}),
            json!({"qdrant": {}}),
            json!({"qdrant": {"enabled": "true"}}),
            json!({"qdrant": {"enabled": null}}),
            json!({"qdrant": "yes"}),
        ] {
            assert!(!movie_similarity_enabled(&bad), "{bad} 应读作未启用");
        }
    }

    #[test]
    fn capabilities_exposes_both_keys() {
        let caps = capabilities(&values(true, true));
        assert!(caps.movie_similarity);
        assert!(caps.image_search);
    }

    #[test]
    fn require_image_search_is_409_with_the_upstream_message() {
        let err = require_image_search(&values(false, true)).expect_err("应被拒");
        assert_eq!(err.message, "当前服务器未启用图片与文字搜图");
        let service: crate::error::ServiceError = err.into();
        assert_eq!(
            (service.status, service.code()),
            (409, "feature_disabled"),
            "状态码与错误码是一个整体：409 表示「换个配置就能用」"
        );
    }

    #[test]
    fn require_image_search_passes_when_both_are_on() {
        assert!(require_image_search(&values(true, true)).is_ok());
    }

    #[test]
    fn the_two_gated_jobs_report_upstream_messages() {
        let off = values(false, false);
        assert_eq!(
            job_disabled_reason("image_search_index", &off).as_deref(),
            Some("图片与文字搜图未启用")
        );
        assert_eq!(
            job_disabled_reason("movie_similarity_recompute", &off).as_deref(),
            Some("相似影片与向量服务未启用")
        );
    }

    /// 开了 Qdrant 没开图搜时，相似影片该可用而图搜不该 —— 两个开关独立。
    #[test]
    fn the_two_gated_jobs_are_independent() {
        let only_qdrant = values(true, false);
        assert_eq!(
            job_disabled_reason("movie_similarity_recompute", &only_qdrant),
            None,
            "Qdrant 开着，相似影片就可用"
        );
        assert!(job_disabled_reason("image_search_index", &only_qdrant).is_some());
    }

    #[test]
    fn both_gated_jobs_pass_when_everything_is_on() {
        let all = values(true, true);
        for key in ["image_search_index", "movie_similarity_recompute"] {
            assert_eq!(job_disabled_reason(key, &all), None, "{key} 不该被禁用");
            assert!(require_job_enabled(key, &all).is_ok());
        }
    }

    /// 其它任务**一律不受**这两个开关约束。
    ///
    /// 上游只有两个 `if`、没有默认分支。给未知 key 返回理由会让一个拼错的
    /// `task_key` 拿到一句无关的解释，而真正的原因（key 拼错）被掩盖。
    #[test]
    fn an_unrelated_job_is_never_gated() {
        let off = values(false, false);
        for key in [
            "media_thumbnail",
            "movie_heat",
            "activity_cleanup",
            "image_search_index_v2", // 前缀相同但不是那个任务
            "",
        ] {
            assert_eq!(
                job_disabled_reason(key, &off),
                None,
                "{key:?} 不该被这两个开关拦住"
            );
        }
    }

    /// `job_disabled_reason` 的两个分支在开关**开启**时都必须返回 `None`。
    #[test]
    fn an_enabled_job_has_no_reason_to_show() {
        let all = values(true, true);
        assert_eq!(job_disabled_reason("image_search_index", &all), None);
        assert_eq!(
            job_disabled_reason("movie_similarity_recompute", &all),
            None
        );
    }

    /// 与 schema 层的跨节约束一致：本模块的 `&&` 不是孤立的。
    #[test]
    fn the_and_is_backed_by_the_schema_level_assertion() {
        // 「开了图搜没开 Qdrant」在 `PATCH /config` 阶段就该被拒
        let mut bad = config_schema::defaults_json();
        bad["image_search"]["enabled"] = json!(true);
        bad["qdrant"]["enabled"] = json!(false);
        let as_map = bad.as_object().expect("快照是对象");
        assert!(config_schema::image_search_requires_qdrant(as_map));
        // 而本模块对同一份配置也读作「图搜不可用」—— 存量配置走到这里
        assert!(!image_search_enabled(&bad));
    }
}
