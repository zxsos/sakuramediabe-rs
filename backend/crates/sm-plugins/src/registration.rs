//! 插件注册结果的校验。
//!
//! # 上游对应
//!
//! `proto/plugin.proto` 的 `RegisterRequest` / `RegisterResponse`：
//!
//! ```text
//! RegisterRequest  { plugin_id, abi_major }
//! RegisterResponse { plugin_id, display_name, version, abi_major,
//!                    capabilities, extensions, jobs, settings_schema,
//!                    data_plane_endpoint }
//! ```
//!
//! 协议里的三条硬约束（都能在 proto 注释里找到出处）：
//!
//! 1. `plugin_id` 由宿主注入、插件**原样回显**，不一致就是加载失败；
//! 2. `plugin_id` 还必须与 manifest 声明一致；
//! 3. `abi_major` 是 ABI 主版本，**不兼容变更时递增，宿主据此拒绝加载**。
//!
//! # 为什么这里只收原始值而不收生成的消息
//!
//! 生成代码（`sm_plugin_api::v1`）的类型名与字段随 proto 变动。校验规则本身
//! 是**稳定的**，所以让它只依赖 `&str` / `i32` / `&[i32]`：宿主把
//! `RegisterResponse` 的字段喂进来即可。这样改 proto 不会动到规则与测试。
//!
//! # 能力声明的校验口径
//!
//! `Capability` 是注册时显式声明的（gRPC 没有「方法存在性」，Python 侧原本靠
//! `supports_*()` 的 getattr 探测）。这里只校验**声明本身**合法：
//!
//! - 不能为 `CAPABILITY_UNSPECIFIED`（0）—— 那是「没填」；
//! - 必须是已知取值；
//! - 不能重复。
//!
//! 「声明了但没实现对应 rpc」要在**第一次调用**才会暴露，无法在注册期判定 ——
//! 那是调用层的事，不属于本模块。

/// ABI 主版本。
///
/// **复用 `sm-plugin-api` 的那一份** —— 契约（`proto/`）与宿主若各持一个常量，
/// 改 proto 时漏改一处就会出现「插件按 1 编译、宿主按 2 校验」的静默不兼容。
pub use sm_plugin_api::ABI_MAJOR;

/// 已知的能力取值（`proto/plugin.proto` 的 `enum Capability`）。
///
/// 与 proto 的数值**逐条对齐**；改 proto 时必须同步这里，否则新能力会被判成
/// 「未知」而拒绝加载。
pub mod capability {
    pub const PROBE_DURATION: i32 = 1;
    pub const PROBE_RESOLUTION: i32 = 2;
    pub const PROBE_VIDEO_INFO: i32 = 3;
    pub const OPEN_COVER_SOURCE: i32 = 4;
    pub const IMPORT_SOURCE_IDENTITY: i32 = 5;
    pub const SCAN_MEDIA_REFS: i32 = 6;
    pub const SCAN_MANAGED_MEDIA_REF_KEYS: i32 = 7;
    pub const MANAGED_MEDIA_REF_KEY: i32 = 8;
    pub const SPACE_USAGE: i32 = 9;
    pub const MERGED_PLAYBACK: i32 = 20;
    pub const MERGED_PLAYBACK_PREFLIGHT: i32 = 21;
    pub const TRANSFER_SOURCE: i32 = 30;
    pub const TRANSFER_SOURCE_CLEANUP: i32 = 31;
    pub const TRANSFER_TARGET: i32 = 32;
    pub const EXTENSION_CATALOG_METADATA_SOURCE: i32 = 40;
    pub const EXTENSION_RANKING_SOURCE: i32 = 41;
    pub const DOWNLOAD: i32 = 50;
}

/// 注册失败的原因。**可一次报多条** —— 加载插件时把所有问题列出来，比一条条
/// 试错有用得多（插件作者一次就能改完）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationProblem {
    /// 插件回显的 `plugin_id` 与宿主注入的不同。
    PluginIdNotEchoed { expected: String, got: String },
    /// 回显的 `plugin_id` 与 manifest 声明的不同。
    PluginIdDiffersFromManifest { manifest: String, got: String },
    /// ABI 主版本不匹配。
    AbiMismatch { host: i32, plugin: i32 },
    /// 声明了 `CAPABILITY_UNSPECIFIED`。
    UnspecifiedCapability,
    /// 声明了未知取值（通常是宿主比插件旧）。
    UnknownCapability(i32),
    /// 同一能力重复声明。
    DuplicateCapability(i32),
}

impl RegistrationProblem {
    /// 给日志与错误详情用的稳定标识。
    pub fn code(&self) -> &'static str {
        match self {
            Self::PluginIdNotEchoed { .. } => "plugin_id_not_echoed",
            Self::PluginIdDiffersFromManifest { .. } => "plugin_id_differs_from_manifest",
            Self::AbiMismatch { .. } => "abi_mismatch",
            Self::UnspecifiedCapability => "unspecified_capability",
            Self::UnknownCapability(_) => "unknown_capability",
            Self::DuplicateCapability(_) => "duplicate_capability",
        }
    }
}

/// 校验一次注册的结果。
///
/// 参数顺序与 `RegisterResponse` 的字段对应；`expected_id` 是宿主注入的
/// `plugin_id`，`manifest_id` 是 manifest 里声明的。
pub fn validate_registration(
    expected_id: &str,
    manifest_id: &str,
    got_id: &str,
    got_abi: i32,
    capabilities: &[i32],
) -> Result<(), Vec<RegistrationProblem>> {
    let mut problems = Vec::new();

    // ① 回显检查：宿主注入什么就得回什么。
    if got_id != expected_id {
        problems.push(RegistrationProblem::PluginIdNotEchoed {
            expected: expected_id.to_owned(),
            got: got_id.to_owned(),
        });
    }
    // ② 与 manifest 一致 —— 防止「包里写的是 A，启动时自称 B」。
    if got_id != manifest_id {
        problems.push(RegistrationProblem::PluginIdDiffersFromManifest {
            manifest: manifest_id.to_owned(),
            got: got_id.to_owned(),
        });
    }
    // ③ ABI 主版本必须**完全相等**：不兼容变更才递增，所以没有「更高也能跑」。
    if got_abi != ABI_MAJOR {
        problems.push(RegistrationProblem::AbiMismatch {
            host: ABI_MAJOR,
            plugin: got_abi,
        });
    }

    problems.extend(validate_capabilities(capabilities));

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// 校验能力声明本身（空列表是合法的：插件可以一个可选能力都没有）。
fn validate_capabilities(capabilities: &[i32]) -> Vec<RegistrationProblem> {
    let mut problems = Vec::new();
    let mut seen: Vec<i32> = Vec::new();

    for value in capabilities {
        if *value == 0 {
            problems.push(RegistrationProblem::UnspecifiedCapability);
            continue;
        }
        if !is_known_capability(*value) {
            problems.push(RegistrationProblem::UnknownCapability(*value));
            continue;
        }
        if seen.contains(value) {
            problems.push(RegistrationProblem::DuplicateCapability(*value));
            continue;
        }
        seen.push(*value);
    }
    problems
}

fn is_known_capability(value: i32) -> bool {
    matches!(
        value,
        capability::PROBE_DURATION
            | capability::PROBE_RESOLUTION
            | capability::PROBE_VIDEO_INFO
            | capability::OPEN_COVER_SOURCE
            | capability::IMPORT_SOURCE_IDENTITY
            | capability::SCAN_MEDIA_REFS
            | capability::SCAN_MANAGED_MEDIA_REF_KEYS
            | capability::MANAGED_MEDIA_REF_KEY
            | capability::SPACE_USAGE
            | capability::MERGED_PLAYBACK
            | capability::MERGED_PLAYBACK_PREFLIGHT
            | capability::TRANSFER_SOURCE
            | capability::TRANSFER_SOURCE_CLEANUP
            | capability::TRANSFER_TARGET
            | capability::EXTENSION_CATALOG_METADATA_SOURCE
            | capability::EXTENSION_RANKING_SOURCE
            | capability::DOWNLOAD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_registration_passes() {
        assert_eq!(
            validate_registration(
                "local",
                "local",
                "local",
                ABI_MAJOR,
                &[capability::DOWNLOAD]
            ),
            Ok(())
        );
        // 一个可选能力都没有也是合法的。
        assert_eq!(
            validate_registration("local", "local", "local", ABI_MAJOR, &[]),
            Ok(())
        );
    }

    #[test]
    fn the_echoed_plugin_id_must_match_both_the_host_and_the_manifest() {
        let problems =
            validate_registration("local", "local", "other", ABI_MAJOR, &[]).unwrap_err();
        // 同一个错值同时命中两条规则 —— 都该报出来。
        assert!(problems.contains(&RegistrationProblem::PluginIdNotEchoed {
            expected: "local".to_owned(),
            got: "other".to_owned()
        }));
        assert!(
            problems.contains(&RegistrationProblem::PluginIdDiffersFromManifest {
                manifest: "local".to_owned(),
                got: "other".to_owned()
            })
        );
    }

    #[test]
    fn the_abi_major_must_be_exactly_equal() {
        // 不兼容变更才递增，所以「更高」不能兼容加载。
        let problems =
            validate_registration("local", "local", "local", ABI_MAJOR + 1, &[]).unwrap_err();
        assert!(problems.contains(&RegistrationProblem::AbiMismatch {
            host: ABI_MAJOR,
            plugin: ABI_MAJOR + 1
        }));
    }

    #[test]
    fn capability_declarations_are_checked() {
        assert_eq!(
            validate_registration("local", "local", "local", ABI_MAJOR, &[0]).unwrap_err(),
            vec![RegistrationProblem::UnspecifiedCapability]
        );
        assert_eq!(
            validate_registration("local", "local", "local", ABI_MAJOR, &[999]).unwrap_err(),
            vec![RegistrationProblem::UnknownCapability(999)]
        );
        assert_eq!(
            validate_registration(
                "local",
                "local",
                "local",
                ABI_MAJOR,
                &[capability::DOWNLOAD, capability::DOWNLOAD]
            )
            .unwrap_err(),
            vec![RegistrationProblem::DuplicateCapability(
                capability::DOWNLOAD
            )]
        );
    }

    #[test]
    fn every_problem_has_a_code() {
        // 错误码要能进日志与 details，缺一个都会在排查时变成「无码」。
        assert_eq!(
            RegistrationProblem::UnspecifiedCapability.code(),
            "unspecified_capability"
        );
        assert_eq!(
            RegistrationProblem::AbiMismatch { host: 1, plugin: 2 }.code(),
            "abi_mismatch"
        );
    }
}
