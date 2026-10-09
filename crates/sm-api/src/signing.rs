//! 签名 URL 的运行时参数：密钥与当前时间。
//!
//! # 为什么密钥每次请求都要重读配置
//!
//! `PATCH /config` 能在**运行期**换掉 `auth.file_signature_secret`。把密钥
//! 缓存在状态里（或签名时读一次就记住）会让「刚换完密钥」的那段时间里签出的
//! URL 通不过校验 —— 而表现是「图片全部 403」，没有任何地方提示是缓存问题。
//!
//! 取 `snapshot()` 而不是 `get()`：后者剔除只读键，而密钥就在 `auth` 段里。
//!
//! # 为什么放在一个独立模块里
//!
//! 演员卡片与影片卡片都要签名图片 URL，两处各留一份实现会让上面那条规则
//! 有两份拷贝 —— 改一处漏一处就是一个只在某个端点复现的 403。

use serde_json::Value;

use crate::state::AppState;

/// Unix 秒。签名 URL 的有效期基准。
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 当前配置里的签名密钥。
///
/// 读不到时返回空串 —— [`crate::dto::sign_image_origin`] 对空密钥的签名失败
/// 处理是「原样返回路径」，所以这里不必把它变成错误：一张图签不了名不该让
/// 整个列表拿不到。
pub fn signing_secret(state: &AppState) -> String {
    state
        .config()
        .snapshot()
        .unwrap_or_default()
        .get("auth")
        .and_then(|auth| auth.get("file_signature_secret"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
