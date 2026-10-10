//! 插件配置（上游 `settings.py:JavdbRankingSettings`）。
//!
//! # 只有账号两个字段
//!
//! JavDB 的出网细节（基址、UA、签名、代理）归**宿主**管 —— 插件只把账号透传
//! 进去，宿主**不保管**任何账号（`host.proto` 的 `GetJavdbRankNumbers` 原话）。
//! 所以这里没有基址、超时、Cookie 之类：那些是本仓早期「插件自己抓榜单」时
//! 留下的，现在去掉了。
//!
//! # 账号决定 TOP250 抓不抓
//!
//! TOP250 要登录才看得到；未配账号时 [`Settings::account_configured`] 为假，
//! `ResolveRankingPeriods` 对 TOP250 回空数组（本次不抓，正常结果）。

/// 插件私有配置。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// JavDB 用户名。
    pub javdb_username: String,
    /// JavDB 密码。
    pub javdb_password: String,
}

impl Settings {
    /// 从已解析的 JSON 构造。
    ///
    /// **进程内组合根走这条**：它直接把 `plugins.<id>.settings` 这个 `Value`
    /// 传进来，不经过「写文件 + 环境变量指路」那一套（进程内只有一份进程环境，
    /// 多插件会互相覆盖）。结构体是 `#[serde(default)]`，缺键回落默认。
    pub fn from_json(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// 账号密码**都**非空白才算配好（上游 `account_configured` 的 `bool(...)`）。
    ///
    /// 首尾空白要裁掉再判断：设置页里粘一次带换行的密码，会变成「看起来配了、
    /// 登录一直失败」。
    pub fn account_configured(&self) -> bool {
        !self.javdb_username.trim().is_empty() && !self.javdb_password.trim().is_empty()
    }

    /// 透传给宿主的账号（原样，不裁 —— 密码里的空白是用户的事）。
    pub fn credentials(&self) -> (String, String) {
        (self.javdb_username.clone(), self.javdb_password.clone())
    }

    /// settings 表单的字段列表（宿主渲染用）。
    pub fn schema() -> Vec<sm_plugin_api::v1::SettingsField> {
        use sm_plugin_api::v1::SettingsField;
        vec![
            SettingsField {
                key: "javdb_username".to_owned(),
                label: "JavDB 用户名".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("用于抓取 TOP250（其余榜单无需登录）".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
            SettingsField {
                key: "javdb_password".to_owned(),
                label: "JavDB 密码".to_owned(),
                input: "password".to_owned(),
                required: false,
                description: Some("同上；账号与密码都填了才会抓 TOP250".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_account_is_not_configured() {
        let settings = Settings::default();
        assert!(!settings.account_configured());
        assert!(!Settings::from_json(&serde_json::json!({
            "javdb_username": "someone",
            "javdb_password": "",
        }))
        .account_configured());
    }

    #[test]
    fn whitespace_only_fields_do_not_count_as_configured() {
        // 粘一次带换行的密码不该表现成「配好了」。
        let settings = Settings::from_json(&serde_json::json!({
            "javdb_username": " someone ",
            "javdb_password": "\n",
        }));
        assert!(!settings.account_configured());
    }

    #[test]
    fn both_fields_configured() {
        let settings = Settings::from_json(&serde_json::json!({
            "javdb_username": "someone",
            "javdb_password": "secret",
        }));
        assert!(settings.account_configured());
        assert_eq!(settings.credentials(), ("someone".to_owned(), "secret".to_owned()));
    }

    #[test]
    fn schema_has_the_two_account_fields() {
        let fields = Settings::schema();
        let keys: Vec<&str> = fields.iter().map(|field| field.key.as_str()).collect();
        assert_eq!(keys, ["javdb_username", "javdb_password"]);
        assert_eq!(fields[1].input, "password", "密码要遮起来");
    }
}
