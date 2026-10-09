//! 集成测试共用的夹具与播种函数。
//!
//! # 为什么需要它
//!
//! 「临时图片根 + 指向它的配置」与「建库 / 建媒体 / 建图片行」在**三个**用例
//! 文件里都要用。抄第二遍就是**两份会各自漂移的夹具** —— 比如一处 `Drop` 里清了
//! 临时目录、另一处忘了，于是跑几十次之后 `%TEMP%` 里堆满垃圾，而没人会觉得
//! 那是 bug；又比如一处给 `media` 多填了一个字段，另一处的用例行为就悄悄不同了。
//!
//! `tests/` 下的**子目录**不会被 cargo 当成独立的测试目标，所以 `mod support;`
//! 只把这里编进引用它的那几个用例文件。
//!
//! 每个用例文件只用到这里的一部分（一个用 `ImageRoot`、另一个只用 `pack_entries`），
//! 所以「从未被使用」这个 lint 在**单个目标**里必然误报。
#![allow(dead_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use sm_db::repo::{
    ImageRepository, MediaLibraryRepository, MediaRepository, NewImage, NewMedia, NewMediaLibrary,
};
use sm_db::testing::TestDb;
use sm_service::playback::media::MediaService;
use sm_service::playback::provider_helpers::{
    MediaHandle, ProviderFailure, StorageGateway, ThumbnailJobResult,
};
use sm_service::system::config::ConfigService;

/// 进程内自增计数 —— 给临时文件名与唯一键（番号、文件名）用。
pub fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 一个独立的临时图片根 + 指向它的配置服务。`Drop` 时连配置一起删掉。
pub struct ImageRoot {
    root: PathBuf,
    /// 指向 `root` 的配置服务。**公开** —— 被测服务要拿它。
    pub config: ConfigService,
    config_path: PathBuf,
}

impl ImageRoot {
    /// 建目录、写配置。
    ///
    /// 配置内容刻意只有一段 `[media]`：`snapshot()` 会把磁盘上的值
    /// **overlay 到 schema 缺省值**上，所以缺的字段有缺省，不必写全。
    pub fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("sm-image-root-{}-{}", std::process::id(), n()));
        std::fs::create_dir_all(&root).expect("建临时图片根");

        let config_path = root.join("config.toml");
        // TOML **字面量字符串**（单引号）：Windows 路径里的反斜杠在基本字符串里
        // 是转义符，会被 `\t` / `\U` 之类吃掉。
        std::fs::write(
            &config_path,
            format!(
                "[media]\nimport_image_root_path = '{}'\n",
                root.to_string_lossy()
            ),
        )
        .expect("写临时配置");

        Self {
            root,
            config: ConfigService::new(config_path.clone()),
            config_path,
        }
    }

    /// 图片根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 按库里的**相对路径**在磁盘上造出真实文件（自动建父目录）。
    pub fn write_loose(&self, relative: &str, bytes: &[u8]) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(&path, bytes).expect("写文件");
    }

    /// 该相对路径在磁盘上是否有文件。
    pub fn loose_exists(&self, relative: &str) -> bool {
        self.root.join(relative).is_file()
    }
}

impl Drop for ImageRoot {
    fn drop(&mut self) {
        // 用例**失败**时也要清掉，否则临时目录只增不减。
        std::fs::remove_dir_all(&self.root).ok();
        std::fs::remove_file(&self.config_path).ok();
    }
}

/// 「远端删除总是成功」的 provider 桩，以及由它装配的媒体服务。
///
/// # 为什么用例**必须**给它
///
/// `MediaService::delete_media` 的第一步就是让 provider 删远端文件，而
/// **没注入网关时它直接 503 `provider_not_installed`** —— 那是刻意的：跳过会
/// 留下远端文件，而调用方以为删干净了。集成测试要验的是库里的行与磁盘上的
/// 文件，所以给一个总是成功的桩。
///
/// ⚠️ 要验「没装插件」那条分支的用例**别用它** —— 那正是
/// `MediaService::new(...)` 之后不做 `with_gateway` 的原样返回。
pub struct NoopGateway;

impl StorageGateway for NoopGateway {
    fn has_provider(&self, _provider_key: &str) -> bool {
        true
    }

    fn delete_media(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderFailure>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn generate_thumbnails(
        &self,
        _handle: &MediaHandle,
        _workspace: &Path,
    ) -> Pin<Box<dyn Future<Output = Result<ThumbnailJobResult, ProviderFailure>> + Send + '_>>
    {
        // 这套夹具不验缩略图生成（那要一个能往 workspace 写产物的桩）。
        unimplemented!("NoopGateway 不生成缩略图")
    }
}

/// 带桩网关的媒体服务。删除类用例用它构造。
///
/// # ⚠️ 删除类用例**不能**用 `TestDb::pool()`（`max_connections = 1`）
///
/// 删除会先取一条**会话级** advisory lock 并**持有它**去做后续的库操作，而
/// 那条锁自己就占着一条连接 —— 池里只剩 0 条，后面的查询全部在
/// `acquire_timeout`（5s）后报 `pool timed out while waiting for an open
/// connection`（表现为 500，看不出是池的问题）。
///
/// 需要的是 `db.pool_with_max_connections(2)`，与
/// `sm_db::common::advisory_lock` 模块文档里那条「需要连接数 = 同时持有的锁数 + 1」
/// 是同一件事。这条不是猜测：`deleting_a_video_takes_its_media_rows_with_it`
/// 第一次真连库跑出来的就是它。
pub fn media_service_with_gateway(pool: &sqlx::PgPool, config: &ConfigService) -> MediaService {
    MediaService::new(pool, config).with_gateway(Arc::new(NoopGateway))
}

/// 该番号的 `movie` 行不存在就建一条。见 [`seed_media`] 的文档。
pub async fn seed_movie_if_missing(db: &TestDb, movie_number: &str) {
    use sm_db::repo::{MovieRepository, NewMovie};
    let repo = MovieRepository::new(db.pool().clone());
    if repo
        .find_by_number(movie_number)
        .await
        .expect("查 movie")
        .is_some()
    {
        return;
    }
    repo.insert(&NewMovie {
        movie_number: movie_number.to_owned(),
        title: movie_number.to_owned(),
        ..NewMovie::default()
    })
    .await
    .expect("insert movie");
}

/// 建一个媒体库 —— 媒体必须挂在某个库下。
pub async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

/// 建一条媒体，返回其 id。**需要番号时连 `movie` 行一起建**。
///
/// # 为什么必须连 `movie` 行一起建
///
/// `media.movie_number` 上有外键 `media_movie_number_fk`（`docker/schema.sql:510`）
/// —— 只建 media 会撞 `23503`。这个夹具此前就是这么写的，于是
/// `media_thumbnails` / `media_point_delete` 两个套件**全部**在夹具里就炸了；
/// 而它们从来没跑过（无库时静默跳过），所以一直没暴露。
///
/// 建 movie 是**幂等**的（先查后插）：同一套件里多次用同一个番号建媒体
/// （`media_summary` 那种）不会撞 `movie.movie_number` 的唯一约束。
///
/// ⚠️ `movie_number = None` 表示「**两个归属都没有**」的媒体。那种行
/// **建不出来**：`NewMedia` 的校验（`Media 必须恰好归属 movie（JAV）或
/// video_item（非 JAV）之一`）会先把插入拒掉。所以这里先建一条正常的（带番号），
/// 再用裸 SQL 把 `movie_number` 置空 —— DDL 里**没有**对应的 CHECK，
/// 于是那种行在库里是合法的（手工改库、别的工具留下的都可能长这样），
/// 只是**经过仓储**造不出来。聚合那份逻辑：不然每个用例都要抄一遍这个把戏。
pub async fn seed_media(db: &TestDb, movie_number: Option<&str>) -> i32 {
    let library_id = seed_library(db).await;
    // 先落一个番号（借用一下它的外键目标），最后再按需置空。
    let anchor = movie_number
        .map(str::to_owned)
        .unwrap_or_else(|| format!("ANCHOR-{}", n()));
    seed_movie_if_missing(db, &anchor).await;
    let media_id = MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m-{}.mp4", n()),
            file_size_bytes: 1,
            // XOR：给了 movie_number 就不能给 video_item_id
            movie_number: Some(anchor.clone()),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id;

    if movie_number.is_none() {
        sqlx::query("UPDATE media SET movie_number = NULL WHERE id = $1")
            .bind(media_id)
            .execute(db.pool())
            .await
            .expect("置空番号");
    }
    media_id
}

/// 登记一张图片（`origin` 唯一，重复登记返回同一个 id）。
pub async fn seed_image(db: &TestDb, origin: &str) -> i32 {
    ImageRepository::new(db.pool().clone())
        .upsert(&NewImage {
            origin: origin.to_owned(),
        })
        .await
        .expect("upsert image")
        .0
}

/// 建一条 `media_thumbnail`。裸 SQL —— 正式路径是 worker 的
/// `record_thumbnail_success`（带状态机），这里只要一行数据。
pub async fn seed_thumbnail(db: &TestDb, media_id: i32, image_id: i32, offset: i32) -> i32 {
    let row = sqlx::query_as::<_, (i32,)>(
        "INSERT INTO media_thumbnail \
             (media_id, image_id, \"offset\", image_search_index_status, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4) RETURNING id",
    )
    .bind(media_id)
    .bind(image_id)
    .bind(offset)
    .bind(sm_db::common::time::now_utc())
    .fetch_one(db.pool())
    .await
    .expect("插入 media_thumbnail 失败");
    row.0
}

/// 包里的条目名（升序）。
pub fn pack_entries(pack_path: &Path) -> Vec<String> {
    let file = std::fs::File::open(pack_path).expect("开包");
    let mut archive = zip::ZipArchive::new(file).expect("解析包");
    let mut names: Vec<String> = (0..archive.len())
        .map(|index| archive.by_index(index).expect("取条目").name().to_owned())
        .collect();
    names.sort();
    names
}
