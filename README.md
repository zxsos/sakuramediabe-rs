# sakuramedia-scrape-translate

SakuraMedia 影片文案抓取与翻译插件的 Rust 实现。

上游：[tinypinglite/sakuramedia_movie_scrape_translate](https://github.com/tinypinglite/sakuramedia_movie_scrape_translate)（Python）。

## 功能

从 DMM 抓取影片的日文标题和简介，可选经 OpenAI 兼容服务翻译成中文后写回影片标题与简介。

三个后台任务：

| task_key | 触发 | 说明 |
|---|---|---|
| `sakuramedia_movie_scrape_translate_sync` | 手动（按番号） | 抓取并翻译指定番号 |
| `sakuramedia_movie_scrape_translate_sync_subscribed` | 每天 04:10 | 按优先级全量抓取并翻译 |
| `sakuramedia_movie_scrape_translate_translate_cached` | 手动 | 只翻译已有 DMM 缓存，不请求 DMM |

## 与上游的对应

| 上游（Python） | 这里（Rust） |
|---|---|
| `plugin.py:register` | `service::Control::register` |
| `jobs.py:build_jobs` | `service::job_definitions` |
| `jobs.py:run_pipeline` | `jobs::run_pipeline` |
| `dmm.py:DmmClient` | `dmm::DmmClient` |
| `dmm.py:_Page` | `html::DmmPage` |
| `translation.py:OpenAITranslationClient` | `translation::TranslationClient` |
| `state.py:DmmState` | `state::DmmState` |
| `settings.py:DmmSettings` | `settings::Settings` |
| `prompts/*.md` | `include_str!` 编译期嵌入 |

## 偏离上游的地方

1. 任务是 gRPC 流式（`PluginControl.RunJob` → `stream JobEvent`），不是 Python 的回调。
2. 影片的列举与写回由 `jobs::MovieStore` trait 抽象 —— 进程拆分后没有进程内宿主对象，宿主在任务编排层实现它。
3. 配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读，不是 `context.settings`。
4. 越界配置按边界取值，不报错（避免看门狗反复重拉）。

## 构建与测试

```bash
cargo build
cargo test
```

测试不联网：DMM 页面与翻译服务都用 wiremock 本地假服务。
