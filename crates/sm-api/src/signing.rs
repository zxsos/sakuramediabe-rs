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

use crate::error::ErrorResponse;

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
/// 读文件签名密钥。
///
/// # 「键不存在」容忍，「配置读不了」要吵
///
/// 原来这里是 `snapshot().unwrap_or_default()`，把两件事混成一件：
///
/// | 情况 | 处置 | 理由 |
/// |---|---|---|
/// | 配置文件不存在 | 空串 | 首次启动，签名功能没开 |
/// | 配置合法但没有这个键 | 空串 | 功能没开；空密钥让签名必然失败（403），**这是可诊断的** |
/// | **配置文件非法** | **返回 `Err`** | 部署坏了，不该伪装成「功能没开」 |
///
/// 第三种原来会退化成第二种：签名密钥变空串，于是**每一个**签名 URL 都 403，
/// 而错误信息指向「签名算法或密钥不对」—— **完全指错方向**。真正的病因是
/// 配置文件里一个转义反斜杠。
///
/// 代价与收益：改成 `Result` 会波及 16 个调用点（6 个 route 文件）。值得 ——
/// 组合根另有一次启动期校验（`ConfigService::validate`），所以真实部署里
/// 坏配置到不了这里；这里是第二道防线，而且它必须**吵**，否则第一道防线
/// 被人绕过时（运行期改坏配置）就又静默了。
/// 给一条媒体签「首个媒体」的播放地址。合集成员与视频详情共用。
///
/// 上游两处都**不带资源路径**（`build_signed_media_url(media.id,
/// delivery=bundle.playback_deliveries[0])`），所以这里 `resource_path` 传空串 ——
/// 播放端点自己按 `media_id` 反查文件。
///
/// `deliveries` 是 provider 声明的交付顺序，**取首项**（默认交付方式）。
///
/// 返回 `None`（**不是空串**）表示「没提供地址」：空串在客户端是「这个媒体在、
/// 但播不了」，会让整条播放列表被判成不可播放。空表或非法 delivery 都归 `None`。
pub fn signed_play_url(
    secret: &str,
    now: i64,
    media_id: i32,
    deliveries: &[String],
) -> Option<String> {
    let delivery = deliveries.first()?;
    sm_core::signing::build_signed_media_url(secret, media_id, "", delivery, now).ok()
}

pub fn signing_secret(state: &AppState) -> Result<String, ErrorResponse> {
    let config = crate::config::snapshot_or_500(state)?;
    Ok(
        crate::config::string_at(&config, "auth", "file_signature_secret")
            .unwrap_or_default()
            .to_owned(),
    )
}
