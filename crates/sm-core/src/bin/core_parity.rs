//! 契约对拍入口：把 sm-core 的类型输出为朴素文本，供 Python 侧比对。
//!
//! 与 `parity-cli` 同样的 `key: value` 输出约定。

#![forbid(unsafe_code)]

use serde_json::json;
use sm_core::{ApiError, AuthTokens, Paginated};

fn emit(fields: &[(&str, String)]) {
    for (key, value) in fields {
        println!("{key}: {value}");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("usage: core-parity <error-from-body|page-from-body|make-error> [json] [arg]");
        std::process::exit(2);
    };
    let raw = args.next().unwrap_or_else(|| "null".to_owned());
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);

    match command.as_str() {
        "error-from-body" => {
            let error = ApiError::from_body(&body);
            let mut fields = vec![
                ("code", error.code.clone()),
                ("message", error.message.clone()),
                ("has_details", error.details.is_some().to_string()),
                ("serialized", serde_json::to_string(&error).unwrap_or_default()),
            ];
            if let Some(details) = &error.details {
                fields.push(("details", serde_json::to_string(details).unwrap_or_default()));
            }
            emit(&fields);
        }
        "page-from-body" => {
            let page: Paginated<serde_json::Value> =
                Paginated::from_body(&body, |item| Some(item.clone()));
            emit(&[
                ("items", serde_json::to_string(&page.items).unwrap_or_default()),
                ("page", page.page.to_string()),
                ("page_size", page.page_size.to_string()),
                ("total", page.total.to_string()),
                ("has_synced_at", page.synced_at.is_some().to_string()),
                ("total_pages", page.total_pages().to_string()),
            ]);
        }
        "auth-from-body" => {
            match AuthTokens::from_body(&body) {
                Ok(tokens) => {
                    let json = tokens.to_client_json();
                    emit(&[
                        ("ok", "true".to_owned()),
                        ("access_token", tokens.access_token.clone()),
                        ("refresh_token", tokens.refresh_token.clone()),
                        ("token_type", tokens.token_type.clone()),
                        ("expires_in", tokens.expires_in.to_string()),
                        ("expires_at", json["expires_at"].as_str().unwrap_or_default().to_owned()),
                        ("refresh_expires_at", json["refresh_expires_at"].as_str().unwrap_or_default().to_owned()),
                        ("username", tokens.user.username.clone()),
                    ]);
                }
                Err(_) => {
                    emit(&[
                        ("ok", "false".to_owned()),
                        ("code", sm_core::auth::INVALID_AUTH_RESPONSE.to_owned()),
                        ("message", sm_core::auth::INVALID_AUTH_RESPONSE_MESSAGE.to_owned()),
                    ]);
                }
            }
        }
        "make-error" => {
            let code = args.next().unwrap_or_else(|| "e".to_owned());
            let with_details = body
                .get("with_details")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let mut error = ApiError::new(code, "消息");
            if with_details {
                let details = json!({"k": "v"}).as_object().cloned().unwrap_or_default();
                error = error.with_details(details);
            }
            emit(&[("serialized", serde_json::to_string(&error).unwrap_or_default())]);
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }
}
