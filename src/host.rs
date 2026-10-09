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
        Ok(Page { movies, next_cursor: resp.next_cursor })
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
    values.get(key).and_then(|v| match &v.kind {
        Some(prost_types::value::Kind::NumberValue(n)) if *n >= 0.0 => Some(*n as u64),
        _ => None,
    }).unwrap_or(0)
}

fn value_as_string(values: &HashMap<String, PbValue>, key: &str) -> String {
    values.get(key).and_then(|v| match &v.kind {
        Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
        _ => None,
    }).unwrap_or_default()
}

fn value_as_bool(values: &HashMap<String, PbValue>, key: &str) -> bool {
    values.get(key).and_then(|v| match &v.kind {
        Some(prost_types::value::Kind::BoolValue(b)) => Some(*b),
        _ => None,
    }).unwrap_or(false)
}

/// 从 `owners: ["<field>=<owner>", ...]` 里取 `is_collection` 的归属。
/// 也接受 `"<field>:<owner>"` 写法（取第一个 `=` 或 `:` 切分）。
pub fn collection_owner_of(owners: &[String]) -> Option<String> {
    owners.iter().find_map(|entry| {
        let (field, owner) = entry
            .split_once('=')
            .or_else(|| entry.split_once(':'))?;
        if field.trim() == "is_collection" {
            Some(owner.trim().to_owned())
        } else {
            None
        }
    })
}

/// `MovieSnapshot` → 判定输入。
pub fn movie_input_from(snapshot: MovieSnapshot) -> MovieInput {
    MovieInput {
        movie_id: snapshot.movie_id,
        revision: snapshot.revision,
        duration_minutes: value_as_u64(&snapshot.values, "duration_minutes"),
        movie_number: value_as_string(&snapshot.values, "movie_number"),
        is_collection: value_as_bool(&snapshot.values, "is_collection"),
        collection_owner: collection_owner_of(&snapshot.owners),
        tag_names: snapshot.tags.into_iter().map(|t| t.name).collect(),
    }
}
