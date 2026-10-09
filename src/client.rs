//! 115 网盘 HTTP 客户端（对应 Python 版 `cloud115.py` 的核心子集）。
//!
//! 覆盖 provider 的四个需求：目录/文件、离线任务、直链、空间用量。
//! 认证走 Cookie（`UID` / `CID` / `SEID` / `KID` 为必需键），
//! 来源见 [`crate::config::Plugin115Config`] —— 本文件不读任何密钥。

use std::collections::HashMap;
use std::time::Duration;

use reqwest::header::{COOKIE, REFERER, USER_AGENT};
use serde::Deserialize;
use thiserror::Error;

/// 115 客户端错误。
#[derive(Debug, Error)]
pub enum Cloud115Error {
    /// 认证失败（Cookie 缺失或过期）。
    #[error("115 认证失败: {0}")]
    Auth(String),
    /// 资源不存在。
    #[error("115 资源不存在: {0}")]
    NotFound(String),
    /// 请求错误。
    #[error("115 请求失败: {0}")]
    Request(String),
    /// 传输错误。
    #[error("115 网络错误: {0}")]
    Transport(#[from] reqwest::Error),
}

/// 认证失败的 errno（与 Python 版 `_AUTH_ERRNOS` 一致）。
const AUTH_ERRNOS: [i64; 8] = [99, 911, 50003, 50004, 99999, 990009, 990017, 20130827];
/// 不存在的 errno（与 Python 版 `_NOT_FOUND_ERRNOS` 一致）。
const NOT_FOUND_ERRNOS: [i64; 5] = [20121, 20125, 990002, 4100003, 4100008];

/// Cookie 里保留的必需键（与 Python 版 `ESSENTIAL_COOKIE_KEYS` 一致）。
const ESSENTIAL_COOKIE_KEYS: [&str; 5] = ["UID", "CID", "SEID", "KID", "acw_tc"];

const WEBAPI: &str = "https://webapi.115.com";
const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// 115 目录条目。
#[derive(Debug, Clone)]
pub struct Cloud115Entry {
    /// 文件/目录 id。
    pub id: String,
    /// 父目录 id。
    pub parent_id: String,
    /// 名称。
    pub name: String,
    /// 是否目录。
    pub is_dir: bool,
    /// 字节数（目录为 0）。
    pub size_bytes: u64,
    /// pickcode（文件）。
    pub pickcode: String,
    /// sha1（文件，可空）。
    pub sha1: Option<String>,
}

/// 115 空间用量。
#[derive(Debug, Clone, Default)]
pub struct SpaceUsage {
    /// 总字节数。
    pub total_bytes: u64,
    /// 已用字节数。
    pub used_bytes: u64,
}

/// 离线任务。
#[derive(Debug, Clone)]
pub struct OfflineTask {
    /// info_hash。
    pub info_hash: String,
    /// 任务名。
    pub name: String,
    /// 状态。
    pub status: String,
    /// 进度百分比。
    pub percent: f64,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    #[serde(default)]
    state: bool,
    #[serde(default)]
    errno: i64,
    #[serde(default)]
    error: String,
    #[serde(default)]
    data: Option<T>,
    #[serde(default)]
    count: Option<i64>,
    #[serde(default)]
    cid: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileItem {
    #[serde(default, deserialize_with = "de_string")]
    fid: String,
    #[serde(default, deserialize_with = "de_string")]
    cid: String,
    #[serde(default, deserialize_with = "de_string")]
    pid: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    fc: String,
    #[serde(default)]
    s: u64,
    #[serde(default)]
    pc: String,
    #[serde(default)]
    sha: String,
}

fn de_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    struct S;
    impl<'de> Visitor<'de> for S {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("string or number")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_owned())
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
    }
    d.deserialize_any(S)
}

#[derive(Debug, Deserialize, Default)]
struct SpaceData {
    #[serde(default)]
    all_total: SpaceSize,
    #[serde(default)]
    all_use: SpaceSize,
}

#[derive(Debug, Deserialize, Default)]
struct SpaceSize {
    #[serde(default)]
    size: u64,
}

/// 115 客户端。
pub struct Cloud115Client {
    http: reqwest::Client,
    cookie: String,
}

impl std::fmt::Debug for Cloud115Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloud115Client")
            .field("cookie", &"<redacted>")
            .finish()
    }
}

impl Cloud115Client {
    /// 用 Cookie 字符串构造。只保留必需键，缺 UID 时报认证错误。
    pub fn new(cookie: &str) -> Result<Self, Cloud115Error> {
        let kept = keep_essential(parse_cookies(cookie));
        if kept.get("UID").map(String::as_str).unwrap_or("").is_empty() {
            return Err(Cloud115Error::Auth("115 cookie 缺少有效 UID".to_owned()));
        }
        let cookie_header = kept
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            cookie: cookie_header,
        })
    }

    fn headers(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert(COOKIE.to_string(), self.cookie.clone());
        map.insert(USER_AGENT.to_string(), USER_AGENT_VALUE.to_owned());
        map
    }

    async fn get_json_space(&self) -> Result<SpaceData, Cloud115Error> {
        let mut req = self.http.get(&format!("{WEBAPI}/user/space"));
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        let payload: ApiEnvelope<SpaceData> = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析 115 响应失败: {e}")))?;
        check_envelope(&payload, "user/space")?;
        payload
            .data
            .ok_or_else(|| Cloud115Error::Request("115 响应缺少 data: user/space".to_owned()))
    }

    /// 存活检查。
    pub async fn check_alive(&self) -> Result<bool, Cloud115Error> {
        let mut req = self.http.get("https://my.115.com/");
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        Ok(response.status().is_success())
    }

    /// 空间用量。
    pub async fn space_usage(&self) -> Result<SpaceUsage, Cloud115Error> {
        let data = self.get_json_space().await?;
        Ok(SpaceUsage {
            total_bytes: data.all_total.size,
            used_bytes: data.all_use.size,
        })
    }

    /// 列目录（一页）。
    ///
    /// 返回 `(条目, 总数)`。
    pub async fn list_dir(
        &self,
        cid: &str,
        offset: u64,
        limit: u64,
    ) -> Result<(Vec<Cloud115Entry>, u64), Cloud115Error> {
        if cid.is_empty() || limit == 0 || limit > 1150 {
            return Err(Cloud115Error::Request(
                "invalid 115 directory page".to_owned(),
            ));
        }
        let offset_s = offset.to_string();
        let limit_s = limit.to_string();
        let mut req = self.http.get(&format!("{WEBAPI}/files"));
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req
            .query(&[
                ("aid", "1"),
                ("cid", cid),
                ("offset", offset_s.as_str()),
                ("limit", limit_s.as_str()),
                ("show_dir", "1"),
            ])
            .send()
            .await?;
        let payload: ApiEnvelope<Vec<FileItem>> = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析 115 目录失败: {e}")))?;
        check_envelope(&payload, "files")?;
        if let Some(response_cid) = &payload.cid {
            let response_cid = response_cid.to_string().trim_matches('"').to_owned();
            if response_cid != cid {
                return Err(Cloud115Error::NotFound("115 目录不存在".to_owned()));
            }
        }
        let total = payload.count.unwrap_or(0).max(0) as u64;
        let items = payload.data.unwrap_or_default();
        let entries = items
            .into_iter()
            .map(|item| Cloud115Entry {
                id: if item.fc == "0" {
                    item.fid.clone()
                } else {
                    item.cid.clone()
                },
                parent_id: item.pid,
                name: item.n,
                is_dir: item.fc == "0",
                size_bytes: item.s,
                pickcode: item.pc,
                sha1: if item.sha.is_empty() {
                    None
                } else {
                    Some(item.sha)
                },
            })
            .collect();
        Ok((entries, total))
    }

    /// 列出目录下全部条目（自动翻页）。
    pub async fn list_directory(&self, cid: &str) -> Result<Vec<Cloud115Entry>, Cloud115Error> {
        let mut out = Vec::new();
        let mut offset = 0u64;
        loop {
            let (page, total) = self.list_dir(cid, offset, 1000).await?;
            let count = page.len() as u64;
            out.extend(page);
            offset += count;
            if count == 0 || offset >= total {
                break;
            }
        }
        Ok(out)
    }

    /// 取文件直链。
    pub async fn get_download_url(
        &self,
        pickcode: &str,
        user_agent: &str,
    ) -> Result<String, Cloud115Error> {
        let mut req = self.http.post("https://proapi.115.com/app/chrome/downurl");
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req
            .header(USER_AGENT, user_agent)
            .header(REFERER, "https://115.com/")
            .form(&[("pickcode", pickcode)])
            .send()
            .await?;
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析直链响应失败: {e}")))?;
        // Python 版会对 payload 做解密；这里取明文字段，加密形态走错误通道。
        let url = payload
            .pointer("/data/url/url")
            .or_else(|| payload.pointer("/data/url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Cloud115Error::Request("115 直链响应缺少 url".to_owned()))?;
        Ok(url.to_owned())
    }

    /// 按路径逐级查找目录，返回 cid。
    pub async fn resolve_path(&self, absolute_path: &str) -> Result<String, Cloud115Error> {
        let path = absolute_path.trim().trim_matches('/');
        if path.is_empty() {
            return Ok("0".to_owned());
        }
        let mut cid = "0".to_owned();
        for segment in path.split('/') {
            let entries = self.list_directory(&cid).await?;
            let found = entries
                .iter()
                .find(|e| e.is_dir && e.name == segment)
                .ok_or_else(|| {
                    Cloud115Error::NotFound(format!("115 路径不存在: {absolute_path}"))
                })?;
            cid = found.id.clone();
        }
        Ok(cid)
    }

    /// 新建目录，返回新 cid。
    pub async fn mkdir(&self, parent_cid: &str, name: &str) -> Result<String, Cloud115Error> {
        let mut req = self.http.post(&format!("{WEBAPI}/files/add"));
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req
            .form(&[("pid", parent_cid), ("cname", name)])
            .send()
            .await?;
        let payload: ApiEnvelope<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析 mkdir 响应失败: {e}")))?;
        check_envelope(&payload, "files/add")?;
        let cid = payload
            .data
            .as_ref()
            .and_then(|d| d.get("cid"))
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .ok_or_else(|| Cloud115Error::Request("mkdir 响应缺少 cid".to_owned()))?;
        Ok(cid)
    }

    /// 确保路径存在（逐级创建），返回最终 cid。
    pub async fn ensure_path(&self, absolute_path: &str) -> Result<String, Cloud115Error> {
        let path = absolute_path.trim().trim_matches('/');
        if path.is_empty() {
            return Ok("0".to_owned());
        }
        let mut cid = "0".to_owned();
        for segment in path.split('/') {
            let entries = self.list_directory(&cid).await?;
            if let Some(found) = entries.iter().find(|e| e.is_dir && e.name == segment) {
                cid = found.id.clone();
            } else {
                cid = self.mkdir(&cid, segment).await?;
            }
        }
        Ok(cid)
    }

    /// 添加离线任务，返回 info_hash。
    pub async fn add_offline_url(
        &self,
        source_url: &str,
        save_cid: &str,
    ) -> Result<String, Cloud115Error> {
        let mut req = self
            .http
            .post("https://115.com/web/lixian/?ct=lixian&ac=add_task_urls");
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req
            .form(&[("url", source_url), ("wp_path_id", save_cid)])
            .send()
            .await?;
        let payload: ApiEnvelope<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析离线任务响应失败: {e}")))?;
        check_envelope(&payload, "lixian/add_task_urls")?;
        let info_hash = payload
            .data
            .as_ref()
            .and_then(|d| d.get("info_hash"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        Ok(info_hash)
    }

    /// 列出离线任务。
    pub async fn list_offline_tasks(&self) -> Result<Vec<OfflineTask>, Cloud115Error> {
        let mut req = self
            .http
            .get("https://115.com/web/lixian/?ct=lixian&ac=task_lists");
        for (key, value) in self.headers() {
            req = req.header(key, value);
        }
        let response = req.send().await?;
        let payload: ApiEnvelope<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| Cloud115Error::Request(format!("解析离线任务列表失败: {e}")))?;
        check_envelope(&payload, "lixian/task_lists")?;
        let tasks = payload
            .data
            .as_ref()
            .and_then(|d| d.get("tasks"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tasks
            .iter()
            .map(|t| OfflineTask {
                info_hash: t
                    .get("info_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                status: t
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                percent: t.get("percent").and_then(|v| v.as_f64()).unwrap_or(0.0),
            })
            .collect())
    }
}

fn check_envelope<T>(envelope: &ApiEnvelope<T>, endpoint: &str) -> Result<(), Cloud115Error> {
    if AUTH_ERRNOS.contains(&envelope.errno)
        || !envelope.state && envelope.errno != 0 && is_auth_message(&envelope.error)
    {
        return Err(Cloud115Error::Auth(format!(
            "115 认证失效 ({}): {}",
            endpoint, envelope.error
        )));
    }
    if NOT_FOUND_ERRNOS.contains(&envelope.errno) {
        return Err(Cloud115Error::NotFound(format!(
            "115 资源不存在 ({}): {}",
            endpoint, envelope.error
        )));
    }
    if !envelope.state && envelope.errno != 0 {
        return Err(Cloud115Error::Request(format!(
            "115 接口错误 ({} errno={}): {}",
            endpoint, envelope.errno, envelope.error
        )));
    }
    Ok(())
}

fn is_auth_message(message: &str) -> bool {
    message.contains("login") || message.contains("登录") || message.contains("auth")
}

/// 解析 Cookie 字符串为键值对。
pub fn parse_cookies(value: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in value.split(';') {
        let (key, _, cookie_value) = part.partition('=');
        let key = key.trim();
        let cookie_value = cookie_value.trim();
        if !key.is_empty() && !cookie_value.is_empty() {
            out.insert(key.to_owned(), cookie_value.to_owned());
        }
    }
    out
}

/// 只保留必需的 Cookie 键。
fn keep_essential(values: HashMap<String, String>) -> HashMap<String, String> {
    values
        .into_iter()
        .filter(|(k, _)| ESSENTIAL_COOKIE_KEYS.contains(&k.as_str()))
        .collect()
}

trait Partition {
    fn partition(&self, sep: char) -> (String, char, String);
}

impl Partition for str {
    fn partition(&self, sep: char) -> (String, char, String) {
        match self.find(sep) {
            Some(idx) => (
                self[..idx].to_owned(),
                sep,
                self[idx + sep.len_utf8()..].to_owned(),
            ),
            None => (self.to_owned(), '\0', String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cookies_keeps_essential_keys() {
        let cookies = "UID=123; CID=abc; SEID=xyz; KID=k1; acw_tc=t; other=drop; empty=";
        let kept = keep_essential(parse_cookies(cookies));
        assert_eq!(kept.get("UID").map(String::as_str), Some("123"));
        assert_eq!(kept.get("other"), None);
        assert_eq!(kept.len(), 5);
    }

    #[test]
    fn client_rejects_cookie_without_uid() {
        let err = Cloud115Client::new("CID=abc; SEID=xyz").unwrap_err();
        assert!(matches!(err, Cloud115Error::Auth(_)));
    }

    #[test]
    fn client_accepts_valid_cookie() {
        let client = Cloud115Client::new("UID=1; CID=2; SEID=3; KID=4");
        assert!(client.is_ok());
    }

    #[test]
    fn envelope_auth_errno_maps_to_auth_error() {
        let envelope: ApiEnvelope<serde_json::Value> = ApiEnvelope {
            state: false,
            errno: 99,
            error: "need login".to_owned(),
            data: None,
            count: None,
            cid: None,
        };
        let err = check_envelope(&envelope, "test").unwrap_err();
        assert!(matches!(err, Cloud115Error::Auth(_)));
    }

    #[test]
    fn envelope_not_found_errno_maps_to_not_found() {
        let envelope: ApiEnvelope<serde_json::Value> = ApiEnvelope {
            state: false,
            errno: 20121,
            error: "not found".to_owned(),
            data: None,
            count: None,
            cid: None,
        };
        let err = check_envelope(&envelope, "test").unwrap_err();
        assert!(matches!(err, Cloud115Error::NotFound(_)));
    }
}
