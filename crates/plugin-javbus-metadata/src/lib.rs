//! JavBus 元数据插件：`catalog.metadata_source` 扩展点的 Rust 实现。
//!
//! # 上游对应
//!
//! `upstream/sakuramedia_javbus_metadata/`（Python，324 行）：`plugin.py`
//! 注册、`javbus.py` 抓取与解析、`settings.py` 配置、`manifest.json`。
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `plugin.py:register` | [`service::Control`]（`register`） |
//! | `javbus.py:parse_movie_page` | [`javbus::parse_movie_page`] |
//! | `javbus.py:JavBusMetadataSource.fetch_movie` | [`javbus::JavBusSource::fetch_movie`] |
//! | `javbus.py:_download_image` | [`javbus::JavBusSource`] 的私有 `download` |
//! | `settings.py:Settings` | [`settings::Settings`] |
//! | `html.parser.HTMLParser` | [`html`]（本仓库手写的扫描器，见它的模块文档） |
//!
//! # 三处与上游不同（任务书第四节）
//!
//! 1. **图片落点是 `FetchMovieRequest.delivery_dir`**，不是插件自有的
//!    `<data_dir>/metadata-tmp/`。判据是 proto 写在 `delivery_dir` 上的那句
//!    「元数据图片必须落在其中」，且必须再深一层：`<delivery_dir>/<uuid>/<文件>`。
//! 2. **「没收录」是 `found = false` 的正常响应**，不是 `Err`。
//! 3. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道。
//!
//! # 只到交付校验为止
//!
//! 入库路径还没有（catalog 域缺「插件元数据 → 入库」的服务），所以本 crate
//! **不写导入逻辑、不写库表**：拿到 [`javbus::JavBusSource::fetch_movie`] 的
//! 结果之后由宿主用 `sm_plugins::movie_delivery` 校验。

pub mod html;
pub mod javbus;
pub mod service;
pub mod settings;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_javbus_metadata";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "JavBus";
