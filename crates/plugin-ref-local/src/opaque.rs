//! 不透明引用（`google.protobuf.Struct`）的读写。
//!
//! proto 把 provider 私有的引用 —— `source_ref` / `parent_ref` / `storage_ref` /
//! `receipt` —— 一律定义成 `google.protobuf.Struct`。宿主只负责原样保存与回传，
//! 结构由插件自己解释，换来的是宿主无需理解 provider 方言。
//!
//! 代价是每个插件都要自己写一遍「取字段 + 缺字段报错」的代码，
//! 而这类代码既不出现在 proto 里，也无法被宿主校验（见报告 §4.2）。
//! 这里是本地插件的那一遍，schema 是固定的 `{"path": "<相对 root 的路径>"}`。

use std::collections::BTreeMap;

use prost_types::{value::Kind, Struct, Value};

/// 本地 provider 在不透明引用里使用的路径键名。
pub const REF_KEY_PATH: &str = "path";

/// 构造一个只含 `path` 字段的不透明引用。
pub fn string_ref(path: &str) -> Struct {
    let mut fields = BTreeMap::new();
    fields.insert(
        REF_KEY_PATH.to_owned(),
        Value {
            kind: Some(Kind::StringValue(path.to_owned())),
        },
    );
    Struct { fields }
}

/// 从引用里取出 `path`；字段缺失或类型不是字符串时返回 `None`。
///
/// 注意 `Kind::NullValue`（prost 用来表示 proto3 结构里的显式 null）
/// 同样落到 `None`，调用方无从区分「没传」和「传了 null」——
/// 这也是 `Struct` 作为通用不透明容器的固有代价。
pub fn ref_path(source: Option<&Struct>) -> Option<&str> {
    match source?.fields.get(REF_KEY_PATH)?.kind.as_ref()? {
        Kind::StringValue(path) => Some(path.as_str()),
        _ => None,
    }
}
