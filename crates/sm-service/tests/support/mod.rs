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

use std::path::{Path, PathBuf};

use sm_db::repo::{
    ImageRepository, MediaLibraryRepository, MediaRepository, NewImage, NewMedia, NewMediaLibrary,
};
use sm_db::testing::TestDb;
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

/// 建一条媒体，返回其 id。
///
/// `movie_number = None` 时是「非 JAV」媒体 —— 注意 `video_item_id` **也留空**，
/// 于是这条媒体两者都没有（缩略图目录因此定不下来，见
/// `thumbnail_namespace_unresolved` 那条用例）。
pub async fn seed_media(db: &TestDb, movie_number: Option<&str>) -> i32 {
    let library_id = seed_library(db).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m-{}.mp4", n()),
            file_size_bytes: 1,
            // XOR：给了 movie_number 就不能给 video_item_id
            movie_number: movie_number.map(str::to_owned),
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
        .id
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
