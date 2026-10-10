//! Provider 离线行为测试（不碰 115 网络）。
//!
//! - 未配置 Cookie 时 `browse` 应报 `unauthenticated`。
//! - provider_key 不匹配时应报 `invalid_argument`。

use plugin_115::{Plugin115Config, Provider115};
use sm_plugin_api::provider::StorageProviderExt;
use sm_plugin_api::v1::{BrowseRequest, LibraryHandle};
use tonic::Request;

fn empty_config() -> Plugin115Config {
    Plugin115Config::default()
}

fn library() -> LibraryHandle {
    LibraryHandle {
        provider_key: "115".to_owned(),
        ..Default::default()
    }
}

#[tokio::test]
async fn browse_without_cookie_is_unauthenticated() {
    let provider = Provider115::new(empty_config());
    let request = Request::new(BrowseRequest {
        library: Some(library()),
        ..Default::default()
    });
    let err = provider.browse(request).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn browse_with_wrong_provider_key_is_invalid_argument() {
    let provider = Provider115::new(empty_config());
    let request = Request::new(BrowseRequest {
        library: Some(LibraryHandle {
            provider_key: "other".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    });
    let err = provider.browse(request).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn browse_without_library_is_invalid_argument() {
    let provider = Provider115::new(empty_config());
    let request = Request::new(BrowseRequest::default());
    let err = provider.browse(request).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn prepare_library_without_cookie_is_invalid_argument() {
    use sm_plugin_api::v1::PrepareLibraryRequest;
    let provider = Provider115::new(empty_config());
    let request = Request::new(PrepareLibraryRequest {
        submitted_config: None,
        ..Default::default()
    });
    let err = provider.prepare_library(request).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
