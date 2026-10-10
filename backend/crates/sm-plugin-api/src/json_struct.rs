//! `serde_json::Value` 与 `google.protobuf.Struct` 的互转。
//!
//! # 为什么放在**契约仓**
//!
//! 宿主侧（`sm-db` / `sm-service`）用 `serde_json::Value`，插件侧（proto 生成
//! 的类型）用 `google.protobuf.Struct`。这个转换在**两侧都要用**，而两边的
//! 规则必须完全一致 —— 放一份在宿主实现里，插件作者就只能读文档照抄，照抄
//! 总会漂移。
//!
//! # 转换规则（照 protobuf 的 well-known type 语义）
//!
//! | JSON | Struct |
//! |---|---|
//! | object | `Struct.fields` |
//! | array | `ListValue.values` |
//! | string / bool | `string_value` / `bool_value` |
//! | number | `number_value`（**f64**）|
//! | `null` | `null_value` |
//!
//! ⚠️ **数字一律走 f64**：`Struct` 的 `number_value` 就是 double。整数在
//! JSON 里能无损表示到 2^53，超过这个范围的整数会丢精度 —— 这是 protobuf
//! 的既定行为，不是本模块的选择。
//!
//! # 转不出来怎么办
//!
//! `json_to_struct` 只接受 **object 作为根**（`Struct` 的语义就是 object）；
//! 其它类型（数组、数字、字符串、null）返回 `None` —— 那是调用方传错了形状，
//! 不是「转成一个空对象」糊过去。

use prost_types::{ListValue, Struct, Value as PbValue};

/// JSON 对象 → `Struct`。**根不是对象则返回 `None`。**
pub fn json_to_struct(value: &serde_json::Value) -> Option<Struct> {
    let object = value.as_object()?;
    Some(Struct {
        fields: object
            .iter()
            .map(|(key, value)| (key.clone(), json_value(value)))
            .collect(),
    })
}

/// `Struct` → JSON 对象。`None` 得到 `null`（不是空对象 —— 两者语义不同）。
pub fn struct_to_json(value: Option<&Struct>) -> serde_json::Value {
    let Some(value) = value else {
        return serde_json::Value::Null;
    };
    let map = value
        .fields
        .iter()
        .map(|(key, value)| (key.clone(), json_from(value)))
        .collect();
    serde_json::Value::Object(map)
}

fn json_value(value: &serde_json::Value) -> PbValue {
    let kind = match value {
        serde_json::Value::Null => prost_types::value::Kind::NullValue(0),
        serde_json::Value::Bool(flag) => prost_types::value::Kind::BoolValue(*flag),
        serde_json::Value::Number(number) => {
            prost_types::value::Kind::NumberValue(number.as_f64().unwrap_or(0.0))
        }
        serde_json::Value::String(text) => prost_types::value::Kind::StringValue(text.clone()),
        serde_json::Value::Array(items) => prost_types::value::Kind::ListValue(ListValue {
            values: items.iter().map(json_value).collect(),
        }),
        serde_json::Value::Object(object) => prost_types::value::Kind::StructValue(Struct {
            fields: object
                .iter()
                .map(|(key, value)| (key.clone(), json_value(value)))
                .collect(),
        }),
    };
    PbValue { kind: Some(kind) }
}

fn json_from(value: &PbValue) -> serde_json::Value {
    use prost_types::value::Kind;
    match value.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(flag)) => serde_json::Value::Bool(*flag),
        Some(Kind::NumberValue(number)) => serde_json::Number::from_f64(*number)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(text)) => serde_json::Value::String(text.clone()),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(json_from).collect())
        }
        Some(Kind::StructValue(object)) => struct_to_json(Some(object)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四种标量 + 嵌套对象 / 数组，往返一致。
    ///
    /// ★ 数字一律按 **f64** 往返（`Struct.number_value` 是 double，见模块文档），
    /// 所以这里用浮点字面量。原先写的是整数字面量 `1` / `2`，而它们回来是
    /// `1.0` / `2.0` —— 那条断言**不可能成立**（`number_value` 是 double，
    /// 类型信息在 protobuf 层就丢了），不是待修的 bug。
    #[test]
    fn a_round_trip_preserves_every_kind() {
        let json = serde_json::json!({
            "s": "x", "b": true, "n": 1.5, "nil": null,
            "list": [1.0, "a", null],
            "obj": {"inner": {"deep": 2.0}}
        });
        let struct_value = json_to_struct(&json).expect("根是对象");
        assert_eq!(struct_to_json(Some(&struct_value)), json);
    }

    /// ★ 整数字面量往返后**变成浮点** —— 把上面那条注释里的取舍钉成契约，
    /// 免得后来者把它当 bug「修」掉（protobuf 层无从区分 `1` 与 `1.0`）。
    #[test]
    fn an_integer_literal_comes_back_as_a_float() {
        let json = serde_json::json!({ "count": 1 });
        let round_tripped = struct_to_json(Some(&json_to_struct(&json).expect("根是对象")));
        assert!(round_tripped["count"].is_f64(), "回来必须是浮点");
        assert_eq!(round_tripped["count"].as_f64(), Some(1.0));
    }

    /// ★ 根不是对象 → `None`，**不是**空对象（那是调用方传错形状）。
    #[test]
    fn a_non_object_root_is_rejected() {
        assert_eq!(json_to_struct(&serde_json::json!([])), None);
        assert_eq!(json_to_struct(&serde_json::json!(1)), None);
        assert_eq!(json_to_struct(&serde_json::json!("x")), None);
        assert_eq!(json_to_struct(&serde_json::json!(null)), None);
    }

    /// `None` 的 Struct 得到 JSON `null` 而不是 `{}` —— 两者语义不同：
    /// 前者是「没给」，后者是「给了个空的」。
    #[test]
    fn a_missing_struct_is_null_not_an_empty_object() {
        assert_eq!(struct_to_json(None), serde_json::Value::Null);
        assert_eq!(
            struct_to_json(Some(&Struct::default())),
            serde_json::json!({})
        );
    }
}
