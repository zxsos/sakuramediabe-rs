//! 下载器客户端配置（上游 `downloads/client_config_service.py`，351 行）。
//!
//! # `provider_config` 是**透传的黑盒**，但仍有白名单校验
//!
//! 看起来矛盾，其实不是：宿主**不解释**里面的字段（各插件字段不同），
//! 但要校验两件事 ——
//!
//! 1. **它是个对象**（不是数组/字符串/数字）
//! 2. **不含未知字段**（相对插件声明的配置 schema）
//!
//! 第 2 条是**向前兼容的关键**：插件升版新增了配置项时，老宿主若把新字段
//! 拒掉，那个插件就完全不能用。校验必须以**插件自己声明的 schema** 为准，
//! 而不是宿主硬编码一份字段清单。
//!
//! # 只读字段不可写：`name` / `kind` / `enabled` 的特殊规则
//!
//! 上游有两条不同的只读规则（`*_READONLY` 与 `*_IMMUTABLE`），照抄：
//!
//! | 字段 | 创建时 | 更新时 |
//! |---|---|---|
//! | `kind` | 必填 | **不可改**（`409 download_client_library_change_forbidden` 同族） |
//! | `enabled` | 可选 | 可改 |
//! | `name` | 必填 | 可改，但**重名 409** |
//!
//! `kind` 不可改是因为它决定 provider_config 的 schema —— 改了之后
//! 旧配置的含义就变了。
//!
//! # `test_client` **不落库**
//!
//! 这是个**无副作用的诊断端点**（见 `routes/download_clients.rs`）：
//! 它测的是「这份配置能不能连上」，所以
//!
//! - **不要求 `client_id` 存在** —— 测的是待创建的配置
//! - **连不上返回 200 + `reachable: false`**，不是 5xx
//!
//! # 删除的两个前置检查，顺序不能反
//!
//! | 检查 | 错误码 |
//! |---|---|
//! | 被索引器绑定 | `409 download_client_in_use_by_indexers` |
//! | 有任务在跑 | `409 download_client_in_use` |
//!
//! 先查绑定再查任务：绑定是**配置层面**的冲突（静态），任务是运行状态
//! （动态）。反过来会让用户看到「有任务在跑」，去掉任务后又看到「被绑定」，
//! 两次修复。

use serde::{Deserialize, Serialize};

use super::download_common::DownloadClientRow;
use crate::error::ServiceError;

/// 客户端配置（响应体，对齐上游 `DownloadClientResource`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientResource {
    pub id: i32,
    pub name: String,
    /// 下载器种类。**取值由插件决定**。
    pub kind: String,
    pub enabled: bool,
    /// 插件配置。**原样透传**。
    pub config: serde_json::Value,
}

/// 创建请求。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientCreateRequest {
    pub name: String,
    pub kind: String,
    /// 插件配置。必须是**对象**，且字段不得超出插件声明的 schema。
    pub config: serde_json::Value,
}

/// 更新请求 —— **部分更新**。
///
/// ⚠️ 字段全 `Option` 带来 serde 的固有局限：`None` **不区分**「没传这个键」
/// 与「显式传了 `null`」。上游 FastAPI 用的是同一套 `exclude_unset` 语义，
/// 所以这不是移植引入的偏差。要真正区分得用 `Option<Option<T>>` + 自定义
/// 反序列化，上游没做，**照抄**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownloadClientUpdateRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<serde_json::Value>,
    /// `kind` **不提供** —— 见模块文档「`kind` 不可改」。
}

/// 探测请求。**不落库。**
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientTestRequest {
    pub kind: String,
    pub config: serde_json::Value,
    /// 探测用的媒体库。`Some` 时校验它与配置里的库是否一致
    /// （`422 download_client_test_library_mismatch`）。
    pub library_id: Option<i64>,
}

/// 诊断结果。**失败也返回 200**（见模块文档）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientDiagnostic {
    pub reachable: bool,
    pub latency_ms: Option<i64>,
    pub version: Option<String>,
    /// 失败原因。`reachable = true` 时为 `None`。
    pub error: Option<String>,
}

/// 插件能力：下载器配置与探测。
pub struct DownloadClientProvider {
    _private: (),
}

impl DownloadClientProvider {
    /// 取某个客户端的 provider 句柄。**未装插件 → 503**。
    pub fn require(kind: &str) -> Result<Self, ServiceError> {
        let _ = kind;
        todo!("骨架：经 sm-plugins 取 download_client 能力；未装 -> 503 provider_not_installed")
    }

    /// 探测。**失败不返回 `Err`** —— 返回 `DownloadClientDiagnostic { reachable: false }`。
    pub async fn test(&self, config: &serde_json::Value) -> DownloadClientDiagnostic {
        let _ = config;
        todo!("骨架：调插件的 downloads.test_client；连不上也要 Ok(reachable=false)")
    }

    /// 插件声明的 `provider_config` schema。**用它**做字段白名单。
    pub fn config_schema(&self) -> serde_json::Value {
        todo!("骨架：取插件声明的配置 schema（字段白名单的依据）")
    }

    /// 当前库里是否还有该客户端的任务在跑。
    pub async fn has_running_tasks(&self, client: &DownloadClientRow) -> Result<bool, ServiceError> {
        let _ = client;
        todo!("骨架：查 download_task 是否有该客户端的进行中任务")
    }
}

/// `GET /download-clients`
pub async fn list_clients() -> Result<Vec<DownloadClientResource>, ServiceError> {
    todo!("骨架：查 download_client 表，按 id 升序")
}

/// `POST /download-clients` —— **201**。
pub async fn create_client(
    payload: DownloadClientCreateRequest,
) -> Result<DownloadClientResource, ServiceError> {
    let _ = payload;
    todo!("骨架：校验 name/kind/provider_config -> 落库；重名 -> 409 download_client_name_conflict")
}

/// `POST /download-clients/test` —— **无副作用**，成功与失败都 200。
pub async fn test_client(
    payload: DownloadClientTestRequest,
) -> Result<DownloadClientDiagnostic, ServiceError> {
    let _ = payload;
    todo!("骨架：不落库；连不上返回 200 + reachable=false（不要 502）")
}

/// `PATCH /download-clients/{client_id}` —— 不存在 → **404**。
pub async fn update_client(
    client_id: i32,
    payload: DownloadClientUpdateRequest,
) -> Result<DownloadClientResource, ServiceError> {
    let _ = (client_id, payload);
    todo!("骨架：部分更新；空更新 -> 422 empty_download_client_update；kind 不可改")
}

/// `DELETE /download-clients/{client_id}` —— **204**，无 body。
///
/// 顺序：先查索引器绑定（409），再查运行中任务（409），最后才删。见模块文档。
pub async fn delete_client(client_id: i32) -> Result<(), ServiceError> {
    let _ = client_id;
    todo!("骨架：先查索引器绑定 -> 再查运行任务 -> 才删（两次 409 有先后）")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `kind` **不在**更新请求里 —— 它不可改。
    ///
    /// 加上它就意味着「改 kind 后旧配置的字段含义变了」，而那不会报错，
    /// 只会让下载器在某天突然连不上。
    #[test]
    fn the_update_request_cannot_change_the_kind() {
        let request: DownloadClientUpdateRequest =
            serde_json::from_str(r#"{"kind":"transmission"}"#).expect("可解析");
        assert_eq!(request.name, None);
        assert_eq!(request.enabled, None);
        assert_eq!(request.config, None);
    }

    /// 三个 `Option` 全 `None` = **空更新** → 422 `empty_download_client_update`。
    ///
    /// 不能当成功：那样一次空 PATCH 也会触发一次无意义的写入与 `updated_at` 刷新。
    #[test]
    fn an_empty_update_is_distinguishable_from_a_real_one() {
        let empty: DownloadClientUpdateRequest = serde_json::from_str("{}").expect("可解析");
        let named: DownloadClientUpdateRequest =
            serde_json::from_str(r#"{"name":"qb2"}"#).expect("可解析");
        assert!(empty.name.is_none() && empty.enabled.is_none() && empty.config.is_none());
        assert_eq!(named.name.as_deref(), Some("qb2"));
    }

    /// 诊断结果的失败分支：`reachable: false` 且**有** `error` 文案。
    ///
    /// 反过来（`reachable: true` 却带 `error`）会让客户端把正常结果显示成失败。
    #[test]
    fn an_unreachable_client_reports_why() {
        let diagnostic = DownloadClientDiagnostic {
            reachable: false,
            latency_ms: None,
            version: None,
            error: Some("连接超时".to_owned()),
        };
        assert!(!diagnostic.reachable);
        assert!(diagnostic.error.is_some());
        assert!(diagnostic.latency_ms.is_none(), "不可达时没有延迟");
    }
}
