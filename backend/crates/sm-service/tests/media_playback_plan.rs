//! `MediaService::plan_playback` —— 播放投递计划的服务层。
//!
//! # 这个文件在钉什么
//!
//! 上游把 422 判定放在**路由**里，靠的是宿主持有「provider 声明了哪些投递方式」
//! 那份清单。本仓的 ABI 里没有它，于是按方案 (b) 把请求的 `delivery` **传给
//! 插件**、由插件判定（`docs/adr/2026-10-08-provider-seam.md`）。
//!
//! 这个改动有一个**看不见的失败模式**：`requested` 参数若在隧道里被丢掉、或
//! 被某个实现硬编码成默认值，**所有既有测试与编译都会过** —— 表现是「客户端要
//! redirect，拿到的却是代理流」。所以这里用桩把「请求到达插件时是什么」记下来
//! 直接断言，而不是只看返回值。
//!
//! 另一条要钉的是 **`unsupported` 必须落成 422**，不能落成 5xx：那是「换一种
//! `delivery` 重试**可能成功**」的情形，报 5xx 会让客户端一直退避重试一个必败
//! 请求（上游 `media.py:278-283` 给的就是 422）。

mod support;

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use sm_db::testing::TestDb;
use sm_service::playback::media::MediaService;
use sm_service::playback::provider_helpers::{
    DeliveryTarget, MediaHandle, PlaybackGateway, PlaybackPlan, ProviderFailure, RequestedDelivery,
};

/// 一次调用的实录：`(provider_key, resource_path, requested)`。
type Seen = Vec<(String, String, RequestedDelivery)>;

/// 播放投递的桩。
struct FakePlayback {
    /// `Some(code)` → 返回这个失败码；`None` → 返回 `plan`。
    fail_with: Option<String>,
    plan: PlaybackPlan,
    seen: Mutex<Seen>,
}

/// 桩记录到的调用。
fn seen_of(fake: &FakePlayback) -> Seen {
    fake.seen.lock().expect("桩的锁没被毒化").clone()
}

fn redirect_plan() -> PlaybackPlan {
    PlaybackPlan {
        delivery: Some(DeliveryTarget::Redirect {
            url: "https://pan.example/d/abc".to_owned(),
            headers: Vec::new(),
        }),
        file_name: "ABC-001.mp4".to_owned(),
        size_bytes: Some(2_048),
        content_type: Some("video/mp4".to_owned()),
        unavailable: false,
    }
}

impl PlaybackGateway for FakePlayback {
    fn plan_playback(
        &self,
        handle: &MediaHandle,
        resource_path: &str,
        requested: RequestedDelivery,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<PlaybackPlan, ProviderFailure>> + Send + '_>>
    {
        self.seen.lock().expect("桩的锁没被毒化").push((
            handle.provider_key.clone(),
            resource_path.to_owned(),
            requested,
        ));
        let outcome = match &self.fail_with {
            Some(code) => Err(ProviderFailure {
                code: code.clone(),
                safe_message: "桩".to_owned(),
                retryable: false,
            }),
            None => Ok(self.plan.clone()),
        };
        Box::pin(async move { outcome })
    }

    fn plan_merged_playback(
        &self,
        _handles: &[MediaHandle],
        _resource_path: &str,
        _requested: RequestedDelivery,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<PlaybackPlan, ProviderFailure>> + Send + '_>>
    {
        // 本文件的用例只走单媒体那条路；这条留个响亮的失败，免得将来有人误以为
        // 「合并播放也测过了」。
        Box::pin(async move {
            Err(ProviderFailure {
                code: "unavailable".to_owned(),
                safe_message: "本桩不实现合并播放".to_owned(),
                retryable: false,
            })
        })
    }
}

/// ★ 请求的 `delivery` 必须**原样到达插件**。
///
/// 这个断言防的是「参数在隧道里被丢掉 / 被硬编码成默认值」—— 那种实现**编译
/// 通过、其它测试也全绿**，只在客户端要 redirect 时表现为静默走代理。
#[tokio::test]
async fn the_requested_delivery_reaches_the_plugin() {
    let db = TestDb::require().await;
    let image_root = support::ImageRoot::new();
    let media_id = support::seed_media(&db, Some("PLB-000001")).await;

    let fake = Arc::new(FakePlayback {
        fail_with: None,
        plan: redirect_plan(),
        seen: Mutex::new(Vec::new()),
    });
    let service = MediaService::new(db.pool(), &image_root.config)
        .with_playback_gateway(Arc::clone(&fake) as Arc<dyn PlaybackGateway>);

    let plan = service
        .plan_playback(media_id, "sub/1.srt", RequestedDelivery::Redirect)
        .await
        .expect("桩返回成功");

    // 计划原样带回（服务层不该改写它）。
    assert_eq!(plan, redirect_plan());

    let seen = seen_of(&fake);
    assert_eq!(seen.len(), 1, "只该调用一次");
    assert_eq!(
        seen[0].1, "sub/1.srt",
        "资源路径要传下去（子资源不在主文件里）"
    );
    assert_eq!(
        seen[0].2,
        RequestedDelivery::Redirect,
        "★ 请求的投递方式必须原样到达插件；丢掉它会让「要 302」静默变成代理流"
    );
    assert!(!seen[0].0.is_empty(), "provider_key 取自媒体所属的库");
}

/// ★ `unsupported` 落成 **422**，不是 5xx。
///
/// 落成 5xx 的后果：客户端把它当暂时故障、一直退避重试一个**永远不会成功**的
/// 请求；而真实语义是「换一种 `delivery` 重试可能成功」。
#[tokio::test]
async fn an_unsupported_delivery_is_a_422_not_a_5xx() {
    let db = TestDb::require().await;
    let image_root = support::ImageRoot::new();
    let media_id = support::seed_media(&db, Some("PLB-000002")).await;

    let service = MediaService::new(db.pool(), &image_root.config).with_playback_gateway(Arc::new(
        FakePlayback {
            fail_with: Some("unsupported".to_owned()),
            plan: redirect_plan(),
            seen: Mutex::new(Vec::new()),
        },
    )
        as Arc<dyn PlaybackGateway>);

    let error = service
        .plan_playback(media_id, "", RequestedDelivery::Redirect)
        .await
        .expect_err("桩返回 unsupported");

    assert_eq!(
        error.code(),
        "provider_playback_delivery_unsupported",
        "上游 media.py:278-283 给这个情形的是这个码"
    );
    // ★ **状态码**才是本条的重点。只断言 `code()` 是个陷阱：把构造器从
    // `validation`（422）换成 `unavailable`（503）时，**码不变**，只断 `code()`
    // 的测试会照样绿 —— 而 5xx 语义恰好是本条要否定的那个。
    // （这条是反向验证抓出来的：第一次这么换，测试没红。）
    assert_eq!(
        error.status, 422,
        "必须是 422：这是「换一种 delivery 重试可能成功」，不是服务端故障"
    );
}

/// 没有 provider（组合根没注入）= 503 `provider_not_installed`。
///
/// **不降级**成「空计划」：调用方无法区分「没装插件」与「插件说资源不在」。
#[tokio::test]
async fn without_a_gateway_playback_is_a_provider_not_installed() {
    let db = TestDb::require().await;
    let image_root = support::ImageRoot::new();
    let media_id = support::seed_media(&db, Some("PLB-000003")).await;

    let error = MediaService::new(db.pool(), &image_root.config)
        .plan_playback(media_id, "", RequestedDelivery::Unspecified)
        .await
        .expect_err("没注入播放能力");

    assert_eq!(error.code(), "provider_not_installed");
}

/// 媒体不存在 → 404 `media_not_found`（两级 404 的第一级）。
#[tokio::test]
async fn an_absent_media_is_a_media_not_found() {
    let db = TestDb::require().await;
    let image_root = support::ImageRoot::new();

    let error = MediaService::new(db.pool(), &image_root.config)
        .with_playback_gateway(Arc::new(FakePlayback {
            fail_with: None,
            plan: redirect_plan(),
            seen: Mutex::new(Vec::new()),
        }) as Arc<dyn PlaybackGateway>)
        .plan_playback(i32::MAX, "", RequestedDelivery::Unspecified)
        .await
        .expect_err("媒体不存在");

    assert_eq!(error.code(), "media_not_found");
}
