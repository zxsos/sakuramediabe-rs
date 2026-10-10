//! 宿主调用面：`PluginHost` 的 `ListMovies` / `PatchMovie`。
//!
//! 上游是进程内插件，直接调 `context.movies.list_page` /
//! `context.movies.patch`。这里走 gRPC，由 [`HostMovies`] 抽象：
//! - 真实实现：[`GrpcHostMovies`]（tonic 客户端）；
//! - 测试实现：`tests/` 里的假实现。
//!
//! # 分页
//!
//! 与上游一致：`after_id` 游标、`PAGE_SIZE = 500`、
//! `next_cursor` 为空即结束。

use std::collections::HashMap;

use async_trait::async_trait;
use prost_types::Value as PbValue;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::{ListMoviesRequest, ListMoviesResponse, MovieSnapshot, PatchMovieRequest};
use tonic::transport::Channel;

use crate::judge::MovieInput;

/// 上游 `PAGE_SIZE = 500`。
pub const PAGE_SIZE: i32 = 500;

/// 宿主影片读写面的最小抽象。
#[async_trait]
pub trait HostMovies: Send + Sync {
    async fn list_page(
        &self,
        after_id: i64,
        limit: i32,
    ) -> Result<Page, Box<dyn std::error::Error + Send + Sync>>;
    async fn patch_is_collection(
        &self,
        movie_id: i64,
        expected_revision: i64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

/// 一页影片。
pub struct Page {
    pub movies: Vec<MovieInput>,
    pub next_cursor: Option<i64>,
}

/// tonic 实现的宿主调用。
pub struct GrpcHostMovies {
    client: PluginHostClient<Channel>,
}

impl GrpcHostMovies {
    pub async fn connect(addr: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = if addr.starts_with("http") {
            addr.to_owned()
        } else {
            format!("http://{addr}")
        };
        let client = PluginHostClient::connect(endpoint).await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl HostMovies for GrpcHostMovies {
    async fn list_page(
        &self,
        after_id: i64,
        limit: i32,
    ) -> Result<Page, Box<dyn std::error::Error + Send + Sync>> {
        let mut client = self.client.clone();
        let resp: ListMoviesResponse = client
            .list_movies(ListMoviesRequest {
                after_id,
                limit,
                filters: None,
            })
            .await?
            .into_inner();
        let movies = resp.movies.into_iter().map(movie_input_from).collect();
        Ok(Page {
            movies,
            next_cursor: resp.next_cursor,
        })
    }

    async fn patch_is_collection(
        &self,
        movie_id: i64,
        expected_revision: i64,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut client = self.client.clone();
        let mut fields = HashMap::new();
        fields.insert(
            "is_collection".to_owned(),
            PbValue {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        let resp = client
            .patch_movie(PatchMovieRequest {
                movie_id,
                fields,
                expected_revision,
            })
            .await?
            .into_inner();
        Ok(resp.updated)
    }
}

fn value_as_u64(values: &HashMap<String, PbValue>, key: &str) -> u64 {
    values
        .get(key)
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::NumberValue(n)) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        })
        .unwrap_or(0)
}

fn value_as_string(values: &HashMap<String, PbValue>, key: &str) -> String {
    values
        .get(key)
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn value_as_bool(values: &HashMap<String, PbValue>, key: &str) -> bool {
    values
        .get(key)
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::BoolValue(b)) => Some(*b),
            _ => None,
        })
        .unwrap_or(false)
}

/// 从 `MovieSnapshot.field_owners`（`{字段: owner}`）里取 `is_collection`
/// 的归属。**无主的字段不在 map 里** —— 那正是上游的 `None`。
///
/// 空串照上游的真值判定当作无主（`if owner and owner != plugin_owner`）。
pub fn collection_owner_of(field_owners: &HashMap<String, String>) -> Option<String> {
    field_owners
        .get("is_collection")
        .filter(|owner| !owner.is_empty())
        .cloned()
}

/// `MovieSnapshot` → 判定输入。
pub fn movie_input_from(snapshot: MovieSnapshot) -> MovieInput {
    MovieInput {
        movie_id: snapshot.movie_id,
        revision: snapshot.revision,
        duration_minutes: value_as_u64(&snapshot.values, "duration_minutes"),
        movie_number: value_as_string(&snapshot.values, "movie_number"),
        is_collection: value_as_bool(&snapshot.values, "is_collection"),
        collection_owner: collection_owner_of(&snapshot.field_owners),
        tag_names: snapshot.tags.into_iter().map(|t| t.name).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::value::Kind;
    use sm_plugin_api::v1::TagSnapshot;

    fn pb_string(s: &str) -> PbValue {
        PbValue {
            kind: Some(Kind::StringValue(s.to_owned())),
        }
    }

    #[test]
    fn a_missing_field_owner_is_nobody() {
        assert_eq!(collection_owner_of(&HashMap::new()), None);
        // 别的字段有主，不等于 `is_collection` 有主。
        let mut owners = HashMap::new();
        owners.insert("title".to_owned(), "plugin:x".to_owned());
        assert_eq!(collection_owner_of(&owners), None);
    }

    #[test]
    fn a_recorded_field_owner_is_returned_verbatim() {
        let mut owners = HashMap::new();
        owners.insert("is_collection".to_owned(), "host:manual".to_owned());
        assert_eq!(collection_owner_of(&owners), Some("host:manual".to_owned()));
    }

    #[test]
    fn an_empty_owner_counts_as_nobody() {
        let mut owners = HashMap::new();
        owners.insert("is_collection".to_owned(), String::new());
        assert_eq!(collection_owner_of(&owners), None);
    }

    #[test]
    fn a_snapshot_maps_to_a_judge_input() {
        let mut values = HashMap::new();
        values.insert("duration_minutes".to_owned(), PbValue {
            kind: Some(Kind::NumberValue(95.0)),
        });
        values.insert("movie_number".to_owned(), pb_string("abp_001"));
        values.insert("is_collection".to_owned(), PbValue {
            kind: Some(Kind::BoolValue(false)),
        });
        let mut field_owners = HashMap::new();
        field_owners.insert("is_collection".to_owned(), "host:manual".to_owned());
        let input = movie_input_from(MovieSnapshot {
            movie_id: 9,
            revision: 4,
            values,
            owners: vec!["someone".to_owned()],
            actors: Vec::new(),
            tags: vec![TagSnapshot {
                tag_id: 1,
                name: "独占".to_owned(),
            }],
            field_owners,
        });
        assert_eq!(input.movie_id, 9);
        assert_eq!(input.revision, 4);
        assert_eq!(input.duration_minutes, 95);
        assert_eq!(input.movie_number, "abp_001");
        assert!(!input.is_collection);
        assert_eq!(input.collection_owner, Some("host:manual".to_owned()));
        assert_eq!(input.tag_names, vec!["独占".to_owned()]);
    }
}
