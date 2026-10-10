//! `proxy` 投递方式的**透明转发**。
//!
//! # 这个模块**不做** Range 解析（这是刻意的，不是没写完）
//!
//! 上游的 Range/206 只有**一份**实现（`common/range_streaming.py:51`），而它跑在
//! **持有文件的那一侧**：`PlaybackContext` 把原始 starlette `Request` 直接交给
//! provider（`provider_protocol.py:176-180`），provider 读完 `Range` 自己返回
//! `StreamingResponse`；宿主那边只有一句 `return await handle(...)`
//! （`playback/media.py:317`）—— **上游根本没有宿主转发这回事**。
//!
//! 跨进程后 `Request` 过不去，契约层用 `PlaybackPlan` 替代（`storage.proto:20-24`）。
//! 其中 `ProxyPlan { endpoint, path_prefix, headers }` 的含义是：**插件自己开一个
//! HTTP 端点提供字节**，于是 Range 仍然由**知道字节的那一侧**算，与上游同构。
//!
//! ⚠️ 所以宿主**绝不能**再算一遍 `Content-Range`。两侧都算、算得不一样时客户端会
//! 拿到与 body 长度不一致的头 —— 症状是「能播但进度条错乱」，**且没有任何报错**。
//! 本模块只做三件事：拼 URL、转发请求头（含原样的 `Range`）、镜像响应头与状态码。

use std::time::Duration;

use crate::error::ServiceError;

/// 转发时**原样带回**的响应头。
///
/// # 为什么是白名单而不是黑名单
///
/// 插件的 `set-cookie` / `www-authenticate` / `server` 这类头如果直接透给浏览器，
/// 轻则泄漏插件的内部信息，重则**把插件的凭据种到客户端**。白名单漏掉一个头，
/// 症状是「某个播放器读不到时长」；黑名单漏掉一个，症状是「凭据泄漏」。
/// 两种错误的代价不对称，所以选白名单。
const MIRRORED_RESPONSE_HEADERS: &[&str] = &[
    // —— 字节流本身的元信息（播放器全靠它们）
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "content-encoding",
    // —— 缓存协商
    "last-modified",
    "etag",
    "cache-control",
];

/// 一次转发的结果。
///
/// `status` 直接来自插件（200 / 206 / 416 都由它决定）—— 宿主**不**改写它。
pub struct ProxiedMedia {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static>,
    >,
}

/// 面向插件数据面的 HTTP 客户端。
///
/// 与 `EmbeddingClient` 同一类东西（`sm-service` 里的出站客户端），所以放在这一层
/// 而不是 `sm-api`：协议层只管「HTTP 响应长什么样」，不管「怎么把字节取回来」。
pub struct ProviderProxyClient {
    http: reqwest::Client,
}

/// 进程级共享实例。
///
/// # 为什么是全局而不是 `AppState` 的一个字段
///
/// `reqwest::Client` 内部就是连接池 + `Arc`，**每次请求新建一个**会为每次播放重建
/// 连接池 —— 播放正是长连接、多请求（Range 分片）的场景，代价直接落在起播延迟上。
///
/// 它不持有任何配置或状态（超时是常量，与签名窗口同类），所以没有理由挂在
/// `AppState` 上走一遍注入 —— 那会让每个组合根都得记得接它，而漏接的症状是
/// 「播放 503」，与「没装插件」混在一起。
pub fn shared() -> &'static ProviderProxyClient {
    static SHARED: std::sync::OnceLock<ProviderProxyClient> = std::sync::OnceLock::new();
    SHARED.get_or_init(|| ProviderProxyClient::new(PROXY_TIMEOUT))
}

/// 转发超时。
///
/// 取一个**比单次播放请求长得多**的值：这是整条流的超时，不是首字节超时。
/// 设短了会在长视频中途把流掐断（症状是「播到一半卡死」，而非报错）。
const PROXY_TIMEOUT: Duration = Duration::from_secs(3600);

impl ProviderProxyClient {
    /// 建一个客户端。
    ///
    /// `no_proxy()` 是**契约的一部分**，不是测试便利：插件端点通常是回环地址
    /// （`127.0.0.1:xxxx`），而部署环境里若有 `HTTP_PROXY` 之类的变量，回环也会被
    /// 塞进代理 → 表现成「插件明明活着，媒体一律 502」。同一理由见 `EmbeddingClient`
    /// 与 `TorznabClient`。
    pub fn new(timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .no_proxy()
                .build()
                .expect("构建出站 HTTP 客户端"),
        }
    }

    /// 转发一次。
    ///
    /// `range` 由调用方从**客户端请求**里原样取来 —— 本函数不解析、不改写它。
    pub async fn fetch(
        &self,
        endpoint: &str,
        path_prefix: &str,
        headers: &[(String, String)],
        resource_path: &str,
        range: Option<&str>,
    ) -> Result<ProxiedMedia, ServiceError> {
        let url = join_url(endpoint, path_prefix, resource_path);

        let mut request = self.http.get(&url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range);
        }

        let response = request.send().await.map_err(|error| {
            ServiceError::bad_gateway(
                "provider_media_unreachable",
                format!("转发插件媒体字节失败：{error}"),
                Default::default(),
            )
        })?;

        let status = response.status().as_u16();
        let headers = mirror_response_headers(response.headers());

        let body = futures::StreamExt::map(response.bytes_stream(), |chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(|error| std::io::Error::other(error.to_string()))
        });

        Ok(ProxiedMedia {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

/// 拼出插件数据面上的完整 URL。
///
/// `endpoint` 允许带或不带尾斜杠；`path_prefix` 与 `resource_path` 之间只补一个
/// 斜杠。`resource_path` **原样**接在最后 —— 它可能含子目录（字幕、片段），
/// 归一化由插件负责（上游 `verify_media_signature` 返回的就是归一化后的路径）。
fn join_url(endpoint: &str, path_prefix: &str, resource_path: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    let prefix = path_prefix.trim_matches('/');
    let path = resource_path.trim_start_matches('/');
    if prefix.is_empty() {
        format!("{endpoint}/{path}")
    } else {
        format!("{endpoint}/{prefix}/{path}")
    }
}

/// 按白名单镜像响应头（小写名，与 HTTP/2 及 `reqwest` 的表示一致）。
fn mirror_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let mut mirrored = Vec::new();
    for name in MIRRORED_RESPONSE_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(value) = value.to_str() {
                mirrored.push(((*name).to_owned(), value.to_owned()));
            }
        }
    }
    mirrored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_of(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("头名"),
                reqwest::header::HeaderValue::from_str(value).expect("头值"),
            );
        }
        map
    }

    #[test]
    fn the_url_join_tolerates_slashes_on_any_side() {
        assert_eq!(
            join_url("http://127.0.0.1:5001/", "/v1/media/", "/abc/main.mp4"),
            "http://127.0.0.1:5001/v1/media/abc/main.mp4"
        );
        assert_eq!(
            join_url("http://127.0.0.1:5001", "v1/media", "abc/main.mp4"),
            "http://127.0.0.1:5001/v1/media/abc/main.mp4"
        );
        // 空前缀：插件把数据面挂在根上。
        assert_eq!(
            join_url("http://127.0.0.1:5001", "", "abc/main.mp4"),
            "http://127.0.0.1:5001/abc/main.mp4"
        );
        // 子目录要保留（字幕、片段）。
        assert_eq!(
            join_url("http://h", "/v1", "sub/1.srt"),
            "http://h/v1/sub/1.srt"
        );
    }

    /// ★ 插件的 `Content-Range` **原样**带回，宿主不重算。
    ///
    /// 这里刻意用一个「奇怪但自洽」的值：只要它被改写（哪怕改得更"正确"），
    /// 就说明宿主在越权算 Range —— 而那种缺陷的症状是「能播但进度条错乱」，
    /// 没有任何报错。
    #[test]
    fn the_content_range_is_passed_through_verbatim() {
        let mirrored = mirror_response_headers(&headers_of(&[
            ("content-range", "bytes 100-199/123456789"),
            ("content-length", "100"),
            ("content-type", "video/mp4"),
            ("accept-ranges", "bytes"),
        ]));

        assert_eq!(
            mirrored,
            vec![
                ("content-type".to_owned(), "video/mp4".to_owned()),
                ("content-length".to_owned(), "100".to_owned()),
                (
                    "content-range".to_owned(),
                    "bytes 100-199/123456789".to_owned()
                ),
                ("accept-ranges".to_owned(), "bytes".to_owned()),
            ]
        );
    }

    /// ★ 白名单生效：插件的凭据类头**不透传**给客户端。
    #[test]
    fn credential_bearing_headers_are_not_mirrored() {
        let mirrored = mirror_response_headers(&headers_of(&[
            ("content-type", "video/mp4"),
            ("set-cookie", "session=secret"),
            ("www-authenticate", "Basic realm=x"),
            ("server", "plugin-internal/1.0"),
            ("x-plugin-internal-token", "leak-me"),
        ]));

        assert_eq!(
            mirrored,
            vec![("content-type".to_owned(), "video/mp4".to_owned())],
            "只有白名单里的头能出去"
        );
    }
}
