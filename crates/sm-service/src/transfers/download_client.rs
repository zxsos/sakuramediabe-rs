//! 下载器客户端配置（上游 `downloads/client_config_service.py`，351 行）。
//!
//! # 这里**只**做库侧那两件事，其余三条卡在 provider seam
//!
//! | 入口 | 上游 | 现在 |
//! |---|---|---|
//! | [`DownloadClientService::list_clients`] | `:257-265` | ✅ 已落地（纯库）|
//! | [`DownloadClientService::delete_client`] | `:333-351` | ✅ 已落地（纯库）|
//! | `create_client` | `:266-278` | ⏳ 阶段二：要 `_prepare`（`:144-172`）|
//! | `update_client` | `:279-331` | ⏳ 阶段二：同上 |
//! | `test_client` | `:199-256` | ⏳ 阶段二：要插件探测 |
//!
//! # 为什么 `create` / `update` 不能「先落库、以后补校验」
//!
//! 上游那两条都要先 `_bundle(library)`（注册表里取 provider bundle）再做两件事：
//!
//! 1. `_validate_config`（`:94-128`）拿**插件声明**的 `config_fields` 判「未知字段 /
//!    只读字段」—— 宿主**没有**那份表；
//! 2. `_prepare`（`:144-172`）直接调 `bundle.downloads.prepare_client(...)`
//!    （合并 secret、派生/归一化配置），失败按 `provider_error` 映射。
//!
//! 宿主自己编一份字段表就是**把插件 schema 抄进宿主** —— 插件升版新增配置项时，
//! 老宿主会把新字段拒掉，那个插件就完全不能用（上游注释里说的「向前兼容」正是
//! 要防这个）。所以宁可留 `todo!()` + 上游行号，也不做半截实现。
//!
//! # ★ 形状按上游 `schema/transfers/downloads.py` 逐字对齐
//!
//! ⚠️ 骨架期这三种类型都是自造的，差异如下（都已按上游改回）：
//!
//! | 骨架期 | 上游 | 为什么 |
//! |---|---|---|
//! | `Resource { id, name, kind, enabled, config }` | `:14-33` `{ id, name, library_id, provider_config, created_at, updated_at }` | `kind` / `enabled` **库里没有这两列**；`config` 的真名是 `provider_config` |
//! | `CreateRequest { name, kind, config }` | `:35-38` `{ name, library_id(>0), provider_config(默认 `{}`) }` | 同上；`library_id` 是必填且必须为正 |
//! | `UpdateRequest { name, enabled, config }`（注释还写着「`kind` 刻意不提供，不可改」）| `:41-44` `{ name?, library_id?, provider_config? }` | 上游**根本没有 `kind` 字段**，也不存在 `download_client_library_change_forbidden` 这个码 —— 那张「kind 不可改」的表是骨架期编的 |
//! | `TestRequest { kind, config, library_id? }` | `:47-50` `{ library_id(>0), provider_config(默认 `{}`), client_id?(>0) }` | 探测要的是「库 + 配置（+ 可选已有客户端）」，不是「种类」 |
//!
//! # ★ 删除的两道 409：**先任务、后绑定**
//!
//! | 顺序 | 检查 | 码 |
//! |---|---|---|
//! | 1 | 该客户端名下**有没有任务行**（含历史） | `409 download_client_in_use` |
//! | 2 | 有没有索引器还绑着它 | `409 download_client_in_use_by_indexers` |
//!
//! ⚠️ 骨架期的文档把顺序写反了（「先查绑定再查任务」），实际上游是
//! `:335` 查任务、`:342` 查绑定。两道的**判据也不同**：第一道是「有没有任何任务」
//! （不是「有没有在跑的」），因为任务行会随下载器 `CASCADE` 删掉 —— 拦的是
//! 「你的下载历史会一起没」，不是「任务还在跑」。
//!
//! # `provider_config` 是**透传的黑盒**，但白名单校验在插件侧
//!
//! 宿主不解释里面有哪些字段，但「未知字段 / 只读字段」的判据来自**插件声明的
//! schema**（`_validate_config`），所以这条校验只能跟 provider seam 一起做。

use serde::{Deserialize, Serialize};

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
};
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::transfers::download_common::require_client;

/// 客户端配置（响应体）。上游 `DownloadClientResource`
/// （`schema/transfers/downloads.py:14-33`）。
///
/// `created_at` / `updated_at` 在实体里是 `Option`（迁移期可能有 NULL），
/// 上游模型继承 `TimestampedMixin` 所以是必填 —— 差异只在历史数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientResource {
    pub id: i32,
    pub name: String,
    /// 归属库。`library_id` **必填且 > 0**（上游 `:38`）。
    pub library_id: i32,
    /// 插件配置。**原样透传** —— 字段由插件解释。
    pub provider_config: serde_json::Value,
    pub created_at: Option<chrono::NaiveDateTime>,
    pub updated_at: Option<chrono::NaiveDateTime>,
}

impl DownloadClientResource {
    /// 从库里的行投影。`provider_config` 是不透明 JSON 文本 → 用
    /// [`provider_config_object`](crate::transfers::download_common) 同款规则
    /// （NULL / 脏数据当空对象，上游 `client.provider_config or {}`）。
    fn from_entity(row: &sm_db::DownloadClient) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            library_id: row.library_id,
            provider_config: sm_db_provider_config(row.provider_config.as_deref()),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// 库里的 `provider_config` 文本 → 对象。规则与 `download_common` 那份一致
/// （上游 `downloads/common.py:93` 的 `or {}`），所以复用它。
fn sm_db_provider_config(raw: Option<&str>) -> serde_json::Value {
    crate::transfers::download_common::provider_config_object(raw)
}

/// 创建请求。上游 `DownloadClientCreateRequest`（`:35-38`）。
///
/// `provider_config` 缺省是空对象；`library_id` 上游带 `gt=0` 校验。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientCreateRequest {
    pub name: String,
    pub library_id: i32,
    #[serde(default)]
    pub provider_config: serde_json::Value,
}

/// 更新请求 —— **部分更新**。上游 `DownloadClientUpdateRequest`（`:41-44`）。
///
/// ⚠️ 字段全 `Option` 带来 serde 的固有局限：`None` **不区分**「没传这个键」
/// 与「显式传了 `null`」。上游 FastAPI 用的是同一套 `exclude_unset` 语义，
/// 所以这不是移植引入的偏差。要真正区分得用 `Option<Option<T>>` + 自定义
/// 反序列化，上游没做，**照抄**。
///
/// 三个字段都可以改（`library_id` 也在内）—— 骨架期那条「`kind` 不可改」的规则
/// 上游并不存在，因为**上游没有 `kind` 字段**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownloadClientUpdateRequest {
    pub name: Option<String>,
    pub library_id: Option<i32>,
    pub provider_config: Option<serde_json::Value>,
}

/// 探测请求。**不落库**。上游 `DownloadClientTestRequest`（`:47-50`）。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientTestRequest {
    /// 用哪个库的 provider 来探测。**必填且 > 0**。
    pub library_id: i32,
    #[serde(default)]
    pub provider_config: serde_json::Value,
    /// 要测的是**已存在**的客户端（会带上库里存的配置）。
    pub client_id: Option<i32>,
}

/// 诊断中的一项检查。上游 `DownloadClientDiagnosticCheckResource`（`:53-58`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientDiagnosticCheck {
    pub key: String,
    /// `ok` / `warning` / `failed` / `skipped`。
    pub status: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// 探测诊断结果。上游 `DownloadClientDiagnosticResource`（`:60-64`）。
///
/// ⚠️ 骨架期这里是 `{ reachable, latency_ms, version, error }` —— **自造的四项**。
/// 上游给的是 `{ status, checks[], checked_at, elapsed_ms }`：一次探测会跑多项
/// 检查，每项各有自己的码与文案，客户端按 `checks[].status` 显示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientDiagnostic {
    /// `ok` / `warning` / `failed`。
    pub status: String,
    pub checks: Vec<DownloadClientDiagnosticCheck>,
    pub checked_at: chrono::NaiveDateTime,
    pub elapsed_ms: i64,
}

/// 插件能力：下载器配置与探测。**形状待 provider seam 定型**（阶段二）。
pub struct DownloadClientProvider {
    _private: (),
}

impl DownloadClientProvider {
    /// 取某个库的下载器 provider 句柄。**未装插件 → 503**。
    ///
    /// ⚠️ 骨架期这个函数收的是 `kind`（「下载器种类」）—— 而**没有 `kind` 这个东西**：
    /// provider 由**媒体库的 `provider_key`** 决定（上游 `_bundle(library)`，`:53-72`）。
    pub fn require(library_provider_key: &str) -> Result<Self, ServiceError> {
        let _ = library_provider_key;
        todo!("骨架：经 provider seam 取 download_client 能力；未装 -> 503 provider_not_installed")
    }

    /// 探测。**失败不返回 `Err`** —— 上游把失败也包成诊断结果（200）。
    pub async fn test(&self, config: &serde_json::Value) -> DownloadClientDiagnostic {
        let _ = config;
        todo!(
            "骨架：调插件的 downloads.test_client；连不上也要 200（见上游 `_diagnostic_resource`）"
        )
    }

    /// 插件声明的 `provider_config` schema。**用它**做字段白名单。
    pub fn config_schema(&self) -> serde_json::Value {
        todo!("骨架：取插件声明的配置 schema（字段白名单的依据，上游 `_validate_config`）")
    }
}

/// 下载器客户端服务（**库侧**）。
///
/// # 为什么要有状态
///
/// 骨架期这三个函数是**无状态的自由 `async fn`**（连 `Db` 都没有）—— 落不了地。
/// `list_clients` / `delete_client` 只查库，所以现在持 `Db` 就够；阶段二注入
/// provider seam 时再加一个网关字段（与 `playback::media::MediaService` 同款）。
pub struct DownloadClientService {
    db: Db,
}

impl DownloadClientService {
    /// 构造。
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    /// `GET /download-clients` —— **裸数组**（上游没有分页信封）。
    ///
    /// 上游 `list_clients`（`:257-265`）：`created_at DESC, id DESC`，最新在前。
    pub async fn list_clients(&self) -> Result<Vec<DownloadClientResource>, ServiceError> {
        Ok(DownloadClientRepository::new(self.db.clone())
            .list_ordered()
            .await?
            .iter()
            .map(DownloadClientResource::from_entity)
            .collect())
    }

    /// `DELETE /download-clients/{client_id}` —— **204**，无 body。
    ///
    /// 上游 `delete_client`（`:333-351`）。两道 409 **有先后**：
    ///
    /// 1. 有任务行（**含历史**）→ `download_client_in_use` + details `{client_id}`；
    /// 2. 有索引器绑定 → `download_client_in_use_by_indexers` + details `{client_id}`。
    ///
    /// 顺序照抄会更好用：先报「有任务」时用户删掉任务，再删会看到「被绑定」——
    /// 两步都能修完。反过来（骨架期文档写的那种顺序）也能修完，但与上游的
    /// **错误码**就不一致了，客户端按码分流的提示会错位。
    ///
    /// ⚠️ 删除会 `CASCADE` 掉该客户端的**全部任务历史**与索引器绑定，
    /// 所以这两道检查拦的正是「你确定要连历史一起删吗」。
    pub async fn delete_client(&self, client_id: i32) -> Result<(), ServiceError> {
        let client = require_client(&self.db, client_id).await?;

        if DownloadTaskRepository::new(self.db.clone())
            .exists_for_client(client.id)
            .await?
        {
            return Err(ServiceError::conflict(
                "download_client_in_use",
                "Download client is still referenced by download tasks",
                Some(details_of("client_id", serde_json::Value::from(client.id))),
            ));
        }
        if IndexerDownloadClientRepository::new(self.db.clone())
            .exists_for_client(client.id)
            .await?
        {
            return Err(ServiceError::conflict(
                "download_client_in_use_by_indexers",
                "Download client is still referenced by indexers",
                Some(details_of("client_id", serde_json::Value::from(client.id))),
            ));
        }

        DownloadClientRepository::new(self.db.clone())
            .delete(client.id)
            .await?;
        Ok(())
    }

    /// `POST /download-clients` —— **201**。**阶段二**（要 provider seam）。
    pub async fn create_client(
        &self,
        payload: DownloadClientCreateRequest,
    ) -> Result<DownloadClientResource, ServiceError> {
        let _ = payload;
        todo!("骨架：等 provider seam —— `_bundle` + `_validate_config`(`:94-128`) + `_prepare`(`:144-172`)")
    }

    /// `PATCH /download-clients/{client_id}` —— 不存在 → **404**。**阶段二**。
    pub async fn update_client(
        &self,
        client_id: i32,
        payload: DownloadClientUpdateRequest,
    ) -> Result<DownloadClientResource, ServiceError> {
        let _ = (client_id, payload);
        todo!("骨架：等 provider seam —— 上游 `:279-331`（`_ensure_name_available` 之后走 `_prepare`）")
    }

    /// `POST /download-clients/test` —— **无副作用**，成功与失败都 200。**阶段二**。
    pub async fn test_client(
        &self,
        payload: DownloadClientTestRequest,
    ) -> Result<DownloadClientDiagnostic, ServiceError> {
        let _ = payload;
        todo!("骨架：等 provider seam —— 上游 `:199-256` + `_diagnostic_resource`(`:175-198`)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 更新请求按上游三个字段反序列化；`{}` → 三个 `None`（「空更新」的判据）。
    ///
    /// ⚠️ 骨架期这里断言的是 `enabled` / `config` —— 那两个字段上游没有。
    #[test]
    fn the_update_request_matches_upstream_fields() {
        let full: DownloadClientUpdateRequest = serde_json::from_value(serde_json::json!({
            "name": "qb",
            "library_id": 7,
            "provider_config": {"host": "127.0.0.1"}
        }))
        .expect("三个字段都合法");
        assert_eq!(full.name.as_deref(), Some("qb"));
        assert_eq!(full.library_id, Some(7));
        assert!(full.provider_config.is_some());

        // 空对象 = 空更新（由 `update_client` 报 422 `empty_download_client_update`）。
        let empty: DownloadClientUpdateRequest =
            serde_json::from_value(serde_json::json!({})).expect("空对象合法");
        assert!(empty.name.is_none() && empty.library_id.is_none());
        assert!(empty.provider_config.is_none());

        // `kind` 不是上游字段 —— 传了也该被忽略，而不是当成未知字段报错
        // （serde 默认忽略未知键；这条用例把它钉住）。
        let with_kind: DownloadClientUpdateRequest =
            serde_json::from_value(serde_json::json!({"kind": "qbittorrent"}))
                .expect("未知键被忽略");
        assert!(with_kind.name.is_none());
    }

    /// ★ 创建请求：`library_id` 必填、`provider_config` 可缺省。
    #[test]
    fn the_create_request_matches_upstream() {
        let minimal: DownloadClientCreateRequest =
            serde_json::from_value(serde_json::json!({"name": "qb", "library_id": 3}))
                .expect("缺 provider_config 应合法");
        assert_eq!(minimal.library_id, 3);
        assert_eq!(
            minimal.provider_config,
            serde_json::json!(null),
            "serde 的缺省是 Null；上游是 `{{}}`，由服务层归一"
        );

        assert!(
            serde_json::from_value::<DownloadClientCreateRequest>(
                serde_json::json!({"name": "qb"})
            )
            .is_err(),
            "library_id 必填"
        );
    }

    /// ★ 响应体的**线上字段名**照上游：`library_id` / `provider_config`，
    /// 且**没有** `kind` / `enabled` / `config`。
    #[test]
    fn the_resource_wire_shape_has_no_invented_fields() {
        let resource = DownloadClientResource {
            id: 1,
            name: "qb".to_owned(),
            library_id: 2,
            provider_config: serde_json::json!({"host": "h"}),
            created_at: None,
            updated_at: None,
        };
        let json = serde_json::to_value(&resource).expect("序列化");
        assert_eq!(json["library_id"], 2);
        assert_eq!(json["provider_config"]["host"], "h");
        for absent in ["kind", "enabled", "config"] {
            assert!(json.get(absent).is_none(), "{absent} 不该出现在响应里");
        }
    }

    /// ★ 诊断结果的形状照上游：一次探测多项 `checks`。
    #[test]
    fn the_diagnostic_shape_is_the_upstream_one() {
        let diagnostic = DownloadClientDiagnostic {
            status: "warning".to_owned(),
            checks: vec![DownloadClientDiagnosticCheck {
                key: "connection".to_owned(),
                status: "ok".to_owned(),
                code: "ok".to_owned(),
                message: "reachable".to_owned(),
                details: None,
            }],
            checked_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 6)
                .expect("日期")
                .and_hms_opt(0, 0, 0)
                .expect("时刻"),
            elapsed_ms: 12,
        };
        let json = serde_json::to_value(&diagnostic).expect("序列化");
        assert_eq!(json["status"], "warning");
        assert_eq!(json["checks"][0]["key"], "connection");
        assert!(json["checks"][0].get("details").is_none(), "None 不序列化");
        for absent in ["reachable", "latency_ms", "version", "error"] {
            assert!(json.get(absent).is_none(), "{absent} 是骨架期自造的字段");
        }
    }
}
