//! 「配置文件坏了不许静默降级」的回归测试。
//!
//! # 这组测试在防什么
//!
//! 曾经的 `clip_collections_http.rs` 有 6 个用例长期红着，症状是
//! `clip_count` 恒为 0。真因**不在测试里**，而在 `sm_api::config` 之前那版
//! 「每处各写一份 `snapshot().unwrap_or_default()`」：
//!
//! ```text
//! 配置文件非法（一个手误的转义反斜杠）
//!   -> Err 被 unwrap_or_default 吞成全默认配置
//!   -> media_clip_root_path 变空串
//!   -> clip_root 退化成 PathBuf::from(".")（进程工作目录）
//!   -> has_valid_artifact 恒为 false -> clip_count 恒为 0，且不报任何错
//! ```
//!
//! **这类失败在 Linux CI 上完全看不见**（POSIX 路径里没有反斜杠，手写
//! TOML 恰好合法）。所以只靠「现有测试绿」防不住 —— 必须主动构造坏配置。

use serde_json::Value;
use sm_service::system::ConfigService;

fn temp_config(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sm-cfg-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir.join("config.toml")
}

fn cleanup(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 文件不存在**不是错误** —— 那是首次启动的正常路径。
#[test]
fn a_missing_config_file_yields_defaults() {
    let service = ConfigService::new("绝对不存在的路径/sm-config-missing.toml".to_owned());
    let snapshot = service.snapshot().expect("文件不存在应返回默认值");
    assert!(snapshot.get("media").is_some(), "默认值里应含 media 节");
    service.validate().expect("默认值本身应通过校验");
}

/// 「配置合法但没这个键」与「配置损坏」是**两件事**。
///
/// 旧代码用同一个 `unwrap_or_default()` 处理两者 —— **这才是 bug 的根源**。
#[test]
fn a_missing_key_is_not_the_same_as_a_broken_file() {
    let valid = temp_config("no-key");
    std::fs::write(&valid, "[auth]\nfile_signature_secret = \"s3cret\"\n").expect("写配置");
    ConfigService::new(valid.clone())
        .snapshot()
        .expect("配置合法就该读得出来，即使少了可选键");
    cleanup(&valid);

    let broken = temp_config("broken");
    std::fs::write(&broken, "[media\nbroken = \n").expect("写配置");
    assert!(
        ConfigService::new(broken.clone()).snapshot().is_err(),
        "配置损坏必须报错，不能当成「没写这个键」"
    );
    cleanup(&broken);
}

/// 非法 TOML 必须报错，**不能**退回默认值 —— 整个改动的核心断言。
#[test]
fn an_unparseable_config_is_an_error_not_a_silent_reset_to_defaults() {
    let path = temp_config("unparseable");
    std::fs::write(&path, "[media\nclip_root = \"/data/clips\"\n").expect("写配置");

    let error = ConfigService::new(path.clone())
        .snapshot()
        .expect_err("非法 TOML 必须报错，不能退回默认值");

    assert_eq!(
        error.status, 500,
        "配置坏了是 500 —— 重试不会变好，所以不是 502/503"
    );
    assert_eq!(error.code(), "config_invalid");
    let details = error
        .details()
        .expect("必须带 details，否则不知道是哪个文件");
    assert!(
        details.get("config_path").is_some(),
        "details 必须指出是哪个配置文件"
    );
    assert!(
        details.get("cause").is_some(),
        "details 必须带底层原因（TOML 报错原文）"
    );
    cleanup(&path);
}

/// 路径要用 TOML **字面量字符串**（单引号）。
///
/// 基本字符串（双引号）会处理转义，而 `\U` 要求跟 8 位十六进制，所以
/// `"C:\Users\..."` 解析必失败。而 `Path::canonicalize` 在 Windows 上返回
/// 扩展长度前缀 `\\?\C:\...`。**这正是那 6 个测试最初的触发原因。**
#[test]
fn a_toml_literal_string_path_round_trips() {
    let path = temp_config("literal");
    let expected = path
        .parent()
        .expect("有父目录")
        .join("clips")
        .display()
        .to_string();
    std::fs::write(
        &path,
        format!("[media]\nmedia_clip_root_path = '{expected}'\n"),
    )
    .expect("写配置");

    let snapshot = ConfigService::new(path.clone())
        .snapshot()
        .expect("字面量字符串写法应能解析");
    assert_eq!(
        snapshot
            .get("media")
            .and_then(|media| media.get("media_clip_root_path"))
            .and_then(Value::as_str),
        Some(expected.as_str()),
        "路径应被原样读回"
    );
    cleanup(&path);
}

/// 把 `canonicalize()` 的结果拼进**双引号** TOML —— 旧 Fixture 的写法。
///
/// 无论哪个平台，**都不许静默降级**：Windows 上解析失败（必须报
/// `config_invalid`），POSIX 上恰好合法（必须原样读回）。
#[test]
fn a_canonicalized_path_in_a_basic_string_never_silently_degrades() {
    let path = temp_config("canon-basic");
    let clips = path.parent().expect("有父目录").join("clips");
    std::fs::create_dir_all(&clips).expect("建 clips 目录");
    let canonical = clips.canonicalize().expect("规范化");
    std::fs::write(
        &path,
        format!(
            "[media]\nmedia_clip_root_path = \"{}\"\n",
            canonical.display()
        ),
    )
    .expect("写配置");

    match ConfigService::new(path.clone()).snapshot() {
        Ok(snapshot) => {
            // POSIX 分支：路径里没有反斜杠，双引号写法合法，必须原样读回。
            assert_eq!(
                snapshot
                    .get("media")
                    .and_then(|media| media.get("media_clip_root_path"))
                    .and_then(Value::as_str),
                Some(canonical.display().to_string().as_str()),
                "POSIX 下应原样读回"
            );
        }
        Err(error) => {
            // Windows 分支：解析失败，但**必须报错**而不是退回默认值。
            assert_eq!(error.code(), "config_invalid");
            assert!(
                error.details().is_some(),
                "失败时必须带 details，不能只回一句「解析失败」"
            );
        }
    }
    cleanup(&path);
}

/// `validate()` 比 `snapshot()` 更严：还要查**字段取值**。
///
/// 「能解析但 URL 不合法」这种，只查解析的话会溜过去，然后在第一次连向量库
/// 时才炸 —— 那时排查方向已经被带偏了。
#[test]
fn validate_rejects_a_parsable_but_illegal_value() {
    let path = temp_config("illegal-value");
    std::fs::write(&path, "[qdrant]\nurl = \"localhost:6334\"\n").expect("写配置");

    let error = ConfigService::new(path.clone())
        .validate()
        .expect_err("字段取值不合法必须被启动期校验拦下");
    assert_eq!(error.code(), "config_invalid");
    let details = error.details().expect("必须带 details");
    let fields = details
        .get("fields")
        .and_then(Value::as_array)
        .expect("必须逐条列出不合法字段");
    assert!(
        fields.iter().any(|field| {
            field
                .get("field")
                .and_then(Value::as_str)
                .is_some_and(|loc| loc.starts_with("qdrant"))
        }),
        "应指出是 qdrant 段的字段：{details:?}"
    );
    cleanup(&path);
}

/// 合法配置必须能过 `validate()` —— 否则上面那条只是「一律拒绝」。
#[test]
fn validate_accepts_a_legal_config() {
    let path = temp_config("legal");
    let expected = path
        .parent()
        .expect("有父目录")
        .join("clips")
        .display()
        .to_string();
    std::fs::write(
        &path,
        format!(
            "[auth]\nfile_signature_secret = \"s3cret\"\n\n\
             [qdrant]\nenabled = false\n\n\
             [media]\nmedia_clip_root_path = '{expected}'\n"
        ),
    )
    .expect("写配置");

    ConfigService::new(path.clone())
        .validate()
        .expect("合法配置应通过启动期校验");
    cleanup(&path);
}
