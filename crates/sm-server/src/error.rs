//! 组合根的错误类型。
//!
//! 只有一个枚举而不是四个：组合根的每一步失败都是「起不来」，而调用方
//! （`main`）对它们的处理完全一样 —— 打进日志、返回非零退出码。分成多个
//! 类型只会让 `main` 里多出一段没有信息量的 match。

use std::path::Path;

/// 启动失败。
#[derive(Debug)]
pub enum ConfigError {
    /// 必填项缺失。
    Missing(&'static str),
    /// 取值非法。
    Invalid { key: &'static str, reason: String },
    /// 配置文件读不了。
    File { path: String, reason: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(key) => write!(f, "缺少必填配置：{key}"),
            Self::Invalid { key, reason } => write!(f, "配置 {key} 非法：{reason}"),
            Self::File { path, reason } => {
                write!(f, "读取配置文件 {path} 失败：{reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    /// 构造「配置文件读不了」。`path` 单独存一份而不是靠 `Display` 重取 ——
    /// 错误要能直接进日志，不能依赖调用方还留着那个 `Path`。
    pub fn file(path: &Path, err: impl std::fmt::Display) -> Self {
        Self::File {
            path: path.display().to_string(),
            reason: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_offending_key() {
        // 配置错误的读者是人，每条消息都要能直接指向要改的那一处。
        assert!(ConfigError::Missing("jwt_secret")
            .to_string()
            .contains("jwt_secret"));
        let invalid = ConfigError::Invalid {
            key: "SAKURAMEDIA_PORT",
            reason: "\"abc\" 不是合法端口".to_owned(),
        };
        let text = invalid.to_string();
        assert!(text.contains("SAKURAMEDIA_PORT"), "{text}");
        assert!(text.contains("不是合法端口"), "{text}");
    }

    #[test]
    fn a_file_error_keeps_the_path() {
        let err = ConfigError::file(std::path::Path::new("/etc/sakura/config.toml"), "解析失败");
        assert!(err.to_string().contains("/etc/sakura/config.toml"));
        assert!(err.to_string().contains("解析失败"));
    }
}
