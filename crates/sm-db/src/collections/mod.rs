//! `collections` 域模型（6 张表）。
//!
//! 对应 `src/model/collections/` 的 `playlists.py` / `moments.py` / `clips.py`。
//!
//! # 三种合集同构，但有三处关键差异
//!
//! | | `Playlist` | `MomentCollection` | `ClipCollection` |
//! |---|---|---|---|
//! | 成员表 | `PlaylistMovie` | `MomentCollectionItem` | `ClipCollectionItem` |
//! | 成员指向 | `Movie`（JAV 影片） | `MediaPoint`（时刻点） | `MediaClip`（片段） |
//! | **`position`** | **无** | 有 | 有 |
//! | **`kind`** | **有** | 无 | 无 |
//!
//! **① `PlaylistMovie` 没有 `position`** —— JAV 侧播放顺序只能靠加入先后，
//! 视频侧与时刻/片段侧都显式维护 `position`。迁移时不能当成同一张表。
//!
//! **② 只有 `Playlist` 有 `kind`** —— 区分用户列表与系统维护列表。
//!
//! **③ 三者都有 `(owner_plugin_id, plugin_key)` 唯一索引** —— 插件用它
//! 稳定 key 管理自己的资源；宿主/用户创建的列表这两列保持 NULL。
//! NULL 不参与唯一约束（SQL 标准），所以该索引的作用是防止**同一个插件**
//! 重复注册同一个 key，而不是保证 `name` 唯一（`name` 自身带 unique）。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// 合集共有的「插件归属」语义。
///
/// 三个合集表都有这两列且索引相同，抽成 trait 供泛型仓储层复用。
pub trait PluginOwned {
    /// 创建该合集的插件 ID；宿主或用户创建时为 `NULL`。
    fn owner_plugin_id(&self) -> Option<&str>;

    /// 插件自定义的稳定 key。
    fn plugin_key(&self) -> Option<&str>;

    /// 是否由插件管理。
    ///
    /// 两列都非空才算 —— 只有其一时视为半配置状态。
    fn is_plugin_owned(&self) -> bool {
        self.owner_plugin_id().is_some() && self.plugin_key().is_some()
    }
}

/// 生成三个结构相同的合集表模型。
///
/// 第三个参数是**花括号包裹的完整字段块**（只有 `Playlist` 用得上 `kind`）。
/// 不能写成 `$(, $extra:tt)*` —— 那样每个额外字段前都得再加一个逗号，
/// 而 `pub kind: String` 本身是多个 token。
macro_rules! owned_collection {
    ($name:ident, $doc:expr, { $($extra:tt)* }) => {
        #[doc = $doc]
        #[derive(Debug, Clone, FromRow)]
        pub struct $name {
            pub id: i64,
            /// 全局唯一。
            pub name: String,
            pub description: String,
            /// 插件列表用稳定 key 管理自己的资源；宿主/用户创建的保持 NULL。
            pub owner_plugin_id: Option<String>,
            pub plugin_key: Option<String>,
            $($extra)*
            pub created_at: Option<NaiveDateTime>,
            pub updated_at: Option<NaiveDateTime>,
        }

        impl PluginOwned for $name {
            fn owner_plugin_id(&self) -> Option<&str> {
                self.owner_plugin_id.as_deref()
            }
            fn plugin_key(&self) -> Option<&str> {
                self.plugin_key.as_deref()
            }
        }
    };
}

owned_collection!(
    Playlist,
    "JAV 播放列表。唯一多一个 `kind` 字段与系统列表语义。",
    {
        /// 区分用户列表与系统维护列表。数据库无 CHECK 约束。
        pub kind: String,
    }
);

owned_collection!(
    MomentCollection,
    "时刻合集：用户整理的一组有序媒体时刻。",
    {}
);

owned_collection!(
    ClipCollection,
    "片段合集：跨影片的有序片段集合，可连续播放。",
    {}
);

impl Playlist {
    /// 对应 `Playlist.kind` 的数据库默认值。
    pub fn default_kind() -> &'static str {
        PLAYLIST_KIND_CUSTOM
    }

    /// 该列表是否由系统维护（用户不应手动增删成员）。
    pub fn is_system(&self) -> bool {
        is_system_playlist_kind(&self.kind)
    }

    /// 校验 kind 合法。
    ///
    /// 数据库列没有 CHECK 约束，所以脏数据只能靠这里挡住。
    pub fn is_valid_kind(kind: &str) -> bool {
        kind == PLAYLIST_KIND_CUSTOM || SYSTEM_PLAYLIST_KINDS.contains(&kind)
    }
}

/// 用户自建列表的 kind。
pub const PLAYLIST_KIND_CUSTOM: &str = "custom";

/// 系统自动维护的最近播放列表。
pub const PLAYLIST_KIND_RECENTLY_PLAYED: &str = "recently_played";

/// 系统列表的固定名称。
pub const RECENTLY_PLAYED_PLAYLIST_NAME: &str = "最近播放";

/// 系统列表的固定描述。
pub const RECENTLY_PLAYED_PLAYLIST_DESCRIPTION: &str = "系统自动维护的最近播放影片列表";

/// 系统播放列表的 kind 集合。
///
/// service / schema 判定「系统列表」的唯一真相源。
pub const SYSTEM_PLAYLIST_KINDS: [&str; 1] = [PLAYLIST_KIND_RECENTLY_PLAYED];

/// 该 kind 是否为系统列表。
pub fn is_system_playlist_kind(kind: &str) -> bool {
    SYSTEM_PLAYLIST_KINDS.contains(&kind)
}

/// `playlist_movie` 表。
///
/// **本表没有 `position` 字段** —— 唯一索引是 `(playlist, movie)`。
#[derive(Debug, Clone, FromRow)]
pub struct PlaylistMovie {
    pub id: i64,
    pub playlist_id: i64,
    /// 指向 `Movie`（JAV 影片）。注意 `Movie` 有 `movie_number` 字段，
    /// 但这个外键指向的是它的 `id`。
    pub movie_id: i64,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl PlaylistMovie {
    /// 播放顺序键。
    ///
    /// **本表没有 `position`**，顺序只能靠 `id` —— 即加入播放列表的先后。
    /// 这与 `MomentCollectionItem` / `ClipCollectionItem` 不同。
    pub fn playback_order_key(&self) -> i64 {
        self.id
    }
}

/// 生成两个结构相同的合集成员表模型。
macro_rules! ordered_collection_item {
    ($name:ident, $field:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, FromRow)]
        pub struct $name {
            pub id: i64,
            pub collection_id: i64,
            pub $field: i64,
            /// 显式播放顺序。
            pub position: i32,
            pub created_at: Option<NaiveDateTime>,
            pub updated_at: Option<NaiveDateTime>,
        }

        impl $name {
            /// 播放顺序键：`position` 升序，同位按 `id` 升序。
            ///
            /// `id` 作为次级键是必要的 —— 删除后重排会让多条成员 `position`
            /// 相同，只按 `position` 排序时顺序不确定，会导致播放列表抖动。
            pub fn playback_order_key(&self) -> (i32, i64) {
                (self.position, self.id)
            }
        }
    };
}

ordered_collection_item!(
    MomentCollectionItem,
    point_id,
    "时刻合集成员。指向 `MediaPoint`，时刻删除后自动移出合集。"
);

ordered_collection_item!(
    ClipCollectionItem,
    clip_id,
    "片段合集成员。指向 `MediaClip`，片段删除后自动移出所有合集。"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(kind: &str, owner: Option<&str>, key: Option<&str>) -> Playlist {
        Playlist {
            id: 1,
            name: "n".to_owned(),
            description: String::new(),
            owner_plugin_id: owner.map(str::to_owned),
            plugin_key: key.map(str::to_owned),
            kind: kind.to_owned(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn system_playlist_kinds_match_backend() {
        assert_eq!(PLAYLIST_KIND_CUSTOM, "custom");
        assert_eq!(PLAYLIST_KIND_RECENTLY_PLAYED, "recently_played");
        assert_eq!(RECENTLY_PLAYED_PLAYLIST_NAME, "最近播放");
        assert_eq!(
            RECENTLY_PLAYED_PLAYLIST_DESCRIPTION,
            "系统自动维护的最近播放影片列表"
        );
        assert_eq!(SYSTEM_PLAYLIST_KINDS.len(), 1);
    }

    #[test]
    fn only_recently_played_counts_as_system() {
        assert!(is_system_playlist_kind(PLAYLIST_KIND_RECENTLY_PLAYED));
        assert!(
            !is_system_playlist_kind(PLAYLIST_KIND_CUSTOM),
            "用户列表不是系统列表"
        );
        assert!(playlist("custom", None, None).is_system() == false);
        assert!(playlist("recently_played", None, None).is_system());
    }

    #[test]
    fn kind_validation_matches_python_literals() {
        assert!(Playlist::is_valid_kind("custom"));
        assert!(Playlist::is_valid_kind("recently_played"));
        assert!(
            !Playlist::is_valid_kind("whatever"),
            "数据库无 CHECK 约束，必须由代码挡住脏值"
        );
        assert_eq!(Playlist::default_kind(), PLAYLIST_KIND_CUSTOM);
    }

    #[test]
    fn plugin_owned_requires_both_columns() {
        // NULL 不参与唯一约束，(owner, key) 的作用是阻止同一插件重复注册同一 key。
        assert!(playlist("custom", Some("actor-metadata"), Some("k")).is_plugin_owned());
        assert!(
            !playlist("custom", Some("actor-metadata"), None).is_plugin_owned(),
            "半配置状态不算插件所有"
        );
        assert!(!playlist("custom", None, None).is_plugin_owned());
    }

    #[test]
    fn all_three_collections_share_plugin_owned_semantics() {
        let moment = MomentCollection {
            id: 1,
            name: "m".to_owned(),
            description: String::new(),
            owner_plugin_id: Some("p".to_owned()),
            plugin_key: Some("k".to_owned()),
            created_at: None,
            updated_at: None,
        };
        let clip = ClipCollection {
            id: 1,
            name: "c".to_owned(),
            description: String::new(),
            owner_plugin_id: None,
            plugin_key: None,
            created_at: None,
            updated_at: None,
        };
        assert!(moment.is_plugin_owned());
        assert!(!clip.is_plugin_owned());
        assert_eq!(
            moment.plugin_key(),
            Some("k"),
            "宏生成的三个类型共享同一套判定"
        );
    }

    #[test]
    fn playlist_movie_has_no_position_and_orders_by_id() {
        // 这是与另两张成员表最关键的差异：JAV 播放列表靠加入先后排序。
        let early = PlaylistMovie {
            id: 7,
            playlist_id: 1,
            movie_id: 100,
            created_at: None,
            updated_at: None,
        };
        let late = PlaylistMovie {
            id: 9,
            ..early.clone()
        };
        assert!(early.playback_order_key() < late.playback_order_key());
    }

    #[test]
    fn ordered_items_sort_by_position_then_id() {
        let a = MomentCollectionItem {
            id: 5,
            collection_id: 1,
            point_id: 50,
            position: 1,
            created_at: None,
            updated_at: None,
        };
        let same_pos_later_id = MomentCollectionItem {
            id: 6,
            ..a.clone()
        };
        let next_pos = MomentCollectionItem {
            id: 2,
            position: 2,
            ..a.clone()
        };

        assert!(a.playback_order_key() < same_pos_later_id.playback_order_key());
        assert!(
            same_pos_later_id.playback_order_key() < next_pos.playback_order_key(),
            "同位时按 id 兜底，避免删除重排后顺序抖动"
        );
    }
}
