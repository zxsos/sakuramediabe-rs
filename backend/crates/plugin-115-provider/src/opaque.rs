//! 不透明引用（`google.protobuf.Struct`）的读写。
//!
//! 115 provider 的引用 schema：
//!
//! - 目录引用：`{"cid": "<115 目录 id>"}` —— 根目录是 `"0"`。
//! - 文件引用：`{"pickcode": "<115 pickcode>", "cid": "<父目录 id>"}`。
//!
//! 宿主只负责原样保存与回传，结构由本插件解释。

use std::collections::BTreeMap;

use prost_types::{value::Kind, Struct, Value};

/// 目录引用里的目录 id 键名。
pub const REF_KEY_CID: &str = "cid";
/// 文件引用里的 pickcode 键名。
pub const REF_KEY_PICKCODE: &str = "pickcode";
/// 115 根目录 id。
pub const ROOT_CID: &str = "0";

fn struct_of(pairs: &[(&str, &str)]) -> Struct {
    let mut fields = BTreeMap::new();
    for (key, value) in pairs {
        fields.insert(
            (*key).to_owned(),
            Value {
                kind: Some(Kind::StringValue((*value).to_owned())),
            },
        );
    }
    Struct { fields }
}

/// 构造目录引用。
pub fn dir_ref(cid: &str) -> Struct {
    struct_of(&[(REF_KEY_CID, cid)])
}

/// 构造文件引用。
pub fn file_ref(pickcode: &str, cid: &str) -> Struct {
    struct_of(&[(REF_KEY_PICKCODE, pickcode), (REF_KEY_CID, cid)])
}

fn get_str<'a>(source: Option<&'a Struct>, key: &str) -> Option<&'a str> {
    match source?.fields.get(key)?.kind.as_ref()? {
        Kind::StringValue(value) => Some(value.as_str()),
        _ => None,
    }
}

/// 从引用里取目录 id；缺失时返回根目录 `"0"`。
pub fn ref_cid(source: Option<&Struct>) -> &str {
    get_str(source, REF_KEY_CID).unwrap_or(ROOT_CID)
}

/// 从引用里取 pickcode；不是文件引用时返回 `None`。
pub fn ref_pickcode(source: Option<&Struct>) -> Option<&str> {
    get_str(source, REF_KEY_PICKCODE)
}
