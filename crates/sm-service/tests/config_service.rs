//! `ConfigService` 集成测试：真实文件，真实读写。
//!
//! 覆盖上游 `config_service.update_config` 的每一条规则，以及三条只有在
//! **真的碰磁盘**时才暴露的语义：
//!
//! 1. 连续两次局部 PATCH 互不覆盖（每次从磁盘快照起算）；
//! 2. 原子写盘后重读拿到的是完整配置，不是半个 TOML；
//! 3. 缺文件不是错误（首次启动的路径）。

use serde_json::{json, Value};
use sm_service::system::config::{
    ConfigService, EMPTY_CONFIG_UPDATE, INVALID_CONFIG_VALUE, READONLY_CONFIG_KEY,
    UNKNOWN_CONFIG_FIELD,
};

/// 每个用例一个独立目录，避免并发跑时互相看到对方的文件。
fn temp_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sm-config-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn service(tag: &str) -> (ConfigService, std::path::PathBuf) {
    let path = temp_dir(tag).join("config.toml");
    (ConfigService::new(&path), path)
}

fn patch(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().expect("patch 必须是对象")
}

#[test]
fn a_missing_file_is_not_an_error() {
    // 首次启动：上游 `SETTINGS_TOML_PATH` 指向的文件不存在，
    // `load_persisted_settings` 走纯默认值。
    let (svc, _path) = service("missing");
    let values = svc.get().expect("缺文件应回落默认值");
    assert_eq!(values["qdrant"]["url"], json!("http://qdrant:6333"));
    assert_eq!(
        values["scheduler"]["media_thumbnail_cron"],
        json!("*/30 * * * *")
    );
}

#[test]
fn readonly_keys_are_rejected_by_name() {
    let (svc, _path) = service("readonly");
    for key in ["auth", "enable_docs", "plugins"] {
        let err = svc
            .update(&patch(json!({ key: {} })))
            .expect_err("只读键必须被拒");
        assert_eq!(err.status, 422, "{key} 应是 422");
        assert_eq!(err.code(), READONLY_CONFIG_KEY, "{key} 的错误码");
        assert_eq!(
            err.api.details.as_ref().and_then(|d| d.get("field")),
            Some(&json!(key)),
            "{key} 的 details.field 应回显键名"
        );
    }
}

#[test]
fn unknown_top_level_and_nested_keys_differ_in_details() {
    let (svc, _path) = service("unknown");

    let err = svc
        .update(&patch(json!({"scheduer": {"enabled": true}})))
        .expect_err("拼错的顶层键");
    assert_eq!(err.code(), UNKNOWN_CONFIG_FIELD);
    assert_eq!(
        err.api.details.as_ref().and_then(|d| d.get("field")),
        Some(&json!("scheduer")),
        "顶层拼错只回显顶层键名"
    );

    let err = svc
        .update(&patch(json!({"scheduler": {"enabld": true}})))
        .expect_err("拼错的子键");
    assert_eq!(err.code(), UNKNOWN_CONFIG_FIELD);
    assert_eq!(
        err.api.details.as_ref().and_then(|d| d.get("field")),
        Some(&json!("scheduler.enabld")),
        "子键要回显点分路径 —— 客户端据此高亮对应控件"
    );
}

#[test]
fn a_section_given_a_non_object_is_a_value_error() {
    let (svc, _path) = service("notobject");
    let err = svc
        .update(&patch(json!({"scheduler": "yes"})))
        .expect_err("子节收到标量");
    assert_eq!(
        err.code(),
        INVALID_CONFIG_VALUE,
        "「形状不对」与「键不认识」是两个码，不能混"
    );
}

#[test]
fn an_empty_patch_is_rejected() {
    let (svc, _path) = service("empty");
    let err = svc.update(&patch(json!({}))).expect_err("空 patch");
    assert_eq!(err.code(), EMPTY_CONFIG_UPDATE);
}

#[test]
fn enabling_image_search_without_qdrant_is_rejected() {
    let (svc, _path) = service("qdrant");
    let err = svc
        .update(&patch(json!({"image_search": {"enabled": true}})))
        .expect_err("开了搜图没开 Qdrant");
    assert_eq!(err.code(), INVALID_CONFIG_VALUE);
    assert!(
        err.api.message.contains("Qdrant"),
        "message 要指明缺的是什么：{}",
        err.api.message
    );
}

#[test]
fn image_search_is_allowed_once_qdrant_is_on_in_the_same_patch() {
    // 跨节不变式校验的是**合并后**的快照，所以同一个 PATCH 里两个都开是合法的。
    let (svc, _path) = service("both");
    let values = svc
        .update(&patch(json!({
            "image_search": {"enabled": true},
            "qdrant": {"enabled": true},
        })))
        .expect("两个一起开应当通过");
    assert_eq!(values["image_search"]["enabled"], json!(true));
    assert_eq!(values["qdrant"]["enabled"], json!(true));
}

#[test]
fn each_upstream_validator_produces_invalid_config_value() {
    let (svc, _path) = service("validators");
    let cases: [(&str, Value); 4] = [
        (
            "cron",
            json!({"scheduler": {"movie_heat_cron": "不是 cron"}}),
        ),
        (
            "范围",
            json!({"scheduler": {"worker_default_concurrency": 99}}),
        ),
        ("URL", json!({"qdrant": {"url": "not-a-url"}})),
        ("类型", json!({"logging": {"level": 123}})),
    ];
    for (tag, body) in cases {
        let err = svc
            .update(&patch(body.clone()))
            .expect_err(&format!("{tag}: {body} 应当被拒，实际通过了"));
        assert_eq!(err.status, 422, "{tag} 应是 422");
        assert_eq!(err.code(), INVALID_CONFIG_VALUE, "{tag} 的错误码");
        assert_eq!(
            err.api.message, "Configuration validation failed",
            "{tag} 的 message 要与上游一致"
        );
        assert!(
            err.api
                .details
                .as_ref()
                .and_then(|d| d.get("errors"))
                .is_some(),
            "{tag} 的 details 要带 errors 数组"
        );
    }
}

#[test]
fn the_plugin_id_validators_are_unreachable_through_this_api() {
    // 上游 `Plugins` 的两个校验器（`enabled` 无重复、命名空间是合法插件 ID）
    // 在配置 API 上**不可达** —— `plugins` 是只读键，请求在白名单那一步就被
    // `readonly_config_key` 挡掉了。它们只在**启动加载手工改过的 TOML**时有用。
    //
    // 这条断言把这个事实钉住：哪天有人把 `plugins` 从 READONLY_KEYS 里挪走，
    // 重复插件 ID 就能从 API 写进去了，而那是「启用两份同一插件」。
    let (svc, _path) = service("plugins-unreachable");
    let err = svc
        .update(&patch(json!({"plugins": {"enabled": ["a", "a"]}})))
        .expect_err("plugins 是只读键");
    assert_eq!(
        err.code(),
        READONLY_CONFIG_KEY,
        "重复插件 ID 的校验器不该在这条路径上生效"
    );
}

#[test]
fn consecutive_patches_do_not_overwrite_each_other() {
    // 这是「每次从磁盘快照起算」的全部意义：上游注释直接写了
    // 「连续局部 PATCH 不会回写旧进程快照覆盖前次结果」。
    let (svc, _path) = service("consecutive");
    svc.update(&patch(json!({"scheduler": {"log_dir": "/data/logs-a"}})))
        .expect("第一次");
    svc.update(&patch(json!({"scheduler": {"enabled": false}})))
        .expect("第二次");

    let values = svc.get().expect("读回");
    assert_eq!(
        values["scheduler"]["log_dir"],
        json!("/data/logs-a"),
        "第二次 PATCH 只该动 enabled，不能把 log_dir 退回默认值"
    );
    assert_eq!(values["scheduler"]["enabled"], json!(false));
}

#[test]
fn a_patch_persists_to_disk_and_survives_a_fresh_service() {
    let (svc, path) = service("persist");
    svc.update(&patch(json!({"logging": {"level": "DEBUG"}})))
        .expect("写盘");

    // 磁盘上真的有，且是可解析的完整 TOML —— 不是「半个文件」。
    let text = std::fs::read_to_string(&path).expect("配置文件存在");
    assert!(text.contains("DEBUG"), "写出的 TOML 应含新值：{text}");
    let reparsed: Value = toml::from_str(&text).expect("写出的 TOML 必须能重新解析");
    assert_eq!(reparsed["logging"]["level"], json!("DEBUG"));

    // 换个服务实例（= 模拟重启）读回，值仍在。
    let fresh = ConfigService::new(&path);
    assert_eq!(
        fresh.get().expect("重启后读回")["logging"]["level"],
        json!("DEBUG")
    );
}

#[test]
fn the_temporary_file_does_not_survive_a_successful_write() {
    let (svc, path) = service("tmp");
    svc.update(&patch(json!({"logging": {"level": "WARN"}})))
        .expect("写盘");
    let leftovers: Vec<String> = std::fs::read_dir(path.parent().expect("目录"))
        .expect("列目录")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "config.toml")
        .collect();
    assert!(
        leftovers.is_empty(),
        "原子写盘后不该有残留文件（运维会以为配置有两份）：{leftovers:?}"
    );
}

#[test]
fn a_null_valued_field_is_omitted_from_the_file() {
    // TOML 没有 null。上游靠 Python toml 编码器静默跳过 None，磁盘上
    // 「键不存在」就是 null；读回时由默认值补上。Rust 侧显式剥掉。
    let (svc, path) = service("nulls");
    svc.update(&patch(
        json!({"image_search": {"inference_api_key": "sk-123"}}),
    ))
    .expect("先写一个非空值");
    assert!(std::fs::read_to_string(&path)
        .expect("读")
        .contains("sk-123"));

    svc.update(&patch(json!({"image_search": {"inference_api_key": null}})))
        .expect("再写回 null");
    let text = std::fs::read_to_string(&path).expect("读");
    assert!(
        !text.contains("inference_api_key"),
        "null 不该出现在 TOML 里：{text}"
    );
    // 读回时回落默认值（仍是 null）—— 往返无损。
    let values = svc.get().expect("读回");
    assert!(
        values["image_search"]["inference_api_key"].is_null(),
        "往返之后应仍是 null"
    );
}

#[test]
fn get_strips_the_readonly_keys() {
    let (svc, _path) = service("strip");
    let values = svc.get().expect("读");
    let object = values.as_object().expect("对象");
    for key in ["auth", "enable_docs", "plugins"] {
        assert!(
            !object.contains_key(key),
            "{key} 不该出现在 GET 响应里 —— auth 里是 secret_key 与 file_signature_secret"
        );
    }
    assert!(object.contains_key("scheduler"));
    assert!(object.contains_key("database"));
}

#[test]
fn extra_writable_keys_stay_patchable() {
    // 上游把 `updates` / `existing_config` 也声明成了 `Settings` 字段，所以它们
    // 就在白名单里。收紧会让原本 200 的请求变 422 —— 那是契约变更，不是修复。
    let (svc, _path) = service("extra");
    let values = svc
        .update(&patch(json!({"updates": {"checked_at": "2026-10-04"}})))
        .expect("updates 可 PATCH");
    assert_eq!(values["updates"]["checked_at"], json!("2026-10-04"));
}

#[test]
fn a_rejected_patch_leaves_the_file_untouched() {
    // 校验在写盘**之前**，所以被拒的 PATCH 不能留下任何痕迹。
    let (svc, path) = service("untouched");
    svc.update(&patch(json!({"logging": {"level": "INFO"}})))
        .expect("先写一个合法值");
    let before = std::fs::read_to_string(&path).expect("读");

    let err = svc
        .update(&patch(
            json!({"logging": {"level": "DEBUG"}, "qdrant": {"url": "坏"}}),
        ))
        .expect_err("含非法值的 patch 应被拒");
    assert_eq!(err.code(), INVALID_CONFIG_VALUE);

    assert_eq!(
        std::fs::read_to_string(&path).expect("读"),
        before,
        "同一个 patch 里合法的那半边也不该落盘 —— 整个 patch 是原子的"
    );
}
