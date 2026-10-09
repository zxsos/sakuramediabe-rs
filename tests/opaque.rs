//! 不透明引用编解码测试。

use plugin_115::opaque::{dir_ref, file_ref, ref_cid, ref_pickcode, ROOT_CID};

#[test]
fn dir_ref_roundtrip() {
    let reference = dir_ref("12345");
    assert_eq!(ref_cid(Some(&reference)), "12345");
    assert_eq!(ref_pickcode(Some(&reference)), None);
}

#[test]
fn file_ref_roundtrip() {
    let reference = file_ref("pc123", "678");
    assert_eq!(ref_pickcode(Some(&reference)), Some("pc123"));
    assert_eq!(ref_cid(Some(&reference)), "678");
}

#[test]
fn missing_cid_falls_back_to_root() {
    assert_eq!(ref_cid(None), ROOT_CID);
    assert_eq!(ROOT_CID, "0");
}

#[test]
fn missing_pickcode_is_none() {
    assert_eq!(ref_pickcode(None), None);
}
