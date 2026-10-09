//! 元数据交付校验：插件把图片放进宿主给的临时目录，宿主接管前先验一遍。
//!
//! # 上游对应
//!
//! `MetadataSourceService._load_plugin` / `_delivery_paths` / `_cleanup_delivery`
//! （`src/service/catalog/metadata_source_service.py`）。上游把这几步做成
//! `@contextmanager`，消费完就删；这里拆成**校验**与**清理**两个显式动作，
//! 因为 Rust 没有 `with` 语法糖，而「谁负责删」必须写在调用点。
//!
//! # 为什么这条规则住在**契约仓**而不是宿主实现里
//!
//! 插件把图片放进目录、宿主去验收 —— **两边都要遵守同一套规则**。放在
//! `sm-plugins`（宿主实现）里，插件作者就只能读文档照抄，而照抄总会漂移；
//! 放在契约仓，作者可以在自己的测试里 `use sm_plugin_api::movie_delivery::*`
//! 直接验一遍自己交出来的东西。
//!
//! 判据是 **proto 给的**，不是自定的。
//! `FetchMovieRequest.delivery_dir` 的注释：「宿主为本次请求分配的临时目录；
//! **元数据图片必须落在其中**」。所以边界就是宿主自己给的那个目录 —— 不需要
//! `plugins.root_dir` 之类的额外配置（上游用
//! `<root>/<plugin_id>/data/metadata-tmp`，那是进程内插件才需要的约定）。
//!
//! # 逐条校验（上游 `_delivery_paths` + `PluginMovieMetadata`）
//!
//! - 图片必须在 `delivery_dir` 内 —— 挡 `../` 穿透；
//! - 必须是**普通文件**（目录 / 不存在 / 别的类型都不行）；
//! - 必须再深一层：`<delivery_dir>/<请求目录>/<文件>`，上游要求
//!   `len(relative.parts) >= 2`；
//! - 同一结果的图片必须在**同一个**请求目录下；
//! - `release_date` 严格 `YYYY-MM-DD`（proto 写在字段上的原话）；
//! - `duration_minutes > 0`（上游 `Field(gt=0)`）。
//!
//! # 番号一致性**不在**本模块
//!
//! 上游还判了「插件返回的番号与请求的是同一个」。那条规则住在
//! `sm_service::movie_numbers::normalize_movie_number`，而 `sm-plugins` 不依赖
//! `sm-service` —— 也不该为了一次比较去依赖它：将来 service 层要回调插件时，
//! 那条边会成环。所以这里**原样返回**番号，由导入方去比。

use std::path::{Path, PathBuf};

use crate::v1::{FetchMovieRequest, FetchMovieResponse, MetadataActor};

/// 一个通过交付校验的元数据结果。
#[derive(Debug, Clone, PartialEq)]
pub struct MovieDelivery {
    /// 插件返回的番号。**未归一化** —— 与请求是否同一个片子由导入方判（见模块文档）。
    pub movie_number: String,
    pub title: String,
    /// 严格 `YYYY-MM-DD`，已校验。
    pub release_date: String,
    pub duration_minutes: i32,
    /// 封面（已确认在交付目录内且是普通文件）。
    pub cover_image_path: PathBuf,
    pub plot_image_paths: Vec<PathBuf>,
    pub summary: String,
    pub maker_name: Option<String>,
    pub director_name: Option<String>,
    pub series_name: Option<String>,
    pub actors: Vec<MetadataActor>,
    pub tag_names: Vec<String>,
    pub source_url: Option<String>,
    pub source_id: Option<String>,
}

impl MovieDelivery {
    /// 封面 + 全部剧情图。清理时用得上。
    pub fn image_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.cover_image_path.clone()];
        paths.extend(self.plot_image_paths.iter().cloned());
        paths
    }
}

/// 交付不合规的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryProblem {
    /// 宿主给的交付目录本身不可用（不存在 / 不是目录）—— 那是宿主的用法错。
    BadDeliveryDir { path: PathBuf },
    /// 图片跑到交付目录外面去了（`../` 穿透）。
    OutsideDeliveryDir { path: PathBuf },
    /// 不是普通文件：目录、不存在、或别的类型。
    NotAFile { path: PathBuf },
    /// 图片直接躺在交付目录根下，缺了「请求目录」那一层。
    MissingRequestDir { path: PathBuf },
    /// 同一结果的图片来自不同的请求目录。
    MixedRequestDirs { first: String, other: String },
    /// 没给封面路径（上游 `cover_image_path` 是必填）。
    MissingCover,
    /// `release_date` 不是严格的 `YYYY-MM-DD`。
    BadReleaseDate { value: String },
    /// `duration_minutes` 非正。
    BadDuration { value: i32 },
}

impl DeliveryProblem {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadDeliveryDir { .. } => "movie_delivery_dir_unusable",
            Self::OutsideDeliveryDir { .. } => "movie_delivery_path_escape",
            Self::NotAFile { .. } => "movie_delivery_not_a_file",
            Self::MissingRequestDir { .. } => "movie_delivery_missing_request_dir",
            Self::MixedRequestDirs { .. } => "movie_delivery_mixed_request_dirs",
            Self::MissingCover => "movie_delivery_missing_cover",
            Self::BadReleaseDate { .. } => "movie_delivery_bad_release_date",
            Self::BadDuration { .. } => "movie_delivery_bad_duration",
        }
    }
}

/// 校验一次 `FetchMovie` 的交付结果。
///
/// `request.delivery_dir` 是边界：图片必须落在它里面。返回的结构里所有路径
/// 都已确认在边界内且是普通文件，调用方可以放心读（但不能假设它们还在 ——
/// 用完要 [`cleanup_delivery`]）。
pub fn validate_movie_delivery(
    request: &FetchMovieRequest,
    response: FetchMovieResponse,
) -> Result<MovieDelivery, DeliveryProblem> {
    let root = PathBuf::from(&request.delivery_dir)
        .canonicalize()
        .map_err(|_| DeliveryProblem::BadDeliveryDir {
            path: PathBuf::from(&request.delivery_dir),
        })?;

    if response.cover_image_path.trim().is_empty() {
        return Err(DeliveryProblem::MissingCover);
    }
    validate_duration(response.duration_minutes)?;
    let release_date = validate_release_date(&response.release_date)?;

    let cover_image_path = delivered_image(&root, &response.cover_image_path)?;
    // 上游要求同一结果的图片来自同一请求目录：`_delivery_paths` 里记下第一个
    // 目录名，之后每一张都要与它一致。
    let request_dir = request_dir_name(&root, &cover_image_path)?;

    let mut plot_image_paths = Vec::with_capacity(response.plot_image_paths.len());
    for raw in &response.plot_image_paths {
        let path = delivered_image(&root, raw)?;
        let dir = request_dir_name(&root, &path)?;
        if dir != request_dir {
            return Err(DeliveryProblem::MixedRequestDirs {
                first: request_dir,
                other: dir,
            });
        }
        plot_image_paths.push(path);
    }

    Ok(MovieDelivery {
        movie_number: response.movie_number,
        title: response.title,
        release_date,
        duration_minutes: response.duration_minutes,
        cover_image_path,
        plot_image_paths,
        summary: response.summary.unwrap_or_default(),
        maker_name: response.maker_name,
        director_name: response.director_name,
        series_name: response.series_name,
        actors: response.actors,
        tag_names: response.tag_names,
        source_url: response.source_url,
        source_id: response.source_id,
    })
}

/// 清掉交付文件与其所在的请求目录。
///
/// 上游 `_cleanup_delivery`：删文件，再删**空**的请求目录。目录删不掉（还有别的
/// 文件）不算错 —— 那是同一次请求里的另一批产物，留着即可。
pub fn cleanup_delivery(paths: &[PathBuf]) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in paths {
        if let Some(parent) = path.parent() {
            if !dirs.contains(&parent.to_path_buf()) {
                dirs.push(parent.to_path_buf());
            }
        }
        if let Err(err) = std::fs::remove_file(path) {
            tracing::warn!(path = %path.display(), error = %err, "元数据交付文件清理失败");
        }
    }
    for dir in dirs {
        // 目录非空时 `remove_dir` 会失败 —— 忽略，见上面的说明。
        let _ = std::fs::remove_dir(&dir);
    }
}

/// 一张图片：必须在 `root` 内、是普通文件、且再深一层。
fn delivered_image(root: &Path, raw: &str) -> Result<PathBuf, DeliveryProblem> {
    let raw_path = PathBuf::from(raw);
    let Ok(path) = raw_path.canonicalize() else {
        return Err(DeliveryProblem::NotAFile { path: raw_path });
    };
    if !path.starts_with(root) {
        return Err(DeliveryProblem::OutsideDeliveryDir { path });
    }
    if !path.is_file() {
        return Err(DeliveryProblem::NotAFile { path });
    }
    // 上游 `len(relative.parts) < 2`：图片必须落在 `<delivery_dir>/<请求目录>/`
    // 里，不能直接躺在交付目录根下 —— 那样不同请求的文件会互相覆盖。
    let depth = path
        .strip_prefix(root)
        .map(|relative| relative.components().count())
        .unwrap_or(0);
    if depth < 2 {
        return Err(DeliveryProblem::MissingRequestDir { path });
    }
    Ok(path)
}

/// 图片所在的那一层目录名（`<delivery_dir>/<它>/<文件>`）。
fn request_dir_name(root: &Path, path: &Path) -> Result<String, DeliveryProblem> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| DeliveryProblem::OutsideDeliveryDir {
            path: path.to_path_buf(),
        })?;
    relative
        .components()
        .next()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .ok_or_else(|| DeliveryProblem::MissingRequestDir {
            path: path.to_path_buf(),
        })
}

/// 严格 `YYYY-MM-DD`：上游是 `date.fromisoformat` 且要求 `parsed.isoformat() == value`。
fn validate_release_date(value: &str) -> Result<String, DeliveryProblem> {
    let parsed = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
    match parsed {
        Some(date) if date.format("%Y-%m-%d").to_string() == value => Ok(value.to_owned()),
        _ => Err(DeliveryProblem::BadReleaseDate {
            value: value.to_owned(),
        }),
    }
}

fn validate_duration(value: i32) -> Result<(), DeliveryProblem> {
    if value > 0 {
        Ok(())
    } else {
        Err(DeliveryProblem::BadDuration { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    /// 每次调用一个不重名的目录，测试之间互不干扰。
    fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "sm-plugins-delivery-{tag}-{nanos}-{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建交付目录");
        dir
    }

    fn put(parent: &Path, relative: &str) -> PathBuf {
        let path = parent.join(relative);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("建请求目录");
        }
        std::fs::write(&path, b"fake-image").expect("写文件");
        path
    }

    fn response(cover: &str, plots: Vec<String>) -> FetchMovieResponse {
        FetchMovieResponse {
            found: true,
            movie_number: "ABC-123".to_owned(),
            title: "一部片子".to_owned(),
            release_date: "2026-01-02".to_owned(),
            duration_minutes: 120,
            cover_image_path: cover.to_owned(),
            plot_image_paths: plots,
            ..Default::default()
        }
    }

    #[test]
    fn a_well_formed_delivery_passes() {
        let dir = scratch("ok");
        let cover = put(&dir, "req-1/cover.jpg");
        let plot = put(&dir, "req-1/plot-1.jpg");

        let delivery = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(
                &cover.display().to_string(),
                vec![plot.display().to_string()],
            ),
        )
        .expect("应当通过");

        assert_eq!(delivery.movie_number, "ABC-123");
        assert_eq!(delivery.release_date, "2026-01-02");
        assert_eq!(delivery.cover_image_path, cover.canonicalize().unwrap());
        assert_eq!(delivery.plot_image_paths.len(), 1);
        assert_eq!(delivery.image_paths().len(), 2);
    }

    #[test]
    fn a_cover_outside_the_delivery_dir_is_refused() {
        // `../` 穿透：插件不能借元数据请求去交出宿主任意路径上的文件。
        let dir = scratch("escape");
        let outside = put(&scratch("outside"), "req-1/cover.jpg");

        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(&outside.display().to_string(), vec![]),
        )
        .expect_err("穿透应当被拒");
        assert_eq!(problem.code(), "movie_delivery_path_escape");
    }

    #[test]
    fn a_directory_is_not_a_deliverable_file() {
        let dir = scratch("not-file");
        let subdir = dir.join("req-1");
        std::fs::create_dir_all(&subdir).expect("建目录");

        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(&subdir.display().to_string(), vec![]),
        )
        .expect_err("目录不是可交付的文件");
        assert_eq!(problem.code(), "movie_delivery_not_a_file");
    }

    #[test]
    fn an_image_directly_under_the_delivery_dir_is_refused() {
        // 少了「请求目录」那一层：不同请求的文件会互相覆盖。
        let dir = scratch("flat");
        let cover = put(&dir, "cover.jpg");

        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(&cover.display().to_string(), vec![]),
        )
        .expect_err("缺请求目录那一层");
        assert_eq!(problem.code(), "movie_delivery_missing_request_dir");
    }

    #[test]
    fn images_from_different_request_dirs_are_refused() {
        let dir = scratch("mixed");
        let cover = put(&dir, "req-1/cover.jpg");
        let plot = put(&dir, "req-2/plot.jpg");

        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(
                &cover.display().to_string(),
                vec![plot.display().to_string()],
            ),
        )
        .expect_err("跨请求目录");
        assert_eq!(problem.code(), "movie_delivery_mixed_request_dirs");
        assert_eq!(
            problem,
            DeliveryProblem::MixedRequestDirs {
                first: "req-1".to_owned(),
                other: "req-2".to_owned()
            }
        );
    }

    #[test]
    fn the_release_date_must_be_strictly_iso() {
        let dir = scratch("date");
        let cover = put(&dir, "req-1/cover.jpg");
        // `2026-1-2`、`2026/01/02` 都能被宽松解析，但 proto 写的是「严格」。
        for bad in ["2026-1-2", "2026/01/02", "2026-01-02T00:00:00", ""] {
            let mut response = response(&cover.display().to_string(), vec![]);
            response.release_date = bad.to_owned();
            let problem = validate_movie_delivery(
                &FetchMovieRequest {
                    movie_number: "ABC-123".to_owned(),
                    delivery_dir: dir.display().to_string(),
                },
                response,
            )
            .expect_err("非严格日期应当被拒");
            assert_eq!(problem.code(), "movie_delivery_bad_release_date", "{bad:?}");
        }
    }

    #[test]
    fn a_non_positive_duration_is_refused() {
        let dir = scratch("duration");
        let cover = put(&dir, "req-1/cover.jpg");
        let mut response = response(&cover.display().to_string(), vec![]);
        response.duration_minutes = 0;
        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response,
        )
        .expect_err("时长必须为正");
        assert_eq!(problem.code(), "movie_delivery_bad_duration");
    }

    #[test]
    fn an_empty_cover_is_refused() {
        let dir = scratch("no-cover");
        let problem = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response("", vec![]),
        )
        .expect_err("封面是必填");
        assert_eq!(problem.code(), "movie_delivery_missing_cover");
    }

    #[test]
    fn cleanup_removes_the_files_and_the_request_dir() {
        let dir = scratch("cleanup");
        let cover = put(&dir, "req-1/cover.jpg");
        let plot = put(&dir, "req-1/plot.jpg");

        let delivery = validate_movie_delivery(
            &FetchMovieRequest {
                movie_number: "ABC-123".to_owned(),
                delivery_dir: dir.display().to_string(),
            },
            response(
                &cover.display().to_string(),
                vec![plot.display().to_string()],
            ),
        )
        .expect("通过");

        cleanup_delivery(&delivery.image_paths());
        assert!(!delivery.cover_image_path.exists(), "文件应当被删");
        assert!(
            !delivery.cover_image_path.parent().unwrap().exists(),
            "空请求目录应当被删"
        );
        // 交付目录本身留着 —— 宿主给的那层由宿主管。
        assert!(dir.exists());
    }
}
