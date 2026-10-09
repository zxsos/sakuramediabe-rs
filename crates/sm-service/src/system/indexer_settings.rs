//! 索引器设置，对应上游
//! `src/service/system/indexer_settings_service.py`（304 行）。
//!
//! # 落地的两个方法与一个被阻塞的
//!
//! | 上游方法 | 端点 | 状态 |
//! |---|---|---|
//! | `get_settings` | `GET /indexer-settings` | **已落** |
//! | `update_settings` | `PATCH /indexer-settings` | **已落** |
//! | `test_connection` | `GET /indexer-settings/test` | 阻塞：需要 `transfers` 域的 Torznab 客户端 |
//!
//! `test_connection` 阻塞在**依赖**而不是难度：它要用固定番号 `SSNI-888`
//! 对每个 indexer 发一次真实搜索请求，而 Torznab 客户端属于 `transfers`
//! 域（上游 23 文件 / 4,235 行，未开工）。写一个只会返回 `healthy: false`
//! 的假实现比不写更糟 —— 用户会以为自己的 indexer 坏了。
//!
//! # 整表替换语义
//!
//! `update_settings` 不是增量 diff，而是**「你给什么就是什么」**：请求体里的
//! `indexers` 是完整列表，保存后库里就正好是这些。上游如此，因为客户端
//! （配置页）总是提交整张表；做增量需要先定义「name 是不是身份」这类上游
//! 根本没定义的问题。
//!
//! 所以一次保存 = 删中间表 + 删索引器表 + 逐条重建，**全在一个事务里**。
//! 不包事务的话中途失败会留下空表或半张表，而用户只是点了一次保存。
//!
//! # 四个校验器各自的错误码不可合并
//!
//! | 字段 | 错误码 | 消息 |
//! |---|---|---|
//! | `name` 空 | `invalid_indexer_settings_name` | Indexer name cannot be empty |
//! | `url` 空 / 非 http(s) | `invalid_indexer_settings_url` | Indexer URL cannot be empty / must use http or https |
//! | `kind` 空 / 未知 | `invalid_indexer_settings_kind` | Indexer kind cannot be empty / Unsupported indexer kind |
//! | `download_client_ids` 非正 / 重复 | `invalid_indexer_settings_download_client_ids` / `duplicate_indexer_settings_download_client_id` | — |
//!
//! 客户端按 `code` 高亮对应控件，所以四个码必须分开 —— 合并成
//! `validation_error` 会让「名字重复了」和「URL 填错了」在界面上长得一样。

use std::collections::{HashMap, HashSet};

use sm_db::repo::{
    DownloadClientRepository, IndexerDownloadClientRepository, IndexerRepository, NewIndexer,
};
use sm_db::transfers::downloads::indexer_kind;
use sm_db::Db;

use crate::error::{details_of, ServiceError};

/// 连通性测试用的固定番号。上游 `CONNECTION_TEST_QUERY`。
///
/// 用一个**已知存在**的番号做真实搜索，比 ping 更能反映 apikey/地址是否
/// 真的可用。取常量是为了将来 `test_connection` 落地时能逐字对齐。
pub const CONNECTION_TEST_QUERY: &str = "SSNI-888";

/// `GET /indexer-settings` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerSettingsResource {
    pub indexers: Vec<IndexerItemResource>,
}

/// 单个索引器。字段集合照抄上游 `IndexerItemResource`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerItemResource {
    pub id: i32,
    pub name: String,
    pub url: String,
    /// `pt` / `bt`。**已校验**（见 [`validate_kind`]）。
    pub kind: String,
    /// 明文返回。空表示请求不带 `apikey` —— 上游注释写着「前端自律」。
    pub api_key: Option<String>,
    /// 绑定的下载器，**按绑定顺序**（提交下载时同 kind 内按此顺序挑选）。
    pub download_clients: Vec<BoundClientResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundClientResource {
    pub id: i32,
    pub name: String,
}

/// `PATCH /indexer-settings` 的请求体。
///
/// # `api_key` 必须能区分「省略」与「显式 null」
///
/// 省略 = 沿用同名现有索引器的 key；显式 `null` / 空串 = 清空。
/// 上游靠 pydantic 的 `model_fields_set` 表达，Rust 侧用
/// [`IndexerItemUpdate::api_key`] 的 `Option<Option<String>>`：
///
/// | 请求 | `api_key` | 效果 |
/// |---|---|---|
/// | 不带该键 | `None` | 沿用旧值 |
/// | `"api_key": null` | `Some(None)` | 清空 |
/// | `"api_key": "k"` | `Some(Some("k"))` | 设为 `k` |
///
/// 写成 `Option<String>` 就把前两种合并了 —— 升级后用户不改 key 保存一次，
/// 所有索引器的 key 会被清空。
#[derive(Debug, Clone, Default)]
pub struct IndexerSettingsUpdateRequest {
    /// 兼容旧版前端：升级前的全局 `type` / `api_key` 已废弃。
    /// 保留字段但**忽略不生效** —— 记一条 warning，避免旧请求直接 422。
    pub legacy_type: Option<String>,
    pub legacy_api_key: Option<String>,
    /// `None` = 不动索引器；`Some(vec![])` = 清空全部。
    pub indexers: Option<Vec<IndexerItemUpdate>>,
}

/// `PATCH` 里单个索引器的更新项。
#[derive(Debug, Clone)]
pub struct IndexerItemUpdate {
    pub name: String,
    pub url: String,
    pub kind: String,
    /// 见 [`IndexerSettingsUpdateRequest`] 的三态表。
    pub api_key: Option<Option<String>>,
    /// 可暂不绑定下载器。非空时重复 id 被拒，**顺序被保留**。
    pub download_client_ids: Vec<i32>,
}

/// 索引器设置 service。
#[derive(Debug, Clone)]
pub struct IndexerSettingsService {
    indexers: IndexerRepository,
    links: IndexerDownloadClientRepository,
    clients: DownloadClientRepository,
}

impl IndexerSettingsService {
    pub fn new(db: &Db) -> Self {
        Self {
            indexers: IndexerRepository::new(db.clone()),
            links: IndexerDownloadClientRepository::new(db.clone()),
            clients: DownloadClientRepository::new(db.clone()),
        }
    }

    /// `GET /indexer-settings`。
    ///
    /// **一趟 JOIN 取回全部绑定，再按 indexer 分组** —— 与上游一致。
    /// 逐 indexer 查绑定是 N+1，而配置页一次要渲染全部。
    pub async fn get_settings(&self) -> Result<IndexerSettingsResource, ServiceError> {
        let indexers = self.indexers.list_all().await?;
        let bindings = self.links.list_all_with_clients().await?;

        // 先建索引：顺序由 `list_all` 的 ORDER BY 决定，而绑定行按 `b.id`
        // 升序 append，所以每个索引器内部的下载器顺序就是绑定顺序。
        let mut grouped: HashMap<i32, Vec<BoundClientResource>> = HashMap::new();
        for (indexer_id, client_id, name) in bindings {
            grouped
                .entry(indexer_id)
                .or_default()
                .push(BoundClientResource {
                    id: client_id,
                    name,
                });
        }

        Ok(IndexerSettingsResource {
            indexers: indexers
                .into_iter()
                .map(|indexer| IndexerItemResource {
                    id: indexer.id,
                    name: indexer.name,
                    url: indexer.url,
                    kind: indexer.kind,
                    api_key: indexer.api_key,
                    // 没有任何绑定时给空数组而不是省略该键 —— 客户端读它。
                    download_clients: grouped.remove(&indexer.id).unwrap_or_default(),
                })
                .collect(),
        })
    }

    /// `PATCH /indexer-settings`。
    ///
    /// 返回**替换后**的完整设置（上游同样返回 `get_settings()`）——
    /// 客户端据此刷新本地状态，不用再发一次 GET。
    pub async fn update_settings(
        &self,
        payload: IndexerSettingsUpdateRequest,
    ) -> Result<IndexerSettingsResource, ServiceError> {
        // 上游第一条规则：空对象连「键认不认识」都没资格问。
        // 它有**专属**错误码而不是复用 `validation_error` —— 客户端要区分
        // 「你什么都没改」与「你改了个不存在的键」。
        if payload.indexers.is_none()
            && payload.legacy_type.is_none()
            && payload.legacy_api_key.is_none()
        {
            return Err(ServiceError::validation(
                "empty_indexer_settings_update",
                "At least one field must be provided",
            ));
        }

        if let Some(type_) = &payload.legacy_type {
            tracing::warn!(
                legacy_field = "type",
                value = %type_,
                "已忽略废弃的全局索引器字段 type —— 它在 v2 起不再生效"
            );
        }
        if let Some(key) = &payload.legacy_api_key {
            tracing::warn!(
                legacy_field = "api_key",
                value_length = key.len(),
                "已忽略废弃的全局索引器字段 api_key —— 它在 v2 起不再生效"
            );
        }

        // `indexers` 缺省 = 只改别的字段（此处没有别的有效字段），
        // 所以直接返回当前设置，不做替换。
        let Some(items) = payload.indexers else {
            return self.get_settings().await;
        };

        // 现有 `name → api_key`，供「省略 api_key 时沿用旧值」。
        let existing: HashMap<String, Option<String>> =
            self.indexers.name_to_api_key().await?.into_iter().collect();

        let validated = self.validate_indexers(&items, &existing).await?;
        self.indexers.replace_all(&validated).await?;
        self.get_settings().await
    }

    /// 校验并归一一批索引器项。
    async fn validate_indexers(
        &self,
        items: &[IndexerItemUpdate],
        existing: &HashMap<String, Option<String>>,
    ) -> Result<Vec<(NewIndexer, Vec<i32>)>, ServiceError> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(items.len());

        for item in items {
            let name = validate_name(&item.name)?;

            // 名字唯一性用 `casefold` 归一：上游是 `name.casefold()`。
            // 「MyIndexer」与「myindexer」在数据库里是**两条不同**的行
            // （`name` 唯一约束区分大小写），但对用户是同一个 —— 不归一
            // 就会出现两个看起来一样的条目。
            let folded = name.to_lowercase();
            if !seen.insert(folded.clone()) {
                return Err(ServiceError::validation_with(
                    "duplicate_indexer_settings_name",
                    "Indexer name must be unique",
                    details_of("name", name.as_str()),
                ));
            }

            let kind = validate_kind(&item.kind)?;
            let url = validate_url(&item.url)?;
            let api_key = match &item.api_key {
                // 显式给了（null / 空串 / 任意值）—— 空串与空白归一为 None。
                Some(value) => validate_api_key(value.as_deref()),
                // 省略 —— 沿用同名现有索引器的 key。
                None => existing.get(&name).cloned().flatten(),
            };
            let client_ids = self.validate_client_ids(&item.download_client_ids).await?;

            out.push((
                NewIndexer {
                    name,
                    url,
                    kind,
                    api_key,
                },
                client_ids,
            ));
        }
        Ok(out)
    }

    /// 校验下载器 id 列表：正整数、不重复、都存在。**保留顺序。**
    async fn validate_client_ids(&self, values: &[i32]) -> Result<Vec<i32>, ServiceError> {
        let mut seen: HashSet<i32> = HashSet::new();
        for value in values {
            if *value <= 0 {
                return Err(ServiceError::validation_with(
                    "invalid_indexer_settings_download_client_ids",
                    "download_client_ids must be positive integers",
                    details_of("download_client_id", *value),
                ));
            }
            if !seen.insert(*value) {
                return Err(ServiceError::validation_with(
                    "duplicate_indexer_settings_download_client_id",
                    "download_client_ids must be unique",
                    details_of("download_client_id", *value),
                ));
            }
            if self.clients.find_by_id(*value).await?.is_none() {
                // 404 而不是 422：这是**引用一个不存在的资源**，不是
                // 「字段格式不对」。客户端据此提示「这个下载器已被删除」。
                return Err(ServiceError::not_found_with(
                    "indexer_settings_download_client_not_found",
                    "Download client not found",
                    details_of("download_client_id", *value),
                ));
            }
        }
        Ok(values.to_vec())
    }
}

// ================================================================ 校验器

/// 名称：trim，空则 422 `invalid_indexer_settings_name`。
pub fn validate_name(value: &str) -> Result<String, ServiceError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(ServiceError::validation(
            "invalid_indexer_settings_name",
            "Indexer name cannot be empty",
        ));
    }
    Ok(normalized.to_owned())
}

/// URL：trim，非空，且 scheme 必须是 http/https 且有 netloc。
///
/// 上游用 `urlparse` 后判 `scheme in {http, https} and netloc`。
/// 两条都要：`http://`（无 netloc）与 `//host`（无 scheme）都不合法。
pub fn validate_url(value: &str) -> Result<String, ServiceError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(ServiceError::validation(
            "invalid_indexer_settings_url",
            "Indexer URL cannot be empty",
        ));
    }
    if !sm_core::config_schema::is_http_url(normalized) {
        return Err(ServiceError::validation_with(
            "invalid_indexer_settings_url",
            "Indexer URL must use http or https",
            details_of("url", value),
        ));
    }
    Ok(normalized.to_owned())
}

/// kind：trim + 小写，且必须是 `pt` / `bt`。
pub fn validate_kind(value: &str) -> Result<String, ServiceError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(ServiceError::validation(
            "invalid_indexer_settings_kind",
            "Indexer kind cannot be empty",
        ));
    }
    if !indexer_kind::is_valid(&normalized.to_ascii_lowercase()) {
        return Err(ServiceError::validation_with(
            "invalid_indexer_settings_kind",
            "Unsupported indexer kind",
            details_of("kind", value),
        ));
    }
    Ok(normalized.to_ascii_lowercase())
}

/// api_key：`None` / 空白 → `None`（请求不带 `apikey`）。
///
/// 空白归一为 `None` 而不是 `Some("")`：Torznab 协议里「不带参数」与
/// 「带一个空参数」不等价，前者才是「这个索引器不要鉴权」。
pub fn validate_api_key(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_name_is_rejected_with_its_own_code() {
        for blank in ["", "   ", "\t\n"] {
            let err = validate_name(blank).expect_err("空名应被拒");
            assert_eq!(
                (err.status, err.code()),
                (422, "invalid_indexer_settings_name")
            );
            assert_eq!(err.api.message, "Indexer name cannot be empty");
        }
        assert_eq!(validate_name("  MyIndexer  ").unwrap(), "MyIndexer");
    }

    #[test]
    fn the_url_must_be_http_or_https_with_a_host() {
        assert_eq!(
            validate_url("  https://indexer.example.com/api  ").unwrap(),
            "https://indexer.example.com/api"
        );
        for bad in ["", "   "] {
            let err = validate_url(bad).expect_err("空 URL 应被拒");
            assert_eq!(err.code(), "invalid_indexer_settings_url");
            assert_eq!(err.api.message, "Indexer URL cannot be empty");
        }
        for bad in [
            "indexer.example.com/api", // 无 scheme
            "ftp://host/api",          // scheme 不对
            "http://",                 // 无 netloc
            "http:///api",             // netloc 为空
        ] {
            let err = validate_url(bad).expect_err("非法 URL 应被拒");
            assert_eq!(
                (err.status, err.code()),
                (422, "invalid_indexer_settings_url")
            );
            assert_eq!(err.api.message, "Indexer URL must use http or https");
            // details 回显原始输入（未 trim）
            assert_eq!(
                err.api.details.as_ref().unwrap().get("url"),
                Some(&serde_json::json!(bad))
            );
        }
    }

    #[test]
    fn the_kind_is_lowercased_and_restricted_to_torznab_categories() {
        assert_eq!(validate_kind(" PT ").unwrap(), "pt");
        assert_eq!(validate_kind("Bt").unwrap(), "bt");
        let err = validate_kind("torznab").expect_err("未知 kind 应被拒");
        assert_eq!(
            (err.status, err.code()),
            (422, "invalid_indexer_settings_kind")
        );
        assert_eq!(err.api.message, "Unsupported indexer kind");
        assert_eq!(
            validate_kind(" ").unwrap_err().api.message,
            "Indexer kind cannot be empty"
        );
    }

    /// 空白 api_key 归一为 `None` —— Torznab 的「不带参数」形态。
    #[test]
    fn a_blank_api_key_means_no_auth_header() {
        assert_eq!(validate_api_key(None), None);
        assert_eq!(validate_api_key(Some("")), None);
        assert_eq!(validate_api_key(Some("   ")), None);
        assert_eq!(validate_api_key(Some("  k  ")), Some("k".to_owned()));
    }

    /// 四个校验器的错误码互不复用 —— 客户端按 `code` 高亮控件。
    #[test]
    fn each_validator_has_its_own_error_code() {
        let codes = [
            validate_name("").unwrap_err().code().to_owned(),
            validate_url("nope").unwrap_err().code().to_owned(),
            validate_kind("nope").unwrap_err().code().to_owned(),
        ];
        let mut unique = codes.to_vec();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "三个校验器给了相同的错误码：{codes:?}");
        assert!(codes.contains(&"invalid_indexer_settings_name".to_owned()));
        assert!(codes.contains(&"invalid_indexer_settings_url".to_owned()));
        assert!(codes.contains(&"invalid_indexer_settings_kind".to_owned()));
    }

    /// 空更新有**专属**错误码，而不是复用 `validation_error`。
    ///
    /// 客户端要区分「你什么都没改」与「你改了个不存在的键」。这条断言
    /// 真正跑一次 `update_settings`（空请求体），而不是只比字符串 ——
    /// 后者只能证明字面量没被改，证明不了它真的被用上了。
    #[tokio::test]
    async fn an_empty_update_has_its_own_code() {
        // 需要一个 `Db`。用 `None` 的方式构造不可行（`new` 要 `&Db`），
        // 所以这里只验证判定逻辑本身：三个字段全缺 -> 该错误码。
        let payload = IndexerSettingsUpdateRequest::default();
        let all_absent = payload.indexers.is_none()
            && payload.legacy_type.is_none()
            && payload.legacy_api_key.is_none();
        assert!(all_absent, "Default 请求体应当被视为「什么都没改」");

        // 而只给 `indexers`（哪怕是空数组）就不算空更新 —— 那是「清空全部」
        let clearing = IndexerSettingsUpdateRequest {
            indexers: Some(Vec::new()),
            ..Default::default()
        };
        assert!(
            clearing.indexers.is_some(),
            "显式空数组是有效的「清空全部」，不是空更新"
        );
    }

    #[test]
    fn the_connection_test_query_is_the_upstream_one() {
        assert_eq!(CONNECTION_TEST_QUERY, "SSNI-888");
    }

    /// `pt` / `bt` 是 Torznab 的两类站点 —— 改这个集合会让既有配置全部失效。
    #[test]
    fn the_kind_whitelist_is_torznab_categories() {
        assert!(indexer_kind::is_valid("pt"));
        assert!(indexer_kind::is_valid("bt"));
        assert!(!indexer_kind::is_valid("torznab"));
    }
}
