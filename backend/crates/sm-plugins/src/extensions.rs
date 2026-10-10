//! 三个扩展点的声明收集与校验。
//!
//! # 上游对应
//!
//! `plugin.proto` 的 `Extension` 是 oneof，三个变体各对应 Python 侧一个扩展点：
//!
//! | `Extension.key` | 上游 | 本仓库 |
//! |---|---|---|
//! | `media.provider` | `src/plugins/extensions/media_provider.py` | [`crate::loader::collect_providers`] |
//! | `catalog.metadata_source` | `src/plugins/extensions/metadata.py` | 本模块 |
//! | `discovery.ranking_source` | `src/plugins/extensions/ranking.py` | 本模块 |
//!
//! # 只收声明，不做调用
//!
//! 取数走 `MetadataSourceExtensionService.FetchMovie` 与
//! `RankingSourceExtensionService.FetchRanking` —— 那是**调用面**，住在
//! [`crate::extension_calls`]。这里只把「这个插件声明了什么、能不能用」算清楚，
//! 与 [`crate::jobs`] / [`crate::registry`] 是一个手法：加载期判死，调用期不炸。
//!
//! # 能力是调用前提
//!
//! proto 在 `MetadataSourceExtensionService` 上的原话：「仅在声明对应 capability
//! 时才会被调用」。所以声明了扩展点却没声明能力时，这里**不收**它 —— 收了也是
//! 一个永远不会被调到的条目。
//!
//! # 冲突隔离到什么程度
//!
//! 上游 `apply_plugin_ranking_sources` 在 `source_key` 冲突时**整插件拒绝**
//! （连它的任务都不注册，见 `src/scheduler/registry.py`）。本模块只管扩展点
//! 这一张表，所以做到「该插件的排行榜来源全部不收」，并把问题报出去 ——
//! 「连任务一起不注册」要等加载器把三张表串起来时才能落地。

use std::collections::HashMap;

use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::{RankingSourceExtension, RegisterResponse};

use crate::registration::capability;

/// `media.provider` 扩展点 key。
pub const MEDIA_PROVIDER: &str = "media.provider";
/// `catalog.metadata_source` 扩展点 key。
pub const METADATA_SOURCE: &str = "catalog.metadata_source";
/// `discovery.ranking_source` 扩展点 key。
pub const RANKING_SOURCE: &str = "discovery.ranking_source";

/// 一个元数据来源（`catalog.metadata_source`）。
///
/// 上游 `MetadataSourceService.register` 存的是 `(plugin_id, display_name,
/// source)` 三元组，来源是**插件粒度**（`enabled_plugin_sources` 用
/// `{plugin_id: ...}` 去重），所以这里也以 `plugin_id` 为键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSourceRegistration {
    pub plugin_id: String,
    /// 插件的 `display_name`。上游把它与来源一起存下来，供日志与提示用。
    pub display_name: String,
    /// 那个插件的**控制面**端点。与 [`RankingSourceRegistration::plugin_endpoint`]
    /// 同一个东西、同一个理由：元数据来源要调 `FetchMovie`，没有端点就
    /// 「查得到声明、打不出去」。
    ///
    /// ⚠️ 与端点一样是**活的** —— 插件重启会换端口。消费方应在**每次需要时**
    /// 从注册表现取（与 `RankingSourceRegistration::plugin_endpoint` 同一条
    /// 「活的注册表」纪律），而不是拷进长生命周期对象后当快照用。
    pub plugin_endpoint: String,
}

/// 一个榜单（`RankingBoard`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingBoardRegistration {
    pub board_key: String,
    pub display_name: String,
    /// **静态**周期集合。空数组 = 「只有总榜」。
    pub supported_periods: Vec<String>,
    /// 未指定周期时用哪个。见 proto 的 `RankingBoard.default_period`。
    pub default_period: String,
    /// 周期要不要问 `ResolveRankingPeriods` 才知道（对应上游
    /// `supported_periods_provider is not None`）。
    pub dynamic_periods: bool,
}

/// 一个排行榜来源（`discovery.ranking_source`）。
///
/// # 来源级的名字在协议里**没有**
///
/// 上游 `RankingSourceDefinition.name` 来自 `PluginRankingSource.name`
/// （`source_key="javdb"` / `name="JavDB"`），而 gRPC 的
/// `RankingSourceExtension` **只有** `source_key` 与 `boards`。
///
/// 所以这里存的是**插件自己的** `display_name`（`RegisterResponse.display_name`，
/// 与 [`MetadataSourceRegistration::display_name`] 同一个来源），它是最接近的东西
/// —— 但**不等于**上游那个来源名（插件名是「SakuraMedia JavDB 排行榜」，
/// 来源名是「JavDB」）。要精确对齐得给 proto 加字段，本轮不做：读侧展示用得上
/// 一个名字，而 `source_key` 作为标题太难看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingSourceRegistration {
    /// 全局唯一。
    pub source_key: String,
    /// 插件声明的名字。见上面的说明 —— 是插件名，不是上游那个来源名。
    pub display_name: String,
    pub boards: Vec<RankingBoardRegistration>,
    /// 提供它的插件 id —— 调用时要知道去连哪个插件。
    pub plugin_id: String,
    /// 那个插件的**控制面**端点。与 `ProviderRegistration::plugin_endpoint` 同一个
    /// 东西，理由也一样：扩展点服务跑在插件进程里，没有端点就「查得到声明、
    /// 打不出去」。
    ///
    /// ⚠️ 与端点一样是**活的** —— 插件重启会换端口，所以调用方每次都要从注册表
    /// 现取（见 `Plugins::provider_registry` 的注释）。
    pub plugin_endpoint: String,
}

impl RankingSourceRegistration {
    pub fn board(&self, board_key: &str) -> Option<&RankingBoardRegistration> {
        self.boards.iter().find(|b| b.board_key == board_key)
    }
}

/// 扩展点声明的问题。**可一次报多条。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionProblem {
    /// 声明了扩展点却没声明对应 capability —— 按 proto 它不会被调用。
    MissingCapability {
        plugin_id: String,
        extension: &'static str,
    },
    /// `Extension.key` 对了，但 `data` 不是该扩展点要求的载荷（协议不同步）。
    MissingPayload {
        plugin_id: String,
        extension: &'static str,
    },
    /// `source_key` 为空、形状不合法或超长。上游是
    /// `^[a-z][a-z0-9_]*$` + `max_length=64`。
    InvalidSourceKey {
        plugin_id: String,
        source_key: String,
    },
    /// 榜单 key 的形状同样受约束（上游 `PluginRankingBoard.key`）。
    InvalidBoardKey {
        plugin_id: String,
        source_key: String,
        board_key: String,
    },
    /// 没声明任何榜单 —— 上游 `boards` 至少一项。
    NoBoards {
        plugin_id: String,
        source_key: String,
    },
    /// 同一来源内 `board_key` 重复。
    DuplicateBoardKey {
        plugin_id: String,
        source_key: String,
        board_key: String,
    },
    /// `source_key` 已被占用（本插件或别的插件）。上游据此**整插件拒绝**。
    DuplicateSourceKey {
        plugin_id: String,
        source_key: String,
    },
}

impl ExtensionProblem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingCapability { .. } => "extension_capability_missing",
            Self::MissingPayload { .. } => "extension_payload_missing",
            Self::InvalidSourceKey { .. } => "ranking_source_key_invalid",
            Self::InvalidBoardKey { .. } => "ranking_board_key_invalid",
            Self::NoBoards { .. } => "ranking_source_no_boards",
            Self::DuplicateBoardKey { .. } => "ranking_board_key_duplicated",
            Self::DuplicateSourceKey { .. } => "ranking_source_key_duplicated",
        }
    }
}

/// 扩展点注册表。**顺序 = 注册顺序 = `plugins.enabled` 的顺序。**
///
/// 上游按 `plugins.enabled` 决定多个来源之间的优先级（兜底链路依次尝试），
/// 所以顺序是语义的一部分，不是实现细节。
#[derive(Debug, Clone, Default)]
pub struct ExtensionRegistry {
    /// 元数据来源是插件粒度。
    metadata_order: Vec<String>,
    metadata: HashMap<String, MetadataSourceRegistration>,
    ranking_order: Vec<String>,
    ranking: HashMap<String, RankingSourceRegistration>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 元数据来源，**按 `plugins.enabled` 顺序**。
    ///
    /// 上游 `enabled_plugin_sources()` 就是按这个顺序依次尝试的。
    pub fn metadata_sources(&self) -> Vec<&MetadataSourceRegistration> {
        self.metadata_order
            .iter()
            .filter_map(|key| self.metadata.get(key))
            .collect()
    }

    /// 排行榜来源，**按 `plugins.enabled` 顺序**。
    pub fn ranking_sources(&self) -> Vec<&RankingSourceRegistration> {
        self.ranking_order
            .iter()
            .filter_map(|key| self.ranking.get(key))
            .collect()
    }

    pub fn ranking_source(&self, source_key: &str) -> Option<&RankingSourceRegistration> {
        self.ranking.get(source_key)
    }

    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty() && self.ranking.is_empty()
    }

    /// 清空全部条目，**保留对象本身**。
    ///
    /// 组合根在插件重启后整体重建注册表（增量合并会留下已经不存在的东西）。
    /// 而句柄是共享的（`Arc<Mutex<..>>`，排行榜写侧指着它），所以必须原地清空
    /// 而不是换一个新对象 —— 换掉等于让调用方抱着一份「永远空」的旧表。
    pub fn clear(&mut self) {
        self.metadata_order.clear();
        self.metadata.clear();
        self.ranking_order.clear();
        self.ranking.clear();
    }

    /// 收一个元数据来源。**同 `plugin_id` 后注册者覆盖前者，但不改顺序** ——
    /// 与 [`crate::registry::ProviderRegistry::insert`] 同一个理由：顺序表达
    /// `plugins.enabled` 的优先级，不该被覆盖动作打乱。
    fn insert_metadata(&mut self, entry: MetadataSourceRegistration) {
        let key = entry.plugin_id.clone();
        if !self.metadata.contains_key(&key) {
            self.metadata_order.push(key.clone());
        }
        self.metadata.insert(key, entry);
    }

    fn insert_ranking(&mut self, entry: RankingSourceRegistration) {
        let key = entry.source_key.clone();
        self.ranking_order.push(key.clone());
        self.ranking.insert(key, entry);
    }
}

/// 把注册响应里的两个扩展点收进注册表（`media.provider` 由
/// [`crate::loader::collect_providers`] 收）。
///
/// 返回问题列表；**空**表示这个插件的扩展点全部可用。
pub fn collect_extensions(
    registry: &mut ExtensionRegistry,
    response: &RegisterResponse,
    endpoint: &str,
) -> Vec<ExtensionProblem> {
    let mut problems = Vec::new();
    // 排行榜来源一旦冲突就是整插件拒绝：后面的同 key 扩展点也不再收。
    let mut ranking_rejected = false;

    for extension in &response.extensions {
        match extension.key.as_str() {
            METADATA_SOURCE => {
                collect_metadata_source(registry, response, endpoint, &mut problems)
            }
            RANKING_SOURCE if ranking_rejected => {}
            RANKING_SOURCE => {
                let Some(Data::RankingSource(bundle)) = &extension.data else {
                    problems.push(ExtensionProblem::MissingPayload {
                        plugin_id: response.plugin_id.clone(),
                        extension: RANKING_SOURCE,
                    });
                    continue;
                };
                ranking_rejected =
                    collect_ranking_source(registry, response, bundle, endpoint, &mut problems);
            }
            // `media.provider` 与未知 key：各有各的去处，这里显式忽略。
            _ => {}
        }
    }
    problems
}

fn has_capability(response: &RegisterResponse, value: i32) -> bool {
    response.capabilities.contains(&value)
}

fn collect_metadata_source(
    registry: &mut ExtensionRegistry,
    response: &RegisterResponse,
    endpoint: &str,
    problems: &mut Vec<ExtensionProblem>,
) {
    if !has_capability(response, capability::EXTENSION_CATALOG_METADATA_SOURCE) {
        problems.push(ExtensionProblem::MissingCapability {
            plugin_id: response.plugin_id.clone(),
            extension: METADATA_SOURCE,
        });
        return;
    }
    registry.insert_metadata(MetadataSourceRegistration {
        plugin_id: response.plugin_id.clone(),
        display_name: response.display_name.clone(),
        plugin_endpoint: endpoint.to_owned(),
    });
}

/// 收一个排行榜来源。返回**是否整插件拒绝**（`true` 时本插件其余榜单也不收）。
fn collect_ranking_source(
    registry: &mut ExtensionRegistry,
    response: &RegisterResponse,
    bundle: &RankingSourceExtension,
    endpoint: &str,
    problems: &mut Vec<ExtensionProblem>,
) -> bool {
    let plugin_id = response.plugin_id.clone();

    if !has_capability(response, capability::EXTENSION_RANKING_SOURCE) {
        problems.push(ExtensionProblem::MissingCapability {
            plugin_id,
            extension: RANKING_SOURCE,
        });
        return true;
    }
    if !is_valid_slug(&bundle.source_key) {
        problems.push(ExtensionProblem::InvalidSourceKey {
            plugin_id,
            source_key: bundle.source_key.clone(),
        });
        return true;
    }
    if bundle.boards.is_empty() {
        problems.push(ExtensionProblem::NoBoards {
            plugin_id,
            source_key: bundle.source_key.clone(),
        });
        return true;
    }

    let mut boards = Vec::with_capacity(bundle.boards.len());
    for board in &bundle.boards {
        if !is_valid_slug(&board.board_key) {
            problems.push(ExtensionProblem::InvalidBoardKey {
                plugin_id: plugin_id.clone(),
                source_key: bundle.source_key.clone(),
                board_key: board.board_key.clone(),
            });
            return true;
        }
        if boards
            .iter()
            .any(|b: &RankingBoardRegistration| b.board_key == board.board_key)
        {
            problems.push(ExtensionProblem::DuplicateBoardKey {
                plugin_id: plugin_id.clone(),
                source_key: bundle.source_key.clone(),
                board_key: board.board_key.clone(),
            });
            return true;
        }
        boards.push(RankingBoardRegistration {
            board_key: board.board_key.clone(),
            display_name: board.display_name.clone(),
            supported_periods: board.supported_periods.clone(),
            default_period: board.default_period.clone(),
            dynamic_periods: board.dynamic_periods,
        });
    }

    if registry.ranking.contains_key(&bundle.source_key) {
        problems.push(ExtensionProblem::DuplicateSourceKey {
            plugin_id,
            source_key: bundle.source_key.clone(),
        });
        return true;
    }

    registry.insert_ranking(RankingSourceRegistration {
        source_key: bundle.source_key.clone(),
        display_name: response.display_name.clone(),
        boards,
        plugin_id,
        plugin_endpoint: endpoint.to_owned(),
    });
    false
}

/// 上游对 `source_key` / `board_key` 的形状约束：`^[a-z][a-z0-9_]*$`，≤64。
///
/// 不引 `regex`：一条字符判定就够，而这类 key 会进 URL 与配置键，形状必须
/// 在加载期拦住 —— 上游是 pydantic 的 `Field(pattern=...)`,同一个意思。
fn is_valid_slug(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && key.chars().count() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_plugin_api::v1::{Extension, MetadataSourceExtension, RankingBoard};

    fn response(
        plugin_id: &str,
        capabilities: Vec<i32>,
        extensions: Vec<Extension>,
    ) -> RegisterResponse {
        RegisterResponse {
            plugin_id: plugin_id.to_owned(),
            display_name: format!("{plugin_id} 插件"),
            abi_major: sm_plugin_api::ABI_MAJOR,
            capabilities,
            extensions,
            ..Default::default()
        }
    }

    /// 测试用的插件控制面端点。断言「端点真的被存下来了」要用它。
    const ENDPOINT: &str = "http://127.0.0.1:50051";

    /// `collect_extensions` 的薄包装：这些用例都只关心「收了什么」，
    /// 不关心端点，所以固定传一个。
    fn collect(
        registry: &mut ExtensionRegistry,
        response: &RegisterResponse,
    ) -> Vec<ExtensionProblem> {
        collect_extensions(registry, response, ENDPOINT)
    }

    fn ranking_extension(source_key: &str, boards: &[(&str, &str)]) -> Extension {
        Extension {
            key: RANKING_SOURCE.to_owned(),
            data: Some(Data::RankingSource(RankingSourceExtension {
                source_key: source_key.to_owned(),
                boards: boards
                    .iter()
                    .map(|(key, name)| RankingBoard {
                        board_key: (*key).to_owned(),
                        display_name: (*name).to_owned(),
                        ..Default::default()
                    })
                    .collect(),
            })),
        }
    }

    fn metadata_extension() -> Extension {
        Extension {
            key: METADATA_SOURCE.to_owned(),
            data: Some(Data::MetadataSource(MetadataSourceExtension::default())),
        }
    }

    #[test]
    fn a_metadata_source_is_registered_with_its_plugin_display_name() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "javdb",
                vec![capability::EXTENSION_CATALOG_METADATA_SOURCE],
                vec![metadata_extension()],
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");

        let sources = registry.metadata_sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].plugin_id, "javdb");
        assert_eq!(sources[0].display_name, "javdb 插件");
        assert_eq!(
            sources[0].plugin_endpoint, ENDPOINT,
            "端点要存下来 —— 与排行源同一个理由（否则调用面打不出去）"
        );
        assert!(registry.ranking_sources().is_empty());
    }

    #[test]
    fn a_metadata_source_without_the_capability_is_not_collected() {
        // proto 的原话：「仅在声明对应 capability 时才会被调用」——
        // 收进来也是一个永远不会被调到条目。
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response("javdb", vec![], vec![metadata_extension()]),
        );
        assert_eq!(
            problems,
            vec![ExtensionProblem::MissingCapability {
                plugin_id: "javdb".to_owned(),
                extension: METADATA_SOURCE
            }]
        );
        assert!(registry.is_empty());
        assert_eq!(problems[0].code(), "extension_capability_missing");
    }

    #[test]
    fn ranking_boards_are_kept_in_order_and_reachable_by_key() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "weekly",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![ranking_extension(
                    "dmm",
                    &[("daily", "每日"), ("weekly", "每周")],
                )],
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");

        let source = registry.ranking_source("dmm").expect("应当收进去");
        assert_eq!(source.plugin_id, "weekly", "要记得去连哪个插件");
        assert_eq!(source.boards.len(), 2);
        assert_eq!(source.board("weekly").unwrap().display_name, "每周");
        assert!(source.board("monthly").is_none());
    }

    /// ★ 榜单定义要**整份**存下来（不只是 key 与名字），端点也要。
    ///
    /// 这条挡的是两种「接口成功但没数据」的退化：榜单定义丢了 → 读侧
    /// `list_boards` 只能报缺口；端点丢了 → 同步时「查得到声明、打不出去」。
    #[test]
    fn a_board_definition_carries_its_periods_and_the_live_endpoint() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "weekly",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![Extension {
                    key: RANKING_SOURCE.to_owned(),
                    data: Some(Data::RankingSource(RankingSourceExtension {
                        source_key: "dmm".to_owned(),
                        boards: vec![
                            RankingBoard {
                                board_key: "playback_all".to_owned(),
                                display_name: "热播".to_owned(),
                                supported_periods: vec!["daily".to_owned(), "weekly".to_owned()],
                                default_period: "daily".to_owned(),
                                dynamic_periods: false,
                            },
                            RankingBoard {
                                board_key: "top250".to_owned(),
                                display_name: "TOP250".to_owned(),
                                supported_periods: vec![],
                                default_period: "all".to_owned(),
                                dynamic_periods: true,
                            },
                        ],
                    })),
                }],
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");

        let source = registry.ranking_source("dmm").expect("应当收进去");
        assert_eq!(
            source.plugin_endpoint, ENDPOINT,
            "端点要存下来 —— 与 provider 那边同一个理由（否则打不出去）"
        );

        let playback = source.board("playback_all").expect("静态周期的榜单");
        assert_eq!(playback.supported_periods, ["daily", "weekly"]);
        assert_eq!(playback.default_period, "daily");
        assert!(!playback.dynamic_periods);

        let top250 = source.board("top250").expect("动态周期的榜单");
        assert!(
            top250.supported_periods.is_empty(),
            "动态周期**不在**载荷里（它随年份滚动）"
        );
        assert_eq!(top250.default_period, "all", "代表值仍要给");
        assert!(
            top250.dynamic_periods,
            "TOP250 的周期要问 ResolveRankingPeriods 才知道"
        );
    }

    #[test]
    fn source_keys_must_be_slugs() {
        // 上游 `^[a-z][a-z0-9_]*$`；这类 key 会进 URL 与配置键，形状不对
        // 必须拦在加载期。
        let mut registry = ExtensionRegistry::new();
        for bad in ["", "DMM", "dmm-ranking", "1st", "dmm ranking"] {
            let problems = collect(
                &mut registry,
                &response(
                    "p",
                    vec![capability::EXTENSION_RANKING_SOURCE],
                    vec![ranking_extension(bad, &[("daily", "每日")])],
                ),
            );
            assert_eq!(
                problems,
                vec![ExtensionProblem::InvalidSourceKey {
                    plugin_id: "p".to_owned(),
                    source_key: bad.to_owned()
                }],
                "{bad:?} 应当不合法"
            );
        }
        assert!(registry.is_empty());
    }

    #[test]
    fn a_source_without_boards_is_rejected() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "p",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![ranking_extension("dmm", &[])],
            ),
        );
        assert_eq!(
            problems,
            vec![ExtensionProblem::NoBoards {
                plugin_id: "p".to_owned(),
                source_key: "dmm".to_owned()
            }]
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn duplicate_board_keys_inside_one_source_are_rejected() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "p",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![ranking_extension(
                    "dmm",
                    &[("daily", "每日"), ("daily", "每日榜")],
                )],
            ),
        );
        assert_eq!(
            problems,
            vec![ExtensionProblem::DuplicateBoardKey {
                plugin_id: "p".to_owned(),
                source_key: "dmm".to_owned(),
                board_key: "daily".to_owned()
            }]
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn a_source_key_conflict_rejects_the_whole_plugin() {
        // 上游 `apply_plugin_ranking_sources`：冲突时整插件拒绝，于是它后面
        // 那个（本来合法的）来源也不该进来。
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "first",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![ranking_extension("dmm", &[("daily", "每日")])],
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");

        let problems = collect(
            &mut registry,
            &response(
                "second",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![
                    ranking_extension("dmm", &[("weekly", "每周")]),
                    ranking_extension("javbus", &[("daily", "每日")]),
                ],
            ),
        );
        assert_eq!(
            problems,
            vec![ExtensionProblem::DuplicateSourceKey {
                plugin_id: "second".to_owned(),
                source_key: "dmm".to_owned()
            }]
        );
        // 被拒插件的第二个来源也没进来 —— 否则「整插件拒绝」就是空话。
        assert!(registry.ranking_source("javbus").is_none());
        assert_eq!(registry.ranking_sources().len(), 1);
        assert_eq!(registry.ranking_source("dmm").unwrap().plugin_id, "first");
    }

    #[test]
    fn re_registering_a_metadata_source_overwrites_without_reordering() {
        // 顺序表达 `plugins.enabled` 的优先级，覆盖不该打乱它。
        let mut registry = ExtensionRegistry::new();
        for name in ["第一个", "第二个"] {
            let mut response = response(
                "javdb",
                vec![capability::EXTENSION_CATALOG_METADATA_SOURCE],
                vec![metadata_extension()],
            );
            response.display_name = name.to_owned();
            let problems = collect(&mut registry, &response);
            assert!(problems.is_empty(), "{problems:?}");
        }
        let sources = registry.metadata_sources();
        assert_eq!(sources.len(), 1, "同插件只占一个位置");
        assert_eq!(sources[0].display_name, "第二个");
    }

    #[test]
    fn unknown_extension_keys_and_other_points_are_ignored() {
        // `media.provider` 归 loader 收；不认识的 key 显式忽略，不报错 ——
        // 宿主比插件旧时这是常态。
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "p",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![
                    Extension {
                        key: MEDIA_PROVIDER.to_owned(),
                        data: None,
                    },
                    Extension {
                        key: "future.point".to_owned(),
                        data: None,
                    },
                ],
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert!(registry.is_empty());
    }

    #[test]
    fn a_ranking_extension_without_its_payload_is_reported() {
        let mut registry = ExtensionRegistry::new();
        let problems = collect(
            &mut registry,
            &response(
                "p",
                vec![capability::EXTENSION_RANKING_SOURCE],
                vec![Extension {
                    key: RANKING_SOURCE.to_owned(),
                    data: None,
                }],
            ),
        );
        assert_eq!(
            problems,
            vec![ExtensionProblem::MissingPayload {
                plugin_id: "p".to_owned(),
                extension: RANKING_SOURCE
            }]
        );
        assert_eq!(problems[0].code(), "extension_payload_missing");
    }

    #[test]
    fn sources_are_returned_in_registration_order() {
        // 兜底链路（JavDB 未收录时依次尝试）靠这个顺序决定先问谁。
        let mut registry = ExtensionRegistry::new();
        for (plugin, source) in [("b_plugin", "b"), ("a_plugin", "a")] {
            let problems = collect(
                &mut registry,
                &response(
                    plugin,
                    vec![
                        capability::EXTENSION_RANKING_SOURCE,
                        capability::EXTENSION_CATALOG_METADATA_SOURCE,
                    ],
                    vec![
                        ranking_extension(source, &[("daily", "每日")]),
                        metadata_extension(),
                    ],
                ),
            );
            assert!(problems.is_empty(), "{problems:?}");
        }
        let ranking: Vec<&str> = registry
            .ranking_sources()
            .iter()
            .map(|s| s.source_key.as_str())
            .collect();
        assert_eq!(ranking, vec!["b", "a"]);
        let metadata: Vec<&str> = registry
            .metadata_sources()
            .iter()
            .map(|s| s.plugin_id.as_str())
            .collect();
        assert_eq!(metadata, vec!["b_plugin", "a_plugin"]);
    }
}
