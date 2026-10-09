# 续做交接

给「下一次接着干」的人（或新会话里的 AI）看。按本文档开工，不需要重读整个会话历史。

## 一、当前状态

> **数字以 `docs/progress-baseline.md` 为准** —— 跑 `pwsh -File scripts/progress.ps1 -Diff`
> 核对，漂移即失败。下表同步到 **2026-10-07 晚**（HEAD `803337f`）。

| 项 | 值 |
|---|---|
| 铺开阶段 | ✅ 已完成（2026-10-05）。路由模块 32/32、端点路径 **136/136**、服务层 106/113 文件 |
| 验证阶段 | ✅ 编译 + clippy + rustdoc + `compare*.py` + `check_paged_wrappers.py` 全绿（2026-10-05） |
| 端点方法体 | **实测 `todo!()` 共 59 个**：`sm-service` **28** + `sm-api` **31**。`sm-db` / `sm-scheduler` / `sm-core` / `sm-plugins` **各 0 个** |
| 端点（方法级） | 上游 177 / Rust **175 已注册**；其中 **33 条 handler 仍是 `todo!()`**。未注册 2 条：`/actors/{}/profile-image\|PUT`、`/media/{}/clips\|POST` |
| 完成的域 | `collections`、`system`、**`videos`（3/7）** —— `system` 本轮清零 |
| 调度 | 19 个内建任务，cron **16/16 全注册**；worker **handler 6/21** |
| 门禁 | `verify.ps1 -Tier full` **全绿**（fmt / doc / clippy / 单测 / 真库集成 / 对拍 / **契约两仓同步** / 进度基线，共 12 项） |
| 提交 | 推送状态以 `git status -sb` 为准（`cnb` 从未推过） |

**下一步看 §七「交接快照（2026-10-07）」与 §八「接下来做什么」** —— 前者是剩余 63 条的
**卡点表**与待拍板项，后者是可立即开工的**执行清单**。§一之二以下的数字是**历史计划**，
别照它开工。**部署形态与瘦身目标见 [`deployment.md`](deployment.md)。**

## 一之一、`cargo check` 全绿是怎么来的（错误分布）

从 `sm-service` 的 75 个错误到全工作区 0 个，一共修了 **约 100 个**：

| 批 | 范围 | 错误数 | 性质 |
|---|---|---|---|
| 1 | `sm-service`（discovery 4 文件 + 散落 10 文件） | 75 | 骨架期遗留：符号缺失、类型没对齐、语法 |
| 2 | `sm-api` | 10 | 缺 import、`DeleteTaskQuery` 重复定义、`one`/`twenty` 重复定义 |
| 3 | **测试目标**（`--all-targets` 才暴露） | 6 | `assert_eq!` 缺参、`&mut` 借用、测试里 `.await` 缺失 |
| 4 | `sm-scheduler` | 12 | `ProgressSink` 不 `Send`、handler future 要 `'static`、`QdrantConfig` 形状 |
| 5 | `sm-server` | 1 | `capability::EXTENSION_RANKING_SOURCE` 少一层模块 |

**第 3 批值得单独说**：`cargo check -p sm-service` 是绿的，但 `#[cfg(test)]` 里的代码
**不参与**那次编译。只跑不带 `--all-targets` 的 check 会以为没问题。
**验证命令一律带 `--all-targets`。**

## 一之一之二、clippy / rustdoc / fmt 三门是怎么清掉的

**clippy 64 → 0**。顺序很重要，先让机器修：

```powershell
cargo clippy --fix --workspace --all-targets --allow-dirty --allow-staged
```

一步从 64 降到 34（`--fix` 只吃机器可判定的建议）。剩下的 34 分五类：

| 类 | 数量 | 处理 |
|---|---|---|
| `dead_code`：字段从不读 | 13 个结构体 / 20 个字段 | **加 `#[allow(dead_code)]` + 一行注释说明「哪个 `todo!()` 落地后删」** |
| `clippy::new_without_default` | 5 | 加 `impl Default { fn default() -> Self { Self::new() } }` |
| `clippy::type_complexity` | 4 | 抽 `type` 别名（`ImageDownloader` / `RebuildPackHook` / `SessionParams`） |
| `items_after_test_module` | 2 | 把服务定义**移到** `#[cfg(test)] mod tests` 之前 |
| 一次性小项 | 10 | 未用导入/变量、`vec!`、多余 cast、`&mut Vec`→`&mut [_]`、`div_ceil`、废弃常量、常量断言 |

**关于 `#[allow(dead_code)]` 的约定**（新增，见纪律第 16 条）：这 13 处**不是**
「代码写错了」，是「方法体还是 `todo!()`」。**每一条都必须带一句注释写明
哪个方法落地后删掉 allow**，否则它就从「待办」变成「永久静音」。
它们同时在 `handoff` 里作为「还剩多少活」的清单存在。

**rustdoc 门禁红了 8 处 `broken_intra_doc_links`**，全是**同一类错**：
`//!` 模块级文档里写了 `[`Self::foo`]`。**`Self` 在模块作用域没有定义** ——
模块文档只能用模块内可见的路径。逐条改成：
- `[`Self::MEDIA_LIST_SORT_FIELD_MAP`]` → `[`MEDIA_LIST_SORT_FIELD_MAP`]`（本来就是模块级 const）
- `[`Self::ensure_subtitle_path`]` / `[`Self::minimum_acceptable_count`]` → 去掉 `Self::`（都是模块级自由函数）
- `[`score_movies`]` / `[`safe_ratio`]` / `[`Self::list_duplicate_media_groups`]`
  → 写**全路径** `[`HotActressReleaseService::score_movies`]` 等（它们是关联函数）
- `[`JAVDB_CHECK_INTERVAL`]` → 常量真名是 `JAVDB_CHECK_INTERVAL_DAYS`
- 跨 crate 的 `[`RankingSourceCatalog`]` → `[`RankingSourceCatalog`](sm_service::discovery::ranking::RankingSourceCatalog)`

**`cargo fmt --all` 不是「可选的美化」**：它跑之前有 **81 个文件**不符合吧
rustfmt。`verify.ps1` 里这一步**只跑不改判**（`cargo fmt --all` 永远 exit 0），
所以它其实**不是门禁**，而 CI 也没有 `--check`。结论：格式化靠自觉，
提交前跑一次 `cargo fmt --all`，别指望门禁拦你。

## 一之一之三、parity 14 处 `UNCHECKED_STRUCT` 是怎么清掉的（已完结）

第六道门原报 14 处，**已全部清零，现在 40/40 通过**。三类，判据是脚本自己
写死的**豁免条件 1**：「不参与任何 SQL —— 没有 `#[derive(FromRow)]`，
不被 `query_as!` 使用」。

| 类 | 数量 | 处理 | 判据 |
|---|---|---|---|
| **仓储** | 4 | 补登记豁免 | 持 `PgPool`，本身不映射表 —— 与豁免名单里已有的 20 多个 `*Repository` 同形，是**登记遗漏** |
| **投影行** | 9 | 改成 `pub type X = (...)` 元组别名 | 带 `FromRow`、被 `query_as` 用；是多表 JOIN / 列子集，不是任何单表的镜像 |
| **聚合值对象** | 1 | 补登记豁免 | `MovieFeatures` **没有 `FromRow`**，是两次元组查询在内存里拼出来的累加器；`movie_features` 这张表两侧 DDL 里都不存在 |

**⚠️ 关键教训：先查 `#[derive(FromRow)]` 再定性，不要按名字猜。**
`MovieFeatures` 名字看着像「表镜像」，实际是纯值对象 —— 我本来打算一并改成
元组，白改。反过来 `PopularMovieRow` 只有两列、看着像值对象，实际是
`query_as` 的行。

**投影行的改造政策早就定死在 `crates/sm-db/src/repo/movie.rs:104-114`**：

> 1. 给对拍脚本加豁免 + 放宽判定条件 —— 那是**削弱门禁**……
> 2. 让 `sm-db` 返回元组，把具名类型放到 `sm-service`。
>
> **选 2。** 代价是这一层的可读性靠文档，收益是门禁一点没松。

样板：`MovieResolutionLevelRow = (i32, i32)`（`movie.rs:122`）、
`TagCountRow = (i32, String, i64)`（`tag_list.rs:29`）。

**改造时定下的一条写法约定（新增，见纪律第 20 条）**：
元组别在调用点散用 `.0` / `.4`，**在循环头一次解构取名**：

```rust
for (item, vector) in chunk.iter().zip(vectors) {
    let (thumbnail_id, media_id, movie_id, _movie_number, offset_seconds, _) = item;
    // 之后一律用名字
}
```

这样「位置含义」只在**一处**（类型别名的文档）定义，循环体里全是名字 ——
既过了门禁，又不把可读性赔进去。

**注意**：`compare_schema.py` 的 `RUST_ROOT` **只扫 `crates/sm-db/src`**，
所以 `sm-service` 里的同名结构体不参与对拍 —— 这正是「具名类型放
`sm-service`」能成立的前提（`MomentRecommendationRow` 就是这么放的，
`discovery/moment_recommendation.rs:161`，无 `FromRow`）。

**一个安全提醒**：改造中我用 `python -c` 做了一次全文 `item.thumbnail_id` →
`*thumbnail_id` 的批量替换，**它误伤了另一个函数里同名的表达式**（那个循环
没有解构，`*thumbnail_id` 不在作用域）。`cargo check` 当场抓到。**批量替换
前先确认目标字符串在文件里只出现在你想到的作用域里**。

**验证命令**（按便宜程度排序）：

```text
cargo check --workspace --all-targets --message-format short          # 约 5 秒（增量）
cargo clippy --workspace --all-targets --all-features -- -D warnings  # 增量几秒
$env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --no-deps      # 增量几秒
python parity/compare_schema.py                                       # 秒级
python parity/compare.py ; python parity/compare_core.py ; python parity/check_paged_wrappers.py
```

⚠️ `cargo clippy --message-format short -- -D warnings` **会把 `--message-format`
转发给 clippy-driver**（报 `Unrecognized option`）—— 要么放在 `--` 之前，
要么干脆不加。

## 一之二、下一步：**继续接方法体**（⚠️ 本节数字是历史计划，已过时）

> **别照本节开工。** 本节写下时是 238 个 `todo!()`；**当前（2026-10-07）是 78 个**。
> **权威清单看 `docs/progress-baseline.md` 与 §七「交接快照」。** 下面的分块列表保留为
> **原计划**（哪些族已接、接的时候要补什么下层）—— 大部分已落地，剩余项的卡点见 §七.3。

验证阶段已收口。接下来的活只有一件：把 `todo!()` 换成真实实现。

| 位置 | 写下时 | 现状（2026-10-07） |
|---|---|---|
| `sm-service` | 150 | **35** |
| `sm-api` | 88 | **43** |
| `sm-db` | 0 | ✅ 0 |
| `sm-scheduler` | 0 | ✅ 0 |

### 已接完的（按族记）

| 族 | 端点 | 需要的新下层 |
|---|---|---|
| `clip-collections` | 9 | —— |
| `moment-collections` | 9 | `MomentCollectionRepository::list_ordered_by_recency`、`MediaPointRepository::find_by_ids`、`MomentCollectionService::{require, get_with_count, list_collections, list_points_paged}`、`INVALID_MOMENT_COLLECTION_FILTER` |
| `video-collections`（**全部 9 个**） | 9 | 合集级：`VideoItemRepository::find_by_ids`、`VideoCollectionRepository::list_ordered_by_recency`、`VideoCollectionService::{get_with_count, list_collections}`；成员级：`VIDEO_COLLECTION_ITEM_SORT_FIELD_MAP` + `list_page_with_video`、`VideoCollectionService::{list_items_paged, list_item_rows}` + `VideoCollectionItemRow` |
| `GET /videos`（列表） | 1 | `VideoItemListItemResource` 全套组装（见下）|
| **media 点列表 / 建点 / 进度** | **3** | **`MediaService` 从无状态单元结构体改成持有 `Db`**（见下）+ 仓储补 `find_by_media_and_thumbnail` / `list_all_by_media`，并**修掉 `insert` 漏写 `thumbnail_id` 的 bug** |

### ⚠️ `MediaService` 的形状此前是错的（本轮修）

`playback/media.rs` 原来是 `pub struct MediaService;` —— **无状态单元结构体，
所有方法都是关联函数**，拿不到任何仓储。所以那 12 个方法**全是 `todo!()`，
一个都落不了地**，不是「还没写」。

全仓**没有任何调用点**（`grep MediaService::` 只命中模块文档），所以改形状
零风险。现已改成持有五个仓储 + `Db`，并把**全部 12 个方法**统一成 `&self`
（一次改完，下一批只填方法体）。

同批修掉一个**静默丢列**的 bug：`MediaPointRepository::insert` 的 INSERT 列
清单里漏了 `thumbnail_id`（DDL 有、模型有），于是恒为 NULL。后果不止少一列
—— service 的**幂等判据**正是 `WHERE media_id = ? AND thumbnail_id = ?`，
恒 NULL 会让它永远命中不了，重复建点变成必然且无报错。已补列 + 加两条仓储
集成测试钉住（`point_insert_round_trips_the_thumbnail_id` /
`point_list_all_by_media_orders_by_id_not_by_offset`）。

### ✅ `VideoItemListItemResource` 组装已落地（本轮）

这是此前判断的「主要瓶颈」，已经做完，**三族端点现在都不再被它挡住**：

| 新增 | 位置 |
|---|---|
| `sm_core::media_formats::normalize_media_resolution` | 上游 `src/common/media_formats.py` 的对应物（含前导零/维度上限/两段三条规则） |
| `VIDEO_LIST_SORT_FIELD_MAP` + `video_list_sort_column` | `sm-db/repo/video_item.rs` —— 排序白名单，`duration`/`file_size` 用**相关子查询**表达上游的 `MIN(Media.id) WHERE valid` + `COALESCE(…,0)` |
| `VideoItemRepository::{list_page, media_stats, first_valid_media, collections_map}` | 四条新查询，投影行一律 `pub type` 元组别名 |
| `VideoItemService::{list, assemble}` | `assemble` 是**共享入口**：四条批量查询，`GET /videos` 与合集成员端点共用 |
| `dto::VideoItemListItemResource` + `deserialize_double_option` | 14 字段资源 + 通用的三态反序列化器 |

### ✅ `video-collections` 四个成员端点也已接完（本轮）

`GET /{id}/items`、`POST /{id}/items`、`DELETE /{id}/items/{item_id}`、
`POST /{id}/items/reorder`。这一族 **9/9 全通**。

三处值得记的：

1. **上游在这里不填 `collections`。** `_query_item_resources` 调
   `_to_list_item` 时没传 `collections`，默认空列表 —— 所以本仓加了
   `VideoItemService::assemble_without_collections`，**不是**「顺手补全」。
2. **`duration`/`file_size` 的两个排序片段是复用的常量**
   （`VIDEO_FIRST_MEDIA_DURATION_COLUMN`），不是抄第二份。
3. **`reorder` 要重新读一遍成员**：`reorder` 返回的行带的是**旧**位置，
   而上游 `reorder_items` 结尾是 `_query_item_resources(collection)` 读库。

### `image_cleanup` 已落地（4 个 `todo!()` 清零）—— 但只到「未打包布局」

`catalog/image_cleanup.rs` 四个方法全实现了，`todo!()` 清零。要点：

| 位置 | 做了什么 |
|---|---|
| `ImageCleanupService::image_root_path` | 从 config 读 `media.import_image_root_path`；`~` 展开 + 相对路径按 **cwd** 解析（照上游，不是相对图片根）|
| `image_record_is_still_used` | **转调** `sm_db::ImageRepository::is_referenced` —— 判据只此一份 |
| `delete_image_record_if_unused` | 转调 `delete_if_unreferenced`（**查引用与删记录在同一个事务里**）|
| `delete_obsolete_image_files` | 去空白/去重/排序 → 不在包里的走 `unlink`；包内成员**跳过**（见下）|

**引用方清单搬到了 `sm_db::repo::image`**（`IMAGE_REFERENCE_SITES`，`sm-service`
`pub use` 回来），因为那条 SQL 也在那儿 —— 清单与 SQL 分家就会分叉。

#### ★ 骨架期的清单是错的：5 项 → 实际 8 项，且表名写错

原来写的是五项，第五项 `plot_image.image_id` —— **没有 `plot_image` 这张表**
（真名 `movie_plot_image`）。漏的三处：`actor.profile_image_override_id`、
**`media_point.image_id`**、`video_item.cover_image_id`。

中间那个最危险：它 `NOT NULL` + `RESTRICT`，即「每个时刻点都钉着一张图」，
漏查等于**每次删时刻点都顺手删掉那张图**。

**新增对拍测试 `crates/sm-service/tests/image_reference_sites.rs`**：直接读
`information_schema` 里所有指向 `image(id)` 的外键列，与常量逐条对拍
（双向：漏登记与写错表名都会红）。这条测试是本轮最有价值的产出 ——
它把「新增一张引用 `image` 的表」从「等线上裂图」变成「CI 红」。

#### 包内成员的清理：**已补上**（下一轮 `movie_asset_pack` 落地后）

上游三支现在都有了：包不存在（旧布局逐个 `unlink`）、`assets.zip`（转包服务
**按数据库活跃集重建**）、`thumbnails.zip`（包内无存活条目则删包，否则重建包）。

`thumbnails.zip` 那支有两条「宁可不做」的分支，都写进文档了：包内一个条目都
读不出来 → **保留旧包不重写**（重写会产出空包 = 把数据删了）；写临时包失败 →
删临时包再报错，**正式包一个字节都没动**。

#### 加固：`resolve_inside` 是**本仓新增**的，上游没有

上游 `_unlink_image_file(image_root / relative_path)` 直接用，而 Python 的
`Path.__truediv__` 遇到绝对路径会**替换**掉整个前缀（Rust 的 `Path::join`
行为相同）—— 也就是 `origin = "/etc/passwd"` 会原样落到系统文件上。
`image.origin` 只是 `varchar(255)`，一个 bug 或一次手工改库就能造出来，而
`unlink` 不可撤销。所以逐段检查：绝对路径、`..`、根前缀一律 422
`invalid_image_path`；空路径也拒（它解析成图片根本身）。

### `movie_asset_pack` 已落地（4 个 `todo!()` 清零）+ `zip` 依赖引入

上表说的「两件事的共同前置」都通了。这一批还**新开了两个共享模块**：

| 新模块 | 上游 | 内容 |
|---|---|---|
| `catalog/media_paths.rs` | `common/media_paths.py` 的子集 | 图片根（`~` 展开 / 相对路径按 **cwd** 解析 / 缺省值）、`image_pack_relative_path`（两条包约定，段数恰好 4）、`resolve_inside`（逃逸防护）、`remove_file_if_exists` |
| `catalog/image_store.rs` | `common/image_store.py` 的子集 | `write_pack`（**ZIP_STORED** + `fsync`）、`read_pack_entry` |

本仓没有上游那个 `common/` 层，而这两块的用户跨 `catalog` 与 `playback`
（未来的 `media_thumbnail_service` 要写 `thumbnails.zip`），所以谁都不该"拥有"
它们 —— 放在 `catalog/` 下并在 `catalog/mod.rs` 里写明理由。

**`zip` 依赖必须 `default-features = false`**：默认特性会拖进 bzip2 / deflate /
aes-crypto / zstd 一整套，而本仓只用 STORED。根 `Cargo.toml` 的注释写了原因。

#### 端到端测试 `crates/sm-service/tests/movie_asset_pack.rs`

真实数据库 + 真实临时图片根 + 真实 zip 字节，5 条用例。锁的是**只看代码看不出
来**的三件事：

- 包里有哪几条（`live_origins` 的 LIKE 粗筛 + Rust 侧精筛）；
- **二次重建的字节来自旧包** —— loose 文件已被清掉。没有这条兜底分支，第二次
  重建会走「拿不到字节 → 三次重试 → 返回 `is_file()` 为 true」，**看起来成功
  但包一个条目都没更新**；
- 嵌套的 `media/<id>/thumbnails/` **不进** `assets.zip`（它归另一个包），且
  `remove_loose_files` 不许进子目录（否则把另一个包的数据删了）。

另外把 `image_cleanup` 串起来验了：删记录 → 传 origin → 删文件 → **包按活跃集
重建，被删条目消失、保留条目的字节不变**。

> 顺带：`movie_asset_pack.rs` 里那条「类型级占位用例」
> （`a_movie_without_images_is_not_an_error`）已随本批**删除** —— 它的语义现在由
> `an_empty_live_set_reports_false_and_removes_the_pack` 真实覆盖（还多验了
> 「空集要删包」这一条）。

### 媒体点的删除链路已通（`delete_point` / `delete_point_by_id` + 路由）

`MediaService` 现在持 `Db` + `ConfigService`（图片根要从配置读），
`ImageCleanupService::new(db, config)` **去掉了那个骨架自造的
`RebuildPackHook`**（形状是错的：上游收的是**目录**不是 `movie_id`，而且包重建
已走 `MovieAssetPackService`，一个闭包表达不了）。

`DELETE /media/{media_id}/points/{point_id}` 已返回真 204。

#### 次序是硬要求：**先删点，后清图**

反过来（先判断引用）会因为「这个点还指着它」而永远清不掉图 —— 而点已经没了。
中间崩溃最坏是「图成孤儿」（磁盘浪费，可恢复），不是「DB 指向打不开的图」。

#### 路径 id 的窄化：**404 而不是 400**

路由层的 `media_id` / `point_id` 是 `i64`（模块文档要求），库里是 `i32`。
超出 `i32` 的值**不是 400 而是 404**：上游是 Python 的 `int`，没有上界，
那种 id 会一路走到查询、查不到、报「不存在」。用 `Path<i32>` 会在提取阶段
变成 400 —— 与上游不一致。见 `routes/media.rs` 的 `narrow_media_id` /
`narrow_point_id`。

#### 新测试 `crates/sm-service/tests/media_point_delete.rs`（6 条）

最值钱的两条：**共享图片**（同一条 `image.origin` 被两条媒体上的时刻点共同
引用，删一个不能动它，删完最后一个才清）与**跨媒体删点必须 404 且什么都不改**
（实现成「按 point_id 删」的话，一个 id 猜错就删错数据）。

另外新增 `tests/support/mod.rs`：临时图片根 + 配置的夹具。**两个测试文件都要用
它** —— 抄第二遍就是两份会各自漂移的夹具（一处 `Drop` 忘了清临时目录，跑几十次
之后 `%TEMP%` 堆满垃圾，而没人觉得那是 bug）。

### 缩略图族：目录布局、列表、读字节已通（`artifacts.rs` 5 → 1）

`ThumbnailArtifactService` 改持有 `Db` + `ConfigService`（同 `MediaService`
那一轮）。`GET /media/{id}/thumbnails` 已接。

#### ★ 骨架猜的目录布局是**错的**

骨架写的是 `<image_root>/thumbnails/<media_id>/`，而上游是**按媒体归属分**：

```text
  JAV 媒体:  <root>/movies/<sha1 前2位>/<番号>/media/<media_id>/thumbnails
  视频条目:  <root>/videos/<video_item_id>/media/<media_id>/thumbnails
```

差别不是"好不好看"：扁平布局下「删一部影片」得**全表扫描**才知道该删哪些
目录；归属布局下是一个 `rm -rf`。而且 `thumbnail_pack_file` 依赖「与目录同级
同名」这条约定（`image_pack_relative_path` 靠它反推包路径）。

为此 `media_paths` 补了 `normalize_asset_dir_name` / `movie_asset_shard` /
`movie_asset_relative_dir` / `MOVIE_MEDIA_SUBDIR`。

#### ★ 分片名**必须是 SHA-1**，且归一化只有一份

`movie_asset_shard` 用 `sha1`（新依赖）。换成 SHA-256 会算出**完全不同的分片**，
迁移过来的数据目录里既有图片一张都找不到 —— 表现为「图片全丢」且**无任何报错**。
所以单测直接钉住 Python 算出来的值（`sha1("ABC-001")` → `a0`）。

`normalize_asset_dir_name` 是四类资产（封面/剧照/缩略图/字幕）**共用的**：
不共用的话同一部影片会散到两个目录，症状是「封面在，缩略图没了」。

#### 一处新的偏离（有意的）

上游 `thumbnail_directory` 在「既无番号也无视频条目」时会静默产出
`videos/None/` 这个**幻影目录** —— 缩略图写进去再也没人找得到。
这里改成 422 `thumbnail_namespace_unresolved`。

#### 契约更正（第 4 处骨架写错）

`MediaThumbnailResource`：骨架是 `id` / `offset` / `image_path`，上游是
`thumbnail_id` / `offset_seconds` / `image` + `width` / `height`。
service 侧类型改名 `MediaThumbnailValue` 带未签名路径，API 层 `dto.rs` 才签名
（同 `MediaPointValue` 的约定）。

`width`/`height` 取自**第一条**缩略图（同一视频流的尺寸相同），解不出来时
两者都留空 —— 上游同款：只记一条 warn，不让整个列表 500。

#### 新增 `image_store::read_image_bytes`

**包条目优先、单文件兜底**（上游同款）。顺序不能反：打包后 loose 文件会被
清掉，包才是权威；而包损坏/条目缺失要回退单文件，否则「包被删过一次」等于图全丢。
测试用**不同内容**的两份（包 vs loose）钉住了这个优先级。

#### 新测试 `crates/sm-service/tests/media_thumbnails.rs`（5 条）

目录布局（含分片）、无归属时的 422、`(offset, id)` 排序、解不出尺寸时的降级、
包的字节优先级（含回退）。

> 顺带把三个测试文件重复的播种函数（`seed_library` / `seed_media` /
> `seed_image` / `seed_thumbnail`）收进了 `tests/support`。

### `artifacts::persist` 落地（`artifacts.rs` 已清零）

三段式：写临时包 → `.bak` 备份后原子替换 → 登记 DB（失败**回滚包**）→ 清备份。

#### 新加了一个 sm-db 的动词：`UnitOfWork::record_thumbnail_artifacts`

登记必须是**一个事务**：一媒体有十几张缩略图，逐条提交时中途失败会留下「包里
12 条、库里 3 条」——那 9 条是**幽灵**（客户端看得到、按库算的列表与清理看不到）。

`Ctx::begin` 不是公开入口（`Ctx` 不持有事务，只有 `UnitOfWork` 持），所以按
`ctx.rs` 既有的设计（「它按**动词**暴露方法，每个方法内部编排多个仓储的 `_in`
变体」）加了一个动词。它与 `generate_thumbnail` 的区别是**不碰 `media` 的状态机**
（那个是任务路径，落地即推进 `succeeded`）。

#### 两处与上游不同的选择，都写了理由

1. **用 upsert 而不是裸 insert**：上游是 `Image.create` + `MediaThumbnail.create`，
   同一媒体重新生成会撞 `(media_id, offset)` 与 `origin` 两个唯一索引 —— 上游靠
   调用方先清干净回避。表的约束本来就是按 upsert 设计的（仓储文档写了理由）。
2. **偏移收窄用 `i32::try_from` 而不是 `as`**：截断会让一个超大偏移**悄悄变成
   另一个时刻点**。这条恰好成了测试里制造「登记失败」的手段。

#### 新测试 `crates/sm-service/tests/thumbnail_persist.rs`（5 条）

最值钱的一条是**回滚**：用 `i64::MAX` 的偏移让「包已替换之后」的登记失败，然后
断言 ①包退回**第一次**落盘的那份字节、②失败批次里**先插入**的那条（offset 180）
也被回滚掉。没有事务的话它会留下 —— 而包已退回旧版，于是它成了指向不存在条目的
幽灵。

另外三条：`videos/<video_item_id>/` 命名空间 + `SKIPPED` 状态位（造了 `video_item`
行）、空输入不触碰任何文件（「顺手删旧包」是错的）、同偏移重放幂等（`created_at`
不被改写）。

#### 顺带抽走了两处重复

`temp_pack_path` / `backup_pack_path` / `cleanup_stale_temp_files` 收进
`image_store`（`movie_asset_pack` 与 `thumbnails/artifacts` 各自都写过一遍）。

⚠️ `ThumbnailArtifact` 的**形状仍是骨架自造的**（`path: PathBuf` +
`thumbnail_id`）。上游来自插件 `provider_protocol`，是 `relative_path: str`
且**没有** `thumbnail_id`。插件 ABI 落地时按彼时的协议改，别顺着现在的实现。

### 缩略图任务的计数/重置落地 —— 同时修掉**三处数值与集合的错**

这一批最值得记的不是"实现了四个查询"，而是那四个查询的**判据此前是错的**，
且错的方式全都是**静默**的。

| 项 | 骨架 | 上游 | 错了会怎样 |
|---|---|---|---|
| 最小可接受数量 | **60% 向上取整** | **85% 向下取整** | 每一部短片都判失败（编译器永远抓不到）|
| 退避曲线 | **指数**（`base << n`）| **线性**（`base × n`）| 0/1 次看起来一样，第 3 次起差一倍 |
| 失败轨终态边界 | `old + 1 > 2` | `new >= 2` | **多送一次重试**（第 3 次才终态）|
| 终态错误码集合 | 7 个，含 `provider_not_installed` / `media_not_found`，缺三个 `thumbnail_generation_*` | 上游那 7 个 | ① provider 没装 → 进终态 = **装好也不再试**；② 三类确定性失败被反复重试 |
| 候选集 | 「状态 = `pending` 的数量」| 见下 | 见下 |

#### ★ 候选集**包含 `succeeded`**，这条最容易读错

上游 `_candidate_query` 是三条的并 + 一个 `NOT EXISTS`：

```text
  valid = true
  且 该媒体一张缩略图都没有
  且 (状态 ∈ {pending, succeeded} 或 状态 = retry_wait 且已到期)
```

里面的 `succeeded` 是**修复路径**：状态机说「做完了」而产物不在（包被删、磁盘
换了、写库成功而落盘失败）时，必须能重新扫到它 —— 否则它永久停在 `succeeded`
而永远没有图。而「已经有缩略图」这一条把正常的成功媒体挡住，所以不会反复重做。

同理 `reset_terminal_media` 的三个 WHERE 条件都不能少：漏 `state = terminal` 等于
**白送重试额度**；漏 `NOT EXISTS` 会把**已有产物**的媒体重置成待处理，下一轮重做。

#### 落地方式

- `sm-db` 加三个方法（`count_thumbnail_candidates` / `count_thumbnail_state` /
  `reset_terminal_thumbnails`），SQL 与判据都写在那儿并在文档里列了逐条理由；
- `MediaThumbnailTaskService` 改成持有 `Db`（骨架是无状态单元结构体）；
- 新增 `deferred_backoff_seconds`（延迟轨用自己的基数，上限共用 24 小时）。

#### 新测试 `crates/sm-service/tests/thumbnail_task_counts.rs`（6 条）

最值钱的一条：**候选集包含「状态说成功、产物却不在」的媒体，且一旦有产物就退出
候选** —— 两个方向都断言了（只测一个方向的话，「恒真」的错误实现也能过）。
另有退避窗口/`valid`/终态三档过滤、重置的四类条件、以及「重置后重新进候选」
这条闭环。

> ⚠️ `generate_pending_thumbnails` **仍未接线**，且是**同一类阻塞**：
> 上游第一步是 `MEDIA_PROVIDER_REGISTRY.storage_for(...).generate_thumbnails(...)`，
> 而本仓的 registry **只存在于注释里**（`sm-api/src/routes/{videos,
> video_collections}.rs` 提到它）。没有 provider 就生成不出产物，**不假造**。

### `import_task` 的契约层 + 入队落地（本轮）+ `download_tasks` 收尾（上一轮）

两轮的落点都在 `transfers`：

| 轮 | 落点 | 结果 |
|---|---|---|
| 上一轮 | `download_task.rs` 的台账响应改回上游形状、`list_tasks`、重复 query 参数、`trigger_import` 的两道门 | ⚠️ 202 正常路径当时会 panic（`enqueue` 是 `todo!()`）|
| 本轮 | `import_task.rs` 的 **6 个 DTO 全部重写** + `enqueue` / `enqueue_batch` 落地 + `sm-api` 路由改单一 DTO 来源 | 202 真的建出 TaskRun（但跑不起来，见下）|

#### 契约层：骨架期那 6 个 DTO **六个全错**

对照 `schema/transfers/media_import.py`（行号见文件内表格）逐条修了：
`ImportRequest`（`media_kind` 是小写 `jav`/`video`；`source_disposition` 是
`keep`/`delete_after_commit`/`in_place`，**没有 `move`**；**删掉自造的
`operation_namespace`** —— 上游那是执行参数，不是请求字段）；
`ImportAcceptedResponse` 补 `task_key` / `state`；`ImportFailedItemResource`
从自造的 5 字段换成上游 **13 字段**；`ImportMetadataSearchResponse` 去掉
`item_id`、补 `source_errors`；`MetadataCandidate` 换成上游 10 字段
（**删掉自造的 `confidence` / `date`**）；`ImportExecuteSummary` 的字段名
按上游 `ImportResult` 逐字改（`imported_count` …）。另补了
`ImportMetadataSearchRequest` / `ImportFailedItemRetryRequest`。

**`sm-api/routes/media_import.rs` 里那套内联 DTO 已全删** —— 它自带一份与
service 层不同、且与上游都不同的第二/第三份形状。现在导入取
`import_task`、浏览取 `provider_browse`。

#### 按库互斥：新增 `TaskQueueService::enqueue_with_mutex_key`

`enqueue` 原本把 `mutex_key` 写死成 `aps:{task_key}`，而导入的互斥键必须按
**媒体库**（`library_import:{id}`，`import_write_mutex.rs` 早就有那把键）。
所以给队列加了一个**显式互斥键**的入口，`enqueue` 退化成它的薄封装。
`import_task` 与以后的 `media_transfer` 走新入口。

> ⚠️ 空白互斥键当场拒绝：仓储层的 `normalize` 会把空白**静默**归一为 `NULL`
> （= 不参与互斥）—— 那对「本来就不要互斥」的调用是对的，对按库互斥是
> **静默失效**（两批文件互相覆盖）。

#### ★ 一处刻意偏离：入队与回写下载任务**不是一个事务**

上游把「建 TaskRun + 回写 `download_task.import_status/import_task_run`」放在
同一个 `atomic()` 里。本仓这两笔分属两个仓储、都只走连接池（没有 `_in`
变体），真要原子得加一批 `_in` 方法并让队列破一次「唯一入队路径」。

本轮**用补偿代替回滚**：入队成功但回写失败（或批量占用数量不符）时，把刚建的
TaskRun **显式判失败**（顺带释放互斥键），再返回上游的 409/502。唯一可见差异：
上游回滚后没有那行，本仓留下一条**失败**的 run（任务中心可见、不发通知）。

**理由是不可逆的那一侧**：宁可留一条失败的 run，也不能留一条占着
`library_import:{id}` 的 pending run —— 互斥键不释放会让该媒体库**永久 409**。
后续项：补 `_in` 变体 + `UnitOfWork`，把这处收回原子性。

批量的「全有或全无」不需要事务：`DownloadTaskRepository::mark_import_started`
用**单条带计数谓词**的 UPDATE，条件不成立时影响 0 行（fail-closed）。

#### ⚠️ 本轮之后仍跑不起来：handler 未注册

`ImportTaskService::execute` 仍是 `todo!()`（要 `import_service` +
`catalog_import`），`sm-scheduler` 的处理器注册表里**没有** `library_import`。
于是 `POST /imports` / `POST /download-tasks/{id}/import` 会真的建出 TaskRun
（202），随后被 worker 领取并以 `WorkerError::NoHandler` **判失败**。
这是阶段性事实，不是回归 —— 任务在任务中心里能看到那条失败记录。

其余三个 `todo!()`（`search_failed_item` / `enqueue_failed_item_retry` /
`execute`）各自的缺口写在模块文档的表格里；注意上游第三个 mode 名是
`retry_failed_file`（骨架期注释写的 `retry_failed_item` 是错的，已改）。

新增测试：`crates/sm-service/tests/media_import_enqueue.rs`（11 条真库用例，
含「两个库互不阻塞」「批量整批拒绝且一条都不改」「撤掉的台账必须释放互斥键」）、
`repo_integration.rs` 的 `mark_import_started_is_all_or_nothing`、
`task_queue.rs` 里两条显式互斥键的钉子用例。

#### 失败项**读取**路径也落地了（`list_failed_items`）

`GET /imports/{task_run_id}/failed-items` 从 `todo!()` 变成可用。要点四处：

1. **失败项没有表**：它们在 `background_task_run.result_summary` 的
   `failed_files` 数组里（上游 `import_task_service.py:178`），随任务结果一起
   保存 —— 它们的生命周期与那次任务完全相同。
2. **404 有两个条件**：行不存在，**或** `task_key != library_import`。后者不是
   洁癖：`{id}` 是裸整数，缩略图/图搜/相似度的 id 都能填进来，而那些任务的
   `result_summary` 是别的形状 —— 放行会让「拿错 id」看起来像「这次没有失败项」。
   （刻意不复用 `TaskRunService::get_task_run`：它报的是另一个码
   `task_run_not_found`，而且不看 `task_key`。）
3. **形状读不出来是 500，不当空列表**：`result_summary` 不是 JSON / 不是对象 /
   `failed_files` 不是数组 → 500。静默当空会让「这一趟全失败了」与「一切正常」
   长得一模一样，而用户再也没有入口重试那些文件。
4. **`can_manual_search` 是算出来的**（上游 `:530-535`）：`pending` + 是视频 +
   JAV + 原因 ∈ {`movie_number_not_found`, `metadata_fetch_failed`} 四条全满足
   才为真 —— 前三条挡「点了没意义」，第四条挡「点了也修不好」。

投影用 `StoredFailedItem`（serde）而不是手挖键：缺键/类型不对由 serde 报出
字段名，落 500，与上游 `item["x"]` 的 `KeyError` 同码；`source_ref` /
`library_id` / `media_kind` / `name` 这些宿主内部字段**不声明进这个结构**，
免得有人顺手外发（`source_ref` 里可能有真实路径）。

#### ★ 顺带修掉一处**自造契约**：失败原因码

`sm-service/import_service.rs` 里那份 `failure_reason` 是骨架期编的六项：
`is_collection`（那是 `Movie` 的**字段**，不是失败原因）、`unsafe_filename` /
`stage_failed` / `finalize_failed`（上游这三类都落 `media_import_failed`）
**上游都没有**，而真的十项里少了八个。

后果不是「多几个常量」，而是**读侧认不出来**：分类表
（`failed_file_kind`）没有这些键，它们会掉进 `file` 这一档 —— 于是「主动跳过」
被渲染成「可删除的文件级失败」。

现在：`sm-db::transfers::downloads` 新增 `failed_file_kind`（四档 + `classify`，
含上游显式锁死分类的**两条历史 reason**）与 `failure_reason`（十项），
`import_service` 改成 `pub use` 转发（一份定义），`import_task` 的两个可修原因
也引它。`kind` 在**读侧是原样读存储值、不重算** —— 重算会让改一次分类表就与
存量数据不一致。

#### `import_notifications` 落地（+ 改回上游形状）

`create_new_media_reminder` 从 `todo!()` 变成可用，同时**删掉一个自造形状**：

| 骨架期 | 上游 |
|---|---|
| `NewMediaReminder { title: "本次导入新增 N 部影片", items: [...] }` —— 自造资源 + 逐部条目 | 通知的 `title` 是**固定文案**「有新的影片可以播放了」，正文「新增了 N 个影片」，**没有逐部条目** |
| 不去重，且截断到 **20 部**（注释自称「这是刻意的差异」）| **按 `movie_number` 去重**、不截断（`:14-23`）|
| `related_task_run_id: Option<i64>` | `Option<i32>` |

那个 20 部上限的理由（「逐部展开 200 行会让通知中心变成列表页」）在形状改回上游
之后**不复存在** —— 正文只有一个计数。`handoff.md` §五 只登记了两处刻意照抄的
缺陷，这一处不在其中（骨架期凭空加的）。

两条落库路径都要保留：带 task run 走 `create_once`（幂等键
`download_import_new_media:task_run:{id}`，同一 TaskRun 重放只留一条），不带
task run 走 `notify`（**不去重**，通用入口的旧行为）。

★ **一处照抄的缺陷**：上游读的是 `movie_items[].movie_id`，而写入侧
（`new_playable_movies`，`import_service.py:466-468` 与 `:718-723`）给的键是
`id` —— 所以 `related_resource_id` 线上**一直是 `None`**。照抄，别「顺手修」成
读 `id`（那会改变通知挂的关联资源）。

接线时的三条约束（`import_task_service.py:287-299`，本轮未落地）已写进模块文档：
只有**下载任务发起**的导入才发提醒（判据是 `params` 里有 `download_tasks` 或
`download_task_id`）；传的是 `reporter.task_run_id`；**提醒失败不能让导入失败**
（上游把整个调用包在 `try/except` 里只记 warning）。

#### `movie_asset_pack_backfill` 落地（catalog 33 → 31）+ 第 4 个 worker handler

上游 `catalog/movie_asset_pack_backfill_service.py`(181) —— 又是一处**自造形状**：

| 骨架期 | 上游 |
|---|---|
| `BackfillCandidate { movie_id, movie_number, movie_dir_relative, image_record_count }` + `should_backfill(记录数, 包在否)` 纯函数判据 | 候选只有**番号**：**库**决定候选（封面 / 薄封面 / 剧照三处并集去重），**磁盘**决定这一部怎么处理 |
| `PackBackfillStats { examined, packed, skipped_missing_files, failed }` | 六个键：`candidate_movies` / `packed_movies` / `cleaned_movies` / `already_packed_movies` / `skipped_missing_files` / `failed_movies` |

三档必须分开：**新建包**、**已有包但清了残留散文件**、**干净跳过**。
混起来就看不出回填到底在「建包」还是在「擦屁股」。缺文件仍是**跳过**
（等 `image_cleanup` 清理），只有重建失败才计 `failed_movies`。

顺带三件：
- `MovieRepository::list_numbers_with_asset_images()`：三次往返 → 一条 `UNION`；
- `sm-scheduler` 注册第 4 个 handler（**3 → 4**）。它是**纯本地**的长任务
  （不依赖 Qdrant / 推理 / 插件），而且 `manual_only`（无 cron）—— 不注册
  handler 等于这个功能完全不存在；
- `MovieAssetPackService` 补 `Debug + Clone`，并改掉它文档里「全仓没有任何
  调用点」那句（现在有两个调用方）。

`loose_files`（列散文件）与 `movie_asset_pack::remove_loose_files`（删散文件）
**刻意不合并**：判据差一个比特 —— 删除那版还认符号链接（上游两处本来就不同）。

新增 `crates/sm-service/tests/movie_asset_pack_backfill.rs`（5 条，真库 + 真图片根）。

#### 手动触发链路收口 + 互动数同步编排（catalog 31 → 28，handler 4 → 5）

四处，都在这条「cron / 手动任务」线上：

1. **`movie_task.rs`（2 处）**：`recompute_movie_heat` 按番号定位（404）→ 以
   `ConflictPolicy::Raise` 入队 → 撞上在跑的同类任务 **409
   `movie_heat_recompute_conflict`**（details 带 `blocking_task_run_id`，取不到时
   是 JSON null）；`execute_movie_heat` 返回上游那四个键。
   ★ 入队参数是**番号**不是 id：骨架期注释写反了（「执行时不该再按番号查」），
   上游 `:23` / `:42` 恰恰是**两次按番号查** —— 任务会排队，而排队期间合并会让
   `movie.id` 换行，`movie_number` 才稳定。
   `POST /movies/{n}/heat-recompute` 顺带从 `todo!()` 接到 202。
2. **`ManualJobTriggerResponse` 三份定义合一**。上游只有
   `{task_run_id, task_key, state}`，而骨架期 `catalog/movie_task.rs` 那份是
   `{task_run_id, task_name, trigger_type}` —— **后两个键上游没有**，且
   `trigger_type` 在请求侧就定了（这个端点恒为 manual），回显没有信息量。
   现在定义只有一份（`system/jobs.rs`），`routes/jobs.rs` 与 `movie_task` 都用它。
3. **`movie_interaction_sync::run()` 落地**：候选 SQL（上游那个四路 OR +
   `javdb_id IS NOT NULL`）搬进 `MovieRepository::list_interaction_sync_candidate_ids`；
   单部失败不中断、失败 id 进 `failed_movie_ids`；八项计数与进度文案逐字对齐上游。
   ★ **「JavDB 上查不到」记 `failed_movies`** —— 骨架期自造了一个 `not_found`
   键，而上游 `:123-130` 是把它计入 failed 的（日志写 skipped、计数是 failed）。
   按错形状接线会让运维看到「失败 0」而实际有一批影片没同步。
   ⚠️ 两个 trait（provider / writer）**还没有宿主实现**（要插件 ABI 与
   `catalog_import`），所以它的 handler 仍未注册 —— 接线时只需实现那两个 trait，
   本文件一行不用改。
4. **第 5 个 worker handler：`movie_heat_update`** —— 有 `params` 只算一部、
   没有则全表（上游 `_run_movie_heat` 的两分支，`params` 的「空」按非空对象判）。
   两条分支缺一不可：少了前者，手动重算会变成扫 30 万行。

#### 订阅演员影片同步落地（catalog 28 → 27）

`subscribed_actor_movie_sync::sync_subscribed_actor_movies` + 四个演员仓储方法
（`list_subscribed_for_sync` / `list_merged_source_targets` /
`mark_subscribed_movies_synced` / `has_actor_movie`）：

- **全量/增量判据不是时间**：`subscribed_movies_full_synced_at` 为 `NULL` → 全量；
  否则增量靠「翻到库里已有关联的那部就停」。骨架期那个 trait 签名带
  `after: Option<NaiveDateTime>`（「上次同步到的时间」）—— **上游没有这个入参**，
  已改成上游的 `(javdb_actor_id, actor_type, page)`。
- **合并来源演员的作品也要抓**：`targets` = 保留记录 + 以它为目标墓碑的演员，
  否则合并会让来源演员的作品永远不再补录。
- **单片失败按影片跳过**（记 warn 继续），那位演员仍算成功；
  「取详情 + 入库」两处都失败才少一部 `imported_movies`。
- ★ 统计改成上游那四个键（`total_actors` / `success_actors` / `failed_actors` /
  `imported_movies`）—— 骨架期那套把**演员数**与**影片数**混在一个 `actors` 里，
  于是「失败了几个演员」这个数字根本不存在（它的 `failed` 记的是影片）。
- 两个时间戳的写法：`subscribed_movies_synced_at` 总是推进；
  full 用 `COALESCE(full, $2)` 表达上游「只在原本为 NULL 时写」——
  **不要**把读到的值传回来（读→写之间隔着整个同步过程，会覆盖别人的值）。
  `updated_at` 不动（上游 `save(only=[...])`）。
- ⚠️ 两个 trait（provider / importer）**还没有宿主实现**（要插件 ABI 与
  `catalog_import`），所以 `actor_subscription_sync` 的 handler 仍未注册。

#### 竖封面回填 + JavDB 补录（catalog 27 → 24）

两个 backfill 任务的编排落地，配套五个仓储方法（`list_missing_thin_cover` /
`list_javdb_backfill_pending` / `find_javdb_backfill_pending` /
`list_javdb_backfill_candidate_ids` / `postpone_javdb_check`）：

1. **`movie_thin_cover_backfill`**：统计改成上游那四个键
   （`scanned/updated/skipped/failed_movies`）；trait 参数 `movie_id` 由 `i64`
   改成 **`i32`**（`movie.id` 是 integer，骨架期写成 i64 会在调用点漂移）；
   ★ 上游这个方法**没有 reporter 参数**，所以也不收 progress。
2. **`movie_javdb_backfill`**（2 处）：
   - `pending()` **不含** `next_check_at` 条件 —— 那是 `run()` 才叠的
     （上游 `pending()` 返回的是未执行的查询）；删掉自造的 `last_attempt_at`
     （`movie` 表上只有 `javdb_next_check_at`，那是**下次**检查时间）。
   - `run()`：条间 sleep 2s（**第一条不 sleep**，上游 `if current > 1`）、
     ★ **推后检查时间在 `finally`（成功/未收录/失败三档都推后 7 天）** ——
     少了它，一条死掉的影片会每轮都占掉 50 个名额之一；
     「已不再是候选」的那条 `continue`（不计数、不推后）。
   - 统计改成上游四键（`candidate/succeeded/not_found/failed_movies`）。
   - 进度文案的「已完成」是**循环计数**而不是三项之和（有 `continue` 分支，
     用和会倒退）。

⚠️ 这两个任务的 trait 同样**没有宿主实现**（竖封面要 image store；
JavDB provider 要插件 ABI），handler 仍未注册。

##### catalog 剩下的 24 处：两条硬依赖

| 被挡的 | 处 | 挡它的 |
|---|---|---|
| `metadata_source`(5) · `movie_metadata_search`(3/4) | 8 | **插件数据面接缝**：`fetch_movie` 要「向插件索取 → 交付目录 → `use(delivery)`」，`sm-plugins` 还没接住那份载荷 |
| `catalog_import`(7) · `movie_metadata_refresh`(3) · `movie_metadata_search`(1) | 11 | 上游的 `CatalogImportService`（详情 → 入库），它自己又依赖 provider 详情模型与主权网关 |
| `movie_image`(5) | 5 | **image store**：下载/落盘/切割（cv2）本仓还没有这一层（同 `moment_recommendation` 的阻塞项） |

在这两条落地之前，这 24 处只能**凭印象写** —— 与本项目「上游是唯一权威」
的硬约束冲突，所以不做。

#### 插件架构 P1–P4：契约仓成为「作者只需依赖它」的完整契约

一口气做完四块（前置 P0 在 `22ea79e`）：

1. **P1 交付校验迁进契约仓**：`movie_delivery`（7 类判据 + 清理 + 8 条测试）
   从 `sm-plugins` 迁到 `sm-plugin-api`。判据是**双方都要遵守**的规则 ——
   作者能在自己的测试里 `use sm_plugin_api::movie_delivery::*` 自检。
   `sm-plugins` 侧改为再导出（`pub use sm_plugin_api::movie_delivery;`），
   既有引用点不动。番号一致性校验仍不在这里（番号归一化属于业务概念）。
2. **P2 错误码过线**，闭合 `provider_calls.rs` 自陈的缺口：
   `ProviderError` / `ProviderErrorCode` **proto 里本来就有**
   （`proto/common.proto:303-329`），缺的是通道 —— 现在由
   `sm_plugin_api::error::{to_status, from_status}` 编进 `Status::details`
   （**不改任何 rpc 签名**），`classify_status` **先试结构化**、
   解不出才按 gRPC 码猜。`retryable` 现在**优先信 provider 说的**
   （新增 `ProviderOperationError::provider_retryable`），猜的那份退成兜底。
   `plugin-ref-local` 里给了一处正确示范（作者照抄）。
3. **P3 接 `PluginHost`**（上游 `PluginContext`，proto 36 个 rpc 此前 0 引用）：
   `sm-server::plugin_host` 起服务端，端点经
   **`SAKURAMEDIA_HOST_GRPC_ADDR`** 注入（新增 `LaunchSpec.host_endpoint`）；
   **只接了 3 个只读 rpc**（`GetMovie` / `FindMoviesByNumbers` / `GetActor`），
   其余 33 个显式 `unimplemented` 并登记在模块文档的缺口表里 ——
   剩下那些大多是**写操作**（要走主权网关 / 业务 service），
   只读快照错了最多是空值，写错了会改坏用户数据，所以按组逐个接。
   `owners` / `revision` 取自 `field_owners` / `mutation_revision`，不猜。
4. **P4 贡献者物料**：修掉 `plugin-ref-local` **虚报的 Download 能力**
   （声明了但全 crate 无 `DownloadProvider` 实现，照抄的人会被误导；
   注册期不校验"声明 ↔ 实现"，所以虚报**不会被发现**）；
   补 `docs/plugin-abi.md`（`sm-plugin-api/src/lib.rs` 早就引用了它，但文件不存在）
   与 `docs/plugin-author-guide.md`（最小骨架、三扩展点、默认实现层、
   交付目录、结构化错误、回调宿主、自检清单）。

⚠️ 顺带记一条**仍存在的缺口**（登记在 `docs/plugin-abi.md`）：注册期**不**校验
「声明的能力 ↔ 是否 serve 了对应 service」。

#### 插件打通后的兑现：`metadata_source` 落地（catalog 24 → 21）

「先解决插件」的收益点到了：给 `sm-service` 加**契约层**依赖（`sm-plugin-api`，
叶子，不成环）之后，`metadata_source` 的 3 处可以从 `todo!()` 变实现（另 2 处
仍缺 `catalog_import`）：

1. `enabled_plugin_sources(config)`：按 **`plugins.enabled` 的顺序**过滤已注册
   来源（上游 `:61-69`）。顺序即优先级 —— 兜底链路按它逐个试。
   端点与数据目录不来自配置（来自注册表与目录约定），所以 `RegisteredSource`
   由**组合根**构造，这里只负责「配置说启用谁」。
2. `fetch_plugin` → 新增 `load_plugin`：建 `<data_dir>/metadata-tmp` → 发
   `FetchMovie` → `found=false` 判「没收录」→ `validate_movie_delivery`（契约仓）
   → **番号一致性**（`normalize_movie_number`，上游 `:131-134`）→ 闭包消费 →
   `cleanup_delivery`。
3. `fetch`：JavDB 优先（**没收录不算错**）→ 按启用顺序逐个试插件
   （没收录 → 下一个；坏了 → 记进 failures）→ 全试完：有真实失败报
   `RequestFailed(failures.join("; "))`，否则 `NotFound`。

⚠️ 三处如实登记的缺口：
- **Pillow 校验**（上游真的把每张图解码一遍）本仓没有图像解码依赖，未实现 ——
  判据到「是普通文件、在交付目录内」为止；
- ~~`source` 只存 `"javdb"` / `"plugin:<id>"` 一个串~~ —— ✅ **已解决**：
  改成 `DeliverySource` 枚举（见下面「`import_by_number` 落地」那一节）；
- 单次索取给了 30 秒上限（上游无上限），理由写在常量文档里。
- ~~另 2 处（`import_by_number` / `match_actors`）仍缺 `catalog_import`~~ ——
  `match_actors` 随后就接上了；`import_by_number` 见下面
  「`import_by_number` 落地」那一节。

#### `catalog_import` 落地：兜底链路第一次能真正落库（catalog 21 → 14）

7 处里落了 6 处，另 1 处（`backfill_movie_thin_cover`）被 image store / cv2
挡住，保留 `todo!()` 并登记。

**先纠错**：骨架期文档写的是「已存在时**只补空字段**」，而上游
`import_movie_if_missing`（`:115-293`）根本没有这个策略 —— 它的分支是
「已存在且无 `javdb_id` 且有 `metadata_source` → 转 `backfill_plugin_movie`；
否则命中就**一个字段都不写**；不存在才建」。上游的"不覆盖用户手改值"是靠
**主权网关**实现的（受保护字段只在无人接管时才被写），不是靠判断目标列空不空。

内核优先：`update_movie_fields` 是四者的公共底（白名单 + 变更检测 + 分流写 +
写后重读回流），其余三个只是**字段集与"已存在就返回 / 覆盖"策略不同** ——
主权校验因此只存在一份，不会四个方法各写一遍而漂移。

- `update_movie_fields`：`fields` 非空 + 去重 + 必须落在
  `_MOVIE_FIELD_UPDATE_MAP` 那 **9 个**字段内（`:470-480`），否则是调用方 bug；
  值相等跳过；受保护四列走 `MovieOwnershipGateway::update_host_unowned`、
  五个计数走 `MovieRepository::update_interaction_counts`；受保护那支写完后
  **重读该行**把真正变化的字段回流（不虚报，上游 `:536-542` 同）。
- `backfill_plugin_movie`：番号一致性 + `javdb_id` 冲突两处显式校验 →
  写 JavDB 那一份 → 清空 `javdb_next_check_at`。
- `refresh_movie_metadata_strict` = 全 9 字段覆盖式，复用内核。
- `upsert_actor_from_javdb_resource`：按 `javdb_id` 建或更新；`gender` 只在
  `update_gender` 且值 ∈ (1,2) 时经 `ActorOwnershipGateway::update_host_source`
  带 `host:javdb` owner 写（网关拒绝人工 owner）。
- `CatalogImport` 窄接口改成 `async`（写入要查库），`metadata_source` 的
  `import_by_number` / `match_actors` 接上 —— **导入必须在 `fetch` 的闭包内做**
  （插件那支的交付目录在闭包退出后立刻清理）。
- 常量去重：`JAVDB_CHECK_INTERVAL_DAYS` 只留 `catalog_import` 那一份，
  `movie_javdb_backfill` 改为 `pub use`。

⚠️ **两处如实登记未接线**：① `force_subscribed`（`NewMovie` 没有订阅两列）；
② 图片落盘与演员 / 标签 / 剧照关联（缺 image store 与三个关联表写入方法）。
在那之前**不写** `cover_image_id` —— 宁可没封面，也不指向不存在的文件。

配套仓储方法：`update_interaction_counts` / `apply_javdb_backfill` /
`set_plugin_metadata_source` / `update_javdb_profile`。

#### `import_by_number` 落地：**入库路径**的第一段通了

上游 `import_by_number`（`metadata_source_service.py:30-44`）是「按番号取元数据
并入库」的入口，骨架期是 `todo!()`。这次补上，并顺手解掉它暴露的两处登记缺口：

- **`source` 从「一个串」改成枚举**（`DeliverySource::Javdb` /
  `Plugin { plugin_id, display_name }`）。原来只存 `"plugin:<id>"`，而
  `import_plugin_movie` 要往 `movie.metadata_source` 写
  `{plugin_id, display_name, source_id, source_url}` 四个键 —— `display_name`
  在那个串里**根本没有**，`plugin_id` 也只能靠拆前缀（自造格式，改前缀即静默失效）。
  插件的两个身份字段本来就在 `fetch` / `fetch_plugin` 手上，只是构造交付时被丢了。
- **`CatalogImport` 窄接口补两支**：`find_movie_id`（短路用）与
  `import_plugin_movie`（插件来源那一支）。窄接口原本只有 JavDB 支，**插件交付
  没有入库入口** —— 那正是「插件元数据没处写」的一半。

编排照抄上游：查已存在 → **短路** → `fetch`（JavDB 优先、插件兜底）→ 按来源
分派。两处**必须在闭包内**入库的继续在闭包内（插件那支的交付文件在闭包退出后
立刻清理）。插件交付经 `import_detail_of` 翻译成 `create_movie` 认的那组键。

测试：4 个单测（键名与 `create_movie` 读的对齐、可选字段给 `null` 而非省略、
来源身份四键、缺 `source_url` 保持 `null`）+ 3 个真库集成测试
（`crates/sm-service/tests/metadata_source_import.rs`）：

| 用例 | 断言 |
|---|---|
| JavDB 命中 | 建库成功、`javdb_id` 写进去了（这是它与插件支的**唯一**区别） |
| **已存在的番号** | 返回同一部、`created = false`，且**来源一次都没被再问**（用调用计数证伪 —— 「没有新建」可以由很多原因造成） |
| 谁都没收录 | `NotFound` 而不是 `RequestFailed`（前者是正常结果，后者会被当故障重试） |

⚠️ **仍未做，且其中一条需要拍板**：

1. **`impl MovieMetadataImporter for MetadataSourceService`**（`transfers/import_service`
   的窄接口，`MediaImportService` 靠它拿元数据）。它**卡在一个接口冲突**：
   那个 trait 的两个方法签名里**没有 config**，而 `fetch` / `fetch_plugin` 必须看
   `plugins.enabled` 的顺序。两条走法 ——
   **(a)** `MetadataSourceService` 构造时持有 config 快照（与上游的全局
   `settings` 同形，但把配置固化在装配期）；**(b)** 给 trait 的方法加 config 参数
   （会一路波及 `MediaImportService` 的调用点）。**未拍板前不动**：改错方向要
   返工一整条调用链。
2. **插件那一支的端到端**：要真起 gRPC 插件进程，属于跨仓集成测试
   （`docs/tasks/proto-p1-gaps.md` §2.5 登记过那笔账）。
3. `import_by_candidate`（对应 `movie_metadata_search::fetch_candidate`，仍是
   `todo!()`）。

#### 图片落盘层落地：`svc-image::paths/store` + `movie_image`（catalog 14 → 9）

之前一直说「被 image store / cv2 挡住」，实际核下来是**两件不同的事**：
cv2 的替代（`svc-image::cover_split`，Sobel 书脊检测）**早就有了**；缺的是
「文件落在磁盘哪儿、怎么落」那一层。这次补的就是它。

1. **`svc-image/src/paths.rs`（新）** —— 落盘布局规则，与上游
   `common/media_paths.py` 逐字对齐：`movies/<shard>/<番号>/{cover,thin-cover,plot-<i>}<ext>`、
   `actors/<safe><ext>`、`assets.zip` / `thumbnails.zip` 的包路径判据。
   ★ **分片拿归一化后的目录名去算**（`sha1[:2]`），先分片再归一会让同一部片
   散到两个 shard —— 而路径一旦写进 `image.origin` 就不会再改。
   剧情图**平铺**（30 万规模下不建 30 万个 `plots/` 空目录）。
2. **`svc-image/src/store.rs`（新）** —— 原子落盘（同目录临时文件 → fsync →
   rename；跨设备 rename 会退化成拷贝、原子性就没了）、单文件读取、
   竖图判定、薄封面切割（用 `cover_split`）。
   ⚠️ **包（zip）读写未实现**：判据在 `paths::image_pack_relative_path` 就位，
   读写缺 zip 依赖。单文件那一路在任何情况下都正确，包是优化。
3. **`movie_image`（5 处 todo 全落）**：下载 6 次重试 / 30s 超时（阻塞式出网
   走 `spawn_blocking`，不占异步线程）→ 临时文件 → 原子落盘 + `ImageRepository::upsert`
   登记。★ **任一任务彻底失败 → 整体 Err**：`image` 记录一旦建立就没有重试机会
   （下次导入认为「已有」），少一张图比一张裂图代价小。
   薄封面「**先切封面**，切不出来才回退前两张剧情图里的第一张竖图」。

⚠️ 仍保留的：`catalog_import::backfill_movie_thin_cover`（要写
`movie.thin_cover_image_id` 的窄更新 + 重建 assets.zip）；
`resolve_thin_cover_from_existing_movie` 的剧情图回退分支（缺 `movie_plot_image`
按影片查询）。

#### transfers 开荒：`download_resource_hash` 落地 + 域内挡点普查（26 → 25）

先落了唯一不依赖 provider 的一处（`torrent_hash`）：reqwest 流式 GET，
重定向限 5 次、**边读边判** 10 MiB（先下完再判就失去意义：一个 10 GiB 的畸形
种子早把内存撑爆了），然后交给 `svc_hash::torrent_v1_info_hash`。
HTTP 状态映射直接取 `ResolveError` 那张表的变体（404 → `SourceNotFound`、
其余非 2xx → `SourceUnavailable`），**不重映射**。

##### ★ 普查结论：transfers 剩下 25 处几乎全被**同一个**挡点卡住

transfers 的 todo 注释里反复写着「等 provider seam」，这不是敷衍 —— 核下来
确实如此。宿主→插件那几个 rpc 的 **Rust 侧签名还没有**（`scan_import_source` /
`stage_import_file` / `finalize_import` / `compute_file_hash` / `browse` /
`plan_playback` / `submit` / `delete_media`），而它们决定了：

| 文件 | 处 | 卡在哪个 rpc |
|---|---|---|
| `download_client.rs` | 5 | `submit` / `delete_media` / 状态快照 |
| `download_common.rs` | 2 | 客户端解析 + `submit` |
| `download_sync.rs` | 4 | 下载状态快照 |
| `media_transfer_task.rs` | 4 | `media.provider` 能力协商 + 复制 |
| `import_service.rs` | 3 | `scan_import_source` / `stage_import_file` / `finalize_import` |
| `import_task.rs` | 3 | 按番号搜元数据候选（`metadata_source`） |
| `auto_download.rs` | 1 | `submit` |
| `download_request.rs` | 1 | 解析唯一客户端 + `submit` |
| `download_task.rs` | 1 | provider 删远端 |
| `provider_browse.rs` | 1 | 解析媒体库的 provider |

##### ★ 已完成：provider seam 的决策与缺口清册

见新 ADR `docs/adr/2026-10-07-provider-seam.md`。核心结论：
**`sm-service → sm-plugins` 现在不成环**（P0 把 `sm-plugins → sm-scheduler`
拆掉后，`sm-plugins` 只依赖 `sm-core / sm-plugin-api / sm-db / tonic`，且全
crate 无 `sm_service` 引用），所以允许 `sm-service` 直接用
`sm_plugins::provider_calls`，**不必**再靠组合根注入一批窄 trait —— 后者会把
`ProviderOperationError`（7 个码 + `retryable`）复制成第二份，而调用方分支
恰恰依赖它。

ADR 里还钉了三条约束（防以后加回去）与**缺口清册**：导入组（Scan/Stage/
Finalize/Abort/GetIdentity/DeleteImportFile）、指纹组（ComputeFileHash）、
转存组（8 个含流式）、下载组（5 个）、浏览组（Browse），各自解锁哪些文件。
⚠️ `ImportFile` / `ImportPlacement` / `StagedMedia` / `MediaHandle` /
`LibraryHandle` / `SourceDisposition` 定义在 **`common.proto`**（storage.proto
里只有引用）—— 实施时先读那边。

##### 下一步的两条路（建议选 ①）

1. **先把 provider seam 的 Rust 侧签名钉下来**：读 `proto/provider.proto` 把
   上面那批 rpc 的 Rust 窄接口（注入 trait）定义好，组合根实现。它一次性
   解锁约 20 处，且这些签名是**契约**（照 proto 抄），不是猜的。
2. 先做 `import_service` 的**宿主写入那一半**（`_create_media`：影片先建、
   媒体后建、`storage_ref` 落 `media.storage_ref`）—— 但 scan/stage/finalize
   仍要等 ①。

已核实的上游语义（供 ① 之后直接写）：`import_from_source` 的 7 步前置校验与
错误码、不安全文件名判据、`source_disposition` 三取值、`supports_in_place_import`
能力名、失败条目字段表、`metadata_import_batch` 用线程池（上限
`import_metadata_max_workers`）、`retry_failed_file` 按 `source_kind` 分支
（`plugin` → `import_plugin_movie`，否则 `import_movie_if_missing`，
两者 `force_subscribed=True`；**没有**骨架注释里那个 `staged` 分支）。

#### `import_service` 三处落地（transfers 26 → 23）

**没有新增 `sm-service → sm-plugins` 依赖**：骨架里那两个 trait
（`StorageProvider` / `CatalogImport`）本来就是注入缝，直接用它们 —— 组合根
（sm-server）持有插件连接并实现 trait。这比让业务层直接拿 tonic client 干净，
也保住了「业务层只依赖契约」。

- **`import_from_source`**：7 步校验按上游顺序（源形状 → disposition → 库 →
  kind → 合集 → 原地能力；**先挡 422 再挡 404**，反过来会让一个错别字变成
  「找不到库」）→ scan → 逐条：文件名安全检查 → 认番号 → stage（带幂等键）
  → 宿主写入（**影片先建、媒体后建**，`storage_ref` 从 **stage** 拿而不是
  finalize —— 宿主写入发生在 finalize 之前）→ finalize → `delete_after_commit`
  删源。★ stage 之后到 finalize 之间**任何**失败都 abort。单条失败进
  `failed`，不整体回滚（一部影片元数据缺失不该让另外 199 部白导）。
- **`metadata_import_batch`**：逐条失败不中断。⚠️ 上游是线程池并发，本仓顺序
  执行（并发是优化不是语义；真要并发应在组合根注入执行器）。
- **`retry_failed_file`**：★ **又订正一处骨架注释** —— 分支依据是
  `source_kind`（`plugin` → 按候选 id；否则按番号重取），**没有**
  「`staged=true` 跳过 scan」那一支。两支都 `force_subscribed=True`。

⚠️ **未闭环（如实登记）**：`retry_failed_file` 目前只**重建影片元数据**；
媒体记录与 provider 定稿那两步要等失败项携带暂存句柄（`ImportFailure` 还没有
那一列）。另外 `in_place` 因 proto 缺口只能按「能力不支持」处理。

#### `download_client` 三处落地（117 → 111）

**读上游原文**（`../sakuramediabe/src/service/transfers/downloads/client_config_service.py`）
而不是照骨架注释写 —— 因此又订正了三处骨架期的错误：

1. ★ **响应体必须剥掉 secret 字段**（上游 `_resource` `:72-91`）。原 `list_clients`
   把 `provider_config` **原样**发出去 = 泄漏凭据。判据来自插件声明的
   `input == "secret"`；拿不到字段表（插件没装 / 无下载能力）时上游发 `{}`。
2. 骨架期那条「`kind` 不可改」的规则上游**不存在**（上游没有 `kind`）；真正被禁
   的是「**有任务时不能换库**」，码 `409 download_client_library_change_forbidden`。
3. 更新只在动了 `library_id` / `provider_config` 时才重跑 `_prepare` —— 改个名字
   不该触发一次 provider 往返。

没有新增依赖：`sm-service` 本来就能看到 `ProviderOperationError`；但真正需要的是
「**谁**提供 `config_fields` / `prepare_client` / `test_client`」，所以拆两个_trait
作注入缝（`DownloadClientCapability` + `DownloadCapabilityRegistry`），由组合根实现。
保留 `new(db)` 不动（纯库两个方法够用），新增 `new_with_downloads` —— **未注入时三个
写方法返回 503**，而不是假装配置合法。

顺带：`test_client` 的失败（含 `prepare_client` 失败）都转成失败诊断、**HTTP 200**；
已有客户端只能用**它自己所属**的库测试（`download_client_test_library_mismatch`）。

⚠️ **未接线**：`routes/download_clients.rs` 那 3 个 handler 还是 `todo!()` ——
它们要 app state 里的插件注册表，下一步和 `download_common::download_provider` 一起做。

#### 下载组闭环：`download_clients` 三路由 + `download_provider`（111 → 107）

接上服务端三处与路由三处，**顺手删掉了路由层四份自造类型**：

- `download_common::download_provider` 落地：★ 参数改成 **`provider_key`**（上游
  `_bundle(library)`），而不是骨架期的 `&DownloadClientRow` —— 真正决定能力的是
  客户端**所属媒体库的 `provider_key`**，传 row 就得再查一次库，而这个助手存在
  的意义正是「调用方不必关心查库」。顺带删掉占位的 `PluginDownloadProvider`
  （unit struct 留着只会让人以为句柄已经有了）。
- `AppState` 加 `downloads: Option<Arc<dyn DownloadCapabilityRegistry>>` +
  `with_download_capabilities`，**照 `storage` 既有的那套写法**（活的 `Option`、
  缺省 = 没装插件）。★ 并加了 `download_client_service()` 便捷构造器 —— 路由
  **不要**自己 `new`：漏了 `with_downloads` 的表现是三个写方法**一律 503**，
  而不是「缺哪个插件报哪个错」，最难分清。
- 路由三个 handler 接上。★ 顺带删掉四类**本地副本**：自造的诊断结果
  （`reachable`/`latency_ms`/`version`/`error`）与两份请求体（`{name, kind,
  config}`），改用服务层那一份 —— wire 形状留两份定义，改一处漏一处时前端发出
  去的键后端不认。

⚠️ 组合根（sm-server）**还没注入**注册表，所以现在三条写路径仍是 503 —— 下一段
就是把 sm-plugins 的能力接进 `with_download_capabilities`。

#### `media_library` 六处落地（107 → 101）+ 又一次「按上游原文纠错」

这轮又是**读上游原文**（`media_library_service.py:182-328` +
`schema/playback/media_libraries.py`）而不是照骨架注释写。骨架期错四处：

1. ★ **`provider_config` 是可改的**，而且是 `update_library` 的**主干分支**
   （带 `previous` 重跑 `prepare_library`）。骨架文档写的「`provider_config`
   不可改」是反的 —— 真正不可改的只有 **`provider_key`**（更新请求里根本没有它）。
2. `enabled` **库里没有这一列**（响应体与创建请求都多写了它）；`space_usage`
   也不在响应体里 —— 容量走独立端点。
3. 更新请求的字段是 `{name?, provider_config?}`，不是 `{name?, enabled?}`。
4. 空间缓存**按 `{provider_key}:{account_key or library:{id}}`**，不是按
   `library_id` —— 同一 115 账号可以挂多个库，按账号去重才少一次远程查询。

实现层面与下载客户端同构（`provider_config` 同样要剥 secret、`prepare_*` 同样在
落库之前），但**刻意没有**把两个 seam 合成一个类型：它们分属「下载能力」与
「库能力」，合并会让边界变模糊。新增 trait `MediaLibraryCapability` /
`MediaLibraryRegistry`（后者还带 `supports_in_place_import` 与容量的**可选**
读取 —— 不支持 / 失败的库不出现在结果里，不让状态页整体失败）。
`account_key` 由 `prepare_library` 派生并落库（`set_account_key`）。

⚠️ 未接线：`routes/media_libraries.rs` 那 5 个 handler 仍是 `todo!()`，且
AppState 还没塞 `MediaLibraryRegistry` —— 下一步接组合根。

#### ⛔ 查证：组合根注入被插件**字段表**卡住（不是「顺手接一下」）

上一段写的「下一步接组合根」**不成立**，原因是查出来的一个真实缺口：

- `prepare_library` / `prepare_client` / `test_client` / `get_space_usage` 这**四个
  rpc 动词在 proto 里全都有**（`sm-plugin-api/src/provider.rs:339/377/387/242`），
  照 `provider_gateway.rs` 的样子包一层就能调。
- 但白名单校验要的 **`config_fields` 没有来源**：全仓
  `sm-plugin-api/**/*.rs` 搜 `ConfigField` / `config_field` / `read_only`
  **零命中**；`ProviderRegistration`（`sm-plugins/src/registry.rs:34`）只带
  `provider_key` / `display_name` / `plugin_id` / `capabilities` /
  `data_plane_endpoint` / `plugin_endpoint`。
- 上游这些字段来自**进程内加载的 Python bundle**（bundle 是对象、自带
  `library_config_fields`），我们的插件是**独立进程** —— 宿主不去问，就永远没有
  那份表。同一族的既有缺口：`playback_deliveries`
  / `merged_playback_format` 也未存（见本文早前的 `videos` 一节）。

**后果不是「功能少一点」，而是会把功能做坏**：字段表为空时 `_validate_config`
把用户提交的每个字段都判成「未知字段」，结果是**建库/建下载器一律 422**；
`_resource` 又会因为拿不到 secret 名单而把 `provider_config` 全部返 `{}`。
所以这里**不做半截实现**，宁可保持三条写路径返回 503。

要往下走，得先在 proto 的注册/描述里补一份 bundle 描述符（config 字段 + 交付方式
+ `supports_in_place_import`），那是插件契约的改动，不属于「顺手接线」。

#### ⚠️ `media.rs` 列表五处：**先纠排序白名单**，别照骨架写

上游 `MediaService.MEDIA_LIST_SORT_FIELD_MAP`（`media_service.py:94-97`）只有
**两个**字段：

```python
MEDIA_LIST_SORT_FIELD_MAP = {"file_size_bytes": Media.file_size_bytes, "heat": Movie.heat}
```

而骨架的 `MEDIA_LIST_SORT_FIELD_MAP`（`playback/media.rs`）是 **4 个**
（`created_at` / `updated_at` / `file_name` / `heat`）—— 多出来的三个是**自造**的。
★ `heat` 在上游是 **`Movie.heat`**，不是 media 的列 → 排序必须 **LEFT JOIN movie**，
且 `NULLS LAST`（非 JAV 视频没有 movie，heat 恒空；不受排序方向影响都要垫底）。

已落地的一半：仓储层 `MediaRepository::list_filtered` / `count_filtered` /
`multi_version_movie_numbers` / `count_multi_version_movies` + `MediaListFilter`
（WHERE 唯一拼接处，全部走 `push_bind`，无字符串插值）。服务层两个方法**还没接**。

#### `media` 列表三处落地（101 → 96，playback 10 → 5）

`list_duplicate_media_groups` / `list_media_points` / `list_invalid_media` 三处齐了。
仓储侧扩了「筛选面」：`MediaListFilter` 新增 `require_valid` / `search` /
`file_hashes`，`MediaPointRepository` 新增时刻列表的 `list_filtered` /
`count_filtered`（条件拼接各只有一处，两个查询共用）。

★ **两处骨架期自造，已按上游纠回**：

1. `list_invalid_media` 的 search 是**四个字段任一命中**（影片番号 / 影片标题 /
   视频标题 / 文件名，`:766-773`），骨架注释只写了「文件名与番号」。差异化的是
   非 JAV 媒体 —— 它们的番号恒空，只按番号搜会把整类失效视频筛没。
2. `list_media_points` 的排序**只有 `created_at:desc` / `created_at:asc`** 两种
   （`MEDIA_POINT_SORT_FIELDS`，`:89-92`），别的值 422。骨架注释只说「按
   created_at DESC」像是随便排。

⚠️ **`DuplicateKeyKind::Degraded` 暂未启用**（只输出 `key_kind: "hash"`）：上游是
**只按 `file_hash` 分组**且只算非空哈希的（`:379-388`）。「按文件名+大小猜重复」
会直接误导用户删文件，所以不自己发明分组与分页语义 —— 要用得先定清它跟哈希组
怎么共同分页（两者顺序、`total` 怎么算）。类型与单测都留着，只差实现。

⚠️ 未做：重复组里 video 项的 `collections`（上游 `:415-443` 带合集引用）需要
合集侧查询面，还没有。

#### `movie_metadata_search`：候选 id 编解码 + 缓存清理（96 → 94）

★ **顺手消掉同 crate 的两份同名类型**：本文件自己声明过
`MetadataCandidate { candidate_id, title, date, preview_url, confidence, source }`，
而 `transfers::import_task` 里那份才是上游形状（`media_import.py:76-86`，10 个字段，
**没有 `confidence` / `date` / `preview_url`**，有 `source_name` / `duration_minutes`
/ `cover_url`）。两份都活着，客户端拿到哪份取决于走哪条路。现在
`movie_metadata_search` 只做转出口。**再不要**往这里加第二份 wire 类型。

落了两处：
- `resolve_candidate_reference(candidate_id, plugin_enabled)` —— 判据**只有两条**：
  段数 = 3、前缀是 `javdb` / `plugin`。★ 不校验番号形状：上游不校验（真伪由
  fetch 时 `_ensure_candidate_number` 兜），自造格式规则会把上游能接受的重试挡掉。
  `plugin_enabled` 做成**参数**：插件可能已卸载而候选 id 还在客户端手里（搜索
  结果缓存数小时），不查就会放行一个必然失败的重试。做成参数也让这条规则能脱离
  插件栈单测。另落 `javdb_candidate_id` / `plugin_candidate_id` 两个编码器 ——
  与解码是一对，只写一边就会出现「搜索给的 id 重试解不出来」。
- `cleanup_search_assets(root)` —— **接 root 参数**（组合根知道图片根目录），
  因此可脱离配置单测。★ 两处 `symlink_metadata` 判断是**安全**要求：目录名虽是
  uuid，但条目可能被换成指向别处的链接，跟进去 `remove_dir_all` 会删掉别处。
  边界：恰好 24h **算过期**（上游 `now - mtime > MAX_AGE`）。

⚠️ 仍未落：`search_by_number` / `fetch_candidate` —— 要元数据 provider seam
（JavDB 取详情 + 枚举已启用插件来源）与图片落盘（根目录 + 下载器 + 解码校验）。
`sm_core::signing::build_signed_image_url` 与 `svc-image` 都在，缺的是
「图片根目录怎么解析」与 provider 侧取数。

### 下一批：`transfers` 与 `catalog` 两块

| 候选 | 备注 |
|---|---|
| `catalog/movie_subscription_search_state.rs`(7) | **纯 DB，无阻塞** —— 建议先做 |
| `catalog/movie_metadata_search.rs`(4) | 纯 DB |
| `transfers/download_client.rs`(9) + `download_common.rs`(8) | ⚠️ **被下载器插件 ABI 挡**（与上面同一个 registry）|
| `catalog/catalog_import.rs`(7) | 依赖 `metadata_source`（插件 ABI）+ `image_cleanup`（已就绪）|
| `transfers/import_task.rs` 剩下 3 个 | 见上（`execute` 要 `import_service` + `catalog_import`；`search` / `retry` 要插件 ABI —— `retry` 第一步就是 `resolve_candidate_reference`）|

### ★ 插件 ABI 评估：协议面其实**已经齐了**，缺的是宿主侧调用面

先说结论，这条与之前的判断不同：**proto 与插件侧基本都在，宿主侧没接出去**。

| 层 | 现状 |
|---|---|
| `proto/{common,plugin,host,storage}.proto`（共 1229 行）| ✅ **完整**。`service StorageProvider`（browse / scan_import_source / read_import_file / delete_import_file / stage / finalize / abort / **delete_media** / compute_file_hash / **generate_thumbnails** / create_clip / probe_* / plan_playback）、`service DownloadProvider`、以及 `MediaProviderBundle`（`playback_deliveries` / `merged_playback_format` / `data_plane_endpoint`）都有 |
| 插件侧 | ✅ `crates/plugin-ref-local/src/provider.rs` 实现了 browse / scan_import_source / plan_playback / generate_thumbnails，其余返回 `Status::unimplemented` |
| 宿主侧 loader / 注册表 / 进程管理 | ✅ `sm-plugins::{loader, registry, runner, supervisor, extensions, jobs}` |
| **宿主侧调用面** | ❌ **没有**。全仓搜 `StorageProviderClient` 只命中 `plugin-ref-local` 自己的 lib 与测试；`extension_calls::fetch_ranking` 也**只有测试在调** |

所以之前的判断「`MEDIA_PROVIDER_REGISTRY` 只存在于注释里」**基本成立但性质不同**：
不是说协议没设计，而是**宿主没把声明变成可调用的东西**。

#### 本轮已落地（两处，都是上面两种改法都要的管道）

1. `ProviderRegistration` 加了 `plugin_endpoint`，`collect_providers(response, endpoint)`。
   之前查表只能查到「声明了 SCAN_MEDIA_REFS」，**打不出去** —— 因为
   `StorageProvider` 与 `PluginControl` 是同一个进程的同一个通道，而注册响应里
   **没有**「我监听的地址」（插件是宿主拉起来的，地址由宿主分配）。
2. 新模块 `sm-plugins/src/provider_calls.rs`：错误类型 + `Status → 上游 7 码` 的映射
   + `connect_storage` + `delete_media`。

#### ✅ 已闭合：P1-1「`GenerateThumbnails` 的返回值丢了」

`docs/parallel/grpc-plugin-report.md` §4.3 已经把四个 P1 缺口列出来了，**并给了
建议**。P1-1 的建议明确（`returns (stream GenerateThumbnailsResponse)` +
`oneof {progress, done}`），而且当前只有 `plugin-ref-local` 一个参考插件 ——
所以这一处**不需要再等决策，直接按建议改了**。

| 层 | 改动 |
|---|---|
| `proto/storage.proto` | rpc 返回改成 `stream GenerateThumbnailsResponse`；消息体改成 `oneof payload { ProgressEvent progress = 1; ThumbnailGeneration done = 2; }`，并在注释里写明「**必须**以 `done` 收尾」 |
| `sm-plugin-api::provider::StorageProviderExt` | 关联类型 `GenerateThumbnailsStream` 与适配层签名同步（P1-4 提到的那层适配**已经存在**）|
| `plugin-ref-local` | 每个产物记下 `offset_seconds` + `relative_path`，流尾发 `done` |
| `sm-plugins::provider_calls` | `generate_thumbnails` 收流：进度走回调，**收不到 `done` 视为 provider 违约**（`Unspecified`）|

宿主侧的关键判据：产物清单是**唯一**的落库依据 —— 没有它就无法 `persist`，
所以「流正常结束但没有 `done`」按失败处理，而不是「有多少算多少」。

> `plugin-ref-local` 的两条测试也跟着改了，并补了断言：偏移严格递增、落在
> `(0, duration)` 内（不取首尾帧）、且 `relative_path` 真的能在 workspace 里找到。

#### ⚠️ 仍待定：P1-3「错误码过不了线」（需要你定）

上游的失败是一条结构化记录，`code` 有 7 个取值，**宿主的控制流是按它分支的**：

| 调用点 | 分支 |
|---|---|
| `delete_media` | `source_not_found` → **继续**清理本地元数据（远端早已不在）|
| 缩略图生成 | `unavailable` 且 `retryable` → 走**延迟轨**（不是失败轨）|
| 播放 | `authentication_failed` → 401；`unavailable` → 503 |

但 `proto/common.proto` 里那个 `ProviderError` / `ProviderErrorCode`
**没有任何 rpc 用它做错误通道** —— 插件自己的注释就写着
（`plugin-ref-local/src/provider.rs:249`）：

> 失败只有一个布尔位 `unavailable`，区分不了「文件不存在」「无权限」「已被黑名单」，
> 也带不了 ProviderErrorCode / retryable —— common.proto 里定义了 ProviderError，
> 但没有任何 rpc 用它来做错误通道。

于是宿主只拿得到 `tonic::Status`（一个 gRPC 码 + 一句人话），映射**必然有损**。
最危险的一处：

> `NotFound` 同时被「远端文件不在」与「媒体库不存在」用。映射成
> `source_not_found` 后，`delete_media` 会当「远端早已删掉」而继续 ——
> 于是「库没配好」这个真实原因被吞掉，而**文件其实还在远端**。

另外 `retryable` **完全拿不到**，只能按码猜（本仓偏保守：只有 `unavailable` 算可重试；
猜反的代价不对称 —— 把确定性失败判成可重试，会让每轮都白烧一次 provider 调用）。

**两条闭合路径都不小，需要你定**：

1. 让 `ProviderError` 真的上错误通道 —— 用 `google.rpc.Status` 的 `details`
   塞 protobuf 编码的 `ProviderError`（宿主侧要引 `tonic-types`），**所有插件都要跟着改**；
2. 每个响应消息加 `optional ProviderError error = n;` —— 更直白，但要改全部
   28 个 rpc 的响应。

在那之前，映射**集中在 `classify_status` 一个函数里**并标注了每个分支的依据，
将来迁移只改那一处。

#### 媒体/时刻列表 8 条路由接线（94 → 86）

把上一段已经写好的服务方法接上路由，**顺带纠了四处骨架期的错**：

1. ★ **`/media` 的 `sort` 形状是 `field:direction`，不是 `-field`**。上游
   `resolve_sort_expression`（`service_helpers.py`）：`strip().lower()` → 按 `:`
   切字段与方向 → 方向只认 `asc`/`desc` → 字段要在白名单里，任一不满足都是
   `422 invalid_media_filter`（`details.sort`）。骨架的 `-heat` 是**自造**约定 ——
   客户端按上游发 `heat:desc` 会被判成「未知字段」。
2. 同一处：路由注释写「`sort` 非法值**降级**」—— 与 1 是同一个错的两种说法。
3. **时刻列表的 `sort` 收裸 `asc`/`desc`** —— 上游 `MEDIA_POINT_SORT_FIELDS`
   只有 `created_at:desc` / `created_at:asc` 两个取值，裸方向词应 422。
   收下来等于把非法输入当合法，客户端会依赖它。
4. `/media-points` 的查询**少了三个上游参数**：`kind`（默认 `jav`）、`keyword`、
   `exclude_collection_id`（`ge=1`）。少参数不是「功能少一点」—— 按上游发的
   `?kind=video` 被静默忽略，「筛选视频」的结果里混着 JAV。另外 `GET /media-points`
   的 `MomentCollectionSummary` 被自造地加了 `item_count`（上游只有 `id`/`name`）。

**两个「静默错误」也一并堵上**：`/media` 的 `kind` 与 `thumbnail_generation_state`
是上游的**枚举**，非法取值该 422；而服务层对 `kind` 是 `_ => {}`（认不出 =
不过滤）—— `?kind=bogus` 会返回全部媒体。校验放在路由层（枚举是 query 参数的事）。

**接线**（`routes/media.rs` 5 条 + `routes/media_points.rs` 3 条）：

| 端点 | 服务方法 |
|---|---|
| `GET /media` | `MediaService::list_media` |
| `GET /media/invalid` | `list_invalid_media` |
| `GET /media/duplicates` | `list_duplicate_media_groups`（`kind` 必填 + 枚举） |
| `GET /media/multi-version-movies` | `list_multi_version_movies` |
| `POST /media/thumbnail-generation/reset` | `MediaThumbnailTaskService::reset_terminal_media` |
| `GET /media-points` | `list_media_points`（★ 结果要**签名**，见下） |
| `GET /media-points/{id}/collections` | **新增** `MomentCollectionService::list_point_collections` |
| `DELETE /media-points/{id}` | `MediaService::delete_point_by_id` |

- ★ `GET /media-points` 必须把服务层给的**未签名** `image_origin` 换成签名 URL，
  否则客户端拿裸路径取图会 403。为此在 `dto.rs` 新增 `MediaPointListItemResource`
  （上游同名），并**删掉**路由层那份自造的 `MediaPointListItem`
  （`id`/`kind`/`title`，字段与上游毫无交集且没有图片）。
- 新增 `sm-db` 查询 `MomentCollectionItemRepository::list_collections_for_point`
  （三表 JOIN，顺序 `c.updated_at DESC, c.id DESC`，与合集列表一致）。
  ★ 「点不存在」的详情键是 **`point_id`**（上游 `_require_point` 显式传的），
  而 `DELETE` 那条走 `MediaService` 的默认键 `media_point_id` —— **两个键确实不同**。
- 这 8 条查询端点从 `axum::extract::Query` 换成 `crate::extract::Query`：原生
  rejection 是 **400 + 纯文本**，项目契约是 422 + 错误信封。
- 顺带删掉 `routes/media.rs` 里**本地重复**的 `secret()`/`now_seconds()`
  （`crate::signing` 早就是那份共享实现），以及 `sm-service` 里**第二份**
  `thumbnail_state`（i32 版，与库里实际存的文本列对不上；四处状态字面量在
  sm-db 已有一份）。改由服务层的 `MediaListQuery` 直接复用，不再有路由副本。

⚠️ 仍未做：`routes/movies.rs` 的刷新、`media_libraries` 5 条（等插件字段表）、
`download_tasks` 2 条（等插件注册表）。

### 下一批（承接插件 ABI）

### `generate_pending_thumbnails` 已落地（剩余 todo 里最大的一块）

三段都写了：取候选 → 逐条加**媒体级锁** → `classify` 落状态机，产物经
`artifacts::persist` 落盘。

#### ★ 一个绕不开的分层约束，以及采用的解法

`sm-service` **不能**依赖 `sm-plugins`：依赖方向会变成
`sm-plugins → sm-scheduler → sm-service → sm-plugins`，**成环**。而生成缩略图
要调 provider。

所以走**依赖倒置**：`sm-service` 定义 `ThumbnailGenerator` trait（宿主侧类型，
**不含 proto 形状**），由**组合根**（`sm-server`，它同时看得见两边）注入实现。
与 `RankingSourceCatalog` 同一个取向。没有注入时直接返回「0 部 + `skipped_no_provider
= true`」—— **不假造产物**（假造会让客户端拿到打不开的图，而状态机以为成功了）。

> ⚠️ `RankingSourceCatalog` 只解决了**声明**那半边；`fetch_ranking` 的调用至今
> 也只有测试在调。缩略图这条现在是**第一个真正打通 provider 调用**的路径，
> `ranking` 那边可以按同一个 trait 注入模式抄。

#### 补的三个 sm-db 方法（此前都缺）

| 方法 | 为什么不能省 |
|---|---|
| `list_thumbnail_candidates(limit)` | 候选行要带 `library_id` + `provider_key`，只给 `media_id` 找不到该调哪个插件 |
| `record_thumbnail_terminal(id, code)` | `record_thumbnail_failure` **固定**写 `retry_wait`，终态是另一条路（清 `next_retry_at` + 记 `terminal_at`）|
| `record_thumbnail_deferred(id, code, next)` | ⭐ 它加的是 `thumbnail_deferred_count` **不是** `attempt_count`。复用 `record_thumbnail_failure` 会让「盘还没挂载」消耗**失败预算**：延迟 2 次之后，一次真正的失败就直接进终态 |

> 没做成 `set_state(state, …)` 一个通用方法：三种终态的语义各不相同
> （成功要**清零**计数、退避要**排下次时间**、终态要**记放弃时刻**），
> 合成一个会让调用方忘掉「终态要清 `next_retry_at`」这类细节。

#### 两条轨道在循环里怎么分流

```text
  provider 报 unavailable 且 retryable = true  -> 延迟轨（deferred）
  其余 provider 失败                            -> classify 按 attempt_count 判失败/终态
  产物全部校验不过                               -> 走失败轨（带上第一条错误码）
```

另有一类 `RoundFailure::Db`：**宿主自己的数据库出错**，不进任何一条轨道 ——
把它算成 provider 失败会让「数据库抖了一下」变成「这部片子的缩略图失败了」。

按「先管道、后接线」：

1. ✅ `provider_calls::generate_thumbnails`（收流 + 终态产物清单）已落地；
2. ✅ `generate_pending_thumbnails` 已落地；
3. ⏭ **组合根注入 `ThumbnailGenerator` 的实现**（`sm-server`：按 `provider_key`
   查 `ProviderRegistration.plugin_endpoint` → `connect_storage` →
   `provider_calls::generate_thumbnails`）；
4. 接线 `MediaService::delete_media`（`provider_calls::delete_media` 已就绪）；
4. `videos` 的 `play_url` —— ⚠️ 需要 `MediaProviderBundle.playback_deliveries[0]`，
   而 `ProviderRegistration` **目前没存**它（也没存 `merged_playback_format`），
   要先补；
5. `plan_playback`（P1-2 说 `PlaybackPlan` 缺 `LocalPathPlan`，本地/NFS 类
   provider 只能退化成 `file://` 伪直链 —— 那是个待定的 proto 改动）。

其余两个 P1（`docs/parallel/grpc-plugin-report.md` §4.3）：
**P1-3 错误码**（待你定，见上）、**P1-4 tonic trait 无默认实现**（`sm-plugin-api`
的适配层已经在，可视为已缓解，剩下的是"proto 每加一个 rpc 全部插件编译不过"
这个编译期耦合 —— 建议把 32 个 rpc 拆成几个更小的 service）。

### 其余候选

| 候选 | 备注 |
|---|---|
| `MediaService` 剩下 8 个 | `list_media`（七参数过滤 + 白名单排序 + `heat` 的 `NULLS LAST`）、`list_duplicate_media_groups`（哈希分组，缺哈希走**退化键**并标 `Degraded`）、`list_multi_version_movies`、`list_media_points`（全局列表，默认 `created_at` 降序）、`list_thumbnails`（等 `artifacts` 改形状）、`list_invalid_media`、`delete_media`（⚠️ **被插件 provider 挡**：第一步要 `storage.delete_media(media_handle)`，不做第一步就是留远端文件；`sync_video_member` 那一支可走 `VideoItemService::delete`） |
| ~~`download_tasks`~~ | ✅ 已收尾（`list_tasks` 落地）；`import_task` 的入队也已落地，两者各剩 `delete_task` / `execute` 一族被插件 ABI 与 `import_service` 挡 |
| `system/telemetry`(2) | 依赖最少 |
| worker handler 3/19 → 19/19 | 每个 handler 的 service 都已就绪 |
| `videos` 详情/创建/更新/删除 | ⚠️ **被插件 ABI 卡住** —— `media_items[].play_url` 要 `MEDIA_PROVIDER_REGISTRY` 拿 `playback_deliveries[0]`。**不要用空串冒充**：客户端会把空地址当成「不可播放」 |

**每次接一族都要做的固定动作**：先读上游 router → schema → 每一处
`ApiError`，再读前端同名 DTO 交叉验证，最后才对照骨架的 DTO。
**目前五族里每一族都查出过骨架写错的契约**（`moment-collections` 5 处、
`video-collections` 8 处、`videos` 3 处、`media` 点/进度 5 处；
`clip-collections` 是唯一一处没错的 —— 因为它的 DTO 在 `dto.rs` 里，
而**路由文件内联的骨架 DTO 全都不能信**）。

### 两条每次接线都要做的事（本轮踩出来的）

1. **先读上游 schema，再看骨架里的 DTO。** 本轮 moment-collections 的
   DTO 有 **5 处**是骨架期凭印象写的：`item_count` 应为 `point_count`、
   缺 `description`/`cover_image`/`updated_at`、点位条目多了个上游没有的
   `kind`、以及文档把「重复添加」写成了 409（上游是**幂等 204**）。
   **骨架里的注释也可能是错的** —— 本轮就有一处（`add_point` 的 409）。
2. **骨架说「不需要」的下层方法，先自己核一遍上游有没有对应端点。**
   `collection.rs` 里 clip 那份的文档写着「`MomentCollectionRepository`
   不需要 `list_ordered_by_recency`，时刻点合集没有列表端点」——
   上游有 `GET /moment-collections`，且排序与 clip 完全相同。

**别再做的**：不要去「顺手修」上游那两处刻意照抄的缺陷（见「五」）。

**`upstream/` 是目录联接（junction）**指向 `../sakuramediabe` 等真实仓库，
不占额外磁盘 —— 别把它当成副本删掉。

## 二、上游参照物（都已克隆在 `upstream/`，`.gitignore` 忽略）

| 目录 | 是什么 |
|---|---|
| `upstream/sakuramediabe` | **后端 Python**（重写对象）。契约的唯一权威来源 |
| `upstream/sakuramedia` | **前端 Flutter 客户端**（Dart）。想验契约对拍时用 |
| `upstream/sakuramedia_local_provider` | **真实插件**（本地存储 + qBittorrent）。插件契约样本 |
| `upstream/sakuramedia_115_provider` / `sakuramedia_javbus_metadata` | 另两个官方插件 |

对齐顺序：**先读上游 router → schema → 每一处 `ApiError(...)` 调用点**，再动手。不要凭印象推契约。

## 二之二、前端的 DTO 是**第二份独立证据** —— 别只读后端

`upstream/sakuramedia/lib/features/<域>/data/dto/*.dart` 是客户端视角的同一份
契约。它与后端 schema 是**两次独立书写**，所以能交叉验证：

- **字段名**逐个对照 —— 例如 `video_item_list_item_dto.dart` 读的 14 个键与
  后端 `VideoItemListItemResource` 的 14 个字段**逐字相同**（含
  `cover_width` / `cover_height` / `can_play` 这些容易漏的）；
- **哪些字段真的有人读**。骨架期给 `GET /videos` 编的 `thumbnail_url` /
  `description` / `metadata` 三个键，前端**一个都不读** —— 后端 schema 里
  也不存在。三方都指不回骨架的写法，那就是凭空发明的。
- **请求体的 null 语义**。`VideoCollectionUpdatePayload.toJson()` 是
  `if (name != null)` —— 客户端**从不发 null**，所以那种「上游会 500」
  的边界（见 `moment_collections.rs` 模块文档）实践中打不到。

`.dart` 里也带注释，且注释经常直接引用后端字段名（「后端 `VideoCollectionRef`」
「`include_play_url=true` 时由后端内联」），读它能省一次后端跳转。

## 三、继续做：按块走，一次做完一块

### 块 A：插件宿主（进行中，①② ③④⑥ 已完成）

已完成：注册校验、能力注册表、加载器（连接 + Register + 收声明）、错误映射、任务注册表、`RunJob` 的流式执行与事件收敛、插件任务接进 cron 触发。
待做（按序）：

1. ~~**`RunJob` 的流式调用**~~ **已完成**（`runner.rs` + `scheduling.rs`）：`stream JobEvent` 收敛成终态，超时即 `drop(stream)` 表达取消（proto 的「宿主直接断开流」）；cron 触发那半是 `JobDefinition.default_cron` / `manual_only` → `sm_scheduler::JobSpec`，与内建任务共用同一套到点判定与 coalesce。
   组合根**已接上**（原先这里写的是「仍刻意不依赖 `sm-plugins`」，与下面第 3 步的「看门狗与组合根也已接上」自相矛盾 —— `sm-server/Cargo.toml` 里 `sm-plugins = { workspace = true }`，`sm-server/src/plugins.rs` 存在，插件任务已并进调度表）。
   剩一处刻意的局限：插件重启后**新增**的 cron 任务要等下次进程启动才生效（`Scheduler` 的任务清单构造时定死），已在表里的不受影响。
2. **三个扩展点的调用面**：`media.provider`（已有注册表）/ `catalog.metadata_source` / `discovery.ranking_source`。
   **已完成**（`extensions.rs` + `extension_calls.rs`）：两个扩展点的载荷校验、`source_key` / `board_key` 形状、缺 capability 不收、排行榜 `source_key` 冲突时该插件的榜单全部不收（对齐 `apply_plugin_ranking_sources`）；调用面真发 rpc，并把「未收录」（`found=false`）与「调用失败」分成两类结果。
   **交付校验已补**（`movie_delivery.rs`）：图片必须落在 `FetchMovieRequest.delivery_dir` 内、是普通文件、再深一层且同一请求目录；`release_date` 严格 `YYYY-MM-DD`、`duration > 0`；用完 `cleanup_delivery`。判据是 proto 给的，不依赖 `plugins.root_dir`。
   两处**还没做**：一是「冲突时连该插件的任务一起不注册」—— 要等加载器把任务表与扩展点表串起来；二是**番号一致性**不在这一层判（`normalize_movie_number` 住在 `sm-service`，那条依赖边将来会成环），由导入方比；三是**入库路径**还没有（catalog 域缺插件导入服务，拿到校验过的结果也没处写）。
3. **进程生命周期**：拉起进程、握端口、重启看门狗（`loader.rs` 刻意没做）。
   **已完成大半**（`supervisor.rs` + 参考插件可执行文件）：宿主分配端口并经 `SAKURAMEDIA_PLUGIN_GRPC_ADDR` / `SAKURAMEDIA_PLUGIN_ID` 注入 → 拉起 → 用 `Register` 探活 → `wait()` 发现崩溃 → `restart_backoff` 给退避。协议是自定的（上游是进程内 import，proto 无此约定），依据见 `docs/adr/2026-10-05-plugin-lifecycle.md`。
   **看门狗与组合根也已接上**（`sm-server/src/plugins.rs`）：`sm-server` 现在依赖 `sm-plugins`，按 `plugins.enabled` 逐个拉起、收三张注册表、把插件任务并进调度表（`cron_info` 里看得到），并起一个看门狗轮询崩溃 → 退避重启 → 重建注册表。
   **一处刻意的局限**：重启后不重挂调度表（`Scheduler` 的任务清单构造时定死），所以插件重启后**新增**的 cron 任务要等下次进程启动才生效；已在表里的不受影响。
4. **数据面**（`data_plane_endpoint`）：**经查证上游 Python 与 proto 均无协议定义**，是预留设计位 —— 协议定了再做，不要凭空发明。

### 块 B：`system` 域 —— `jobs` 与 `activity` 已落地

原写「剩 11 个文件」，实测已经不成立：

- **`jobs`**：`GET /system/jobs`（列表）与 `POST /system/jobs/{task_key}/run`（触发）两条都在 `routes/jobs.rs`。列表的 `last_task_run` 用一次子查询（`MAX(id) GROUP BY task_key`）拿全部，不是 N+1。
- **`activity`**：整包 6 个端点在 `routes/activity.rs` —— bootstrap / notifications 列表 / 批量已读 / 全部已读 / task-runs / task-runs-active。DB → service → 路由三层都在。

剩下的 `telemetry`（约 188 行）可做；`plugins` 那部分要等插件宿主，`image_search_reset` 与 `metadata_provider_probe` 要等 Qdrant 与 metadata source。

### 块 C：插件 ABI 之后才解锁的（约 40 条端点）

transfers 编排、`/files/*` 与 `/media/{id}/play/{path}` 签名路由、multipart 上传、`system/plugins`、`{n}/reviews`、JavDB 导入。

### 卡死的（不用试）

- ~~`GET /movies/{n}/subtitles`~~ —— **两个端点都已落地**（读侧 `7698b78`、
  写侧 `0e10757`）。原判断「要读媒体文件系统（provider 族）」是**错的**：读字幕
  只读宿主自己的字幕目录，provider 参与的是「把字幕搬过来」那一步（写侧
  `subtitle_asset.rs`）。
  - 不变量一（**骨架期写错、已纠正**）：10 MiB 上限是
    `os.fstat` 判一次 + 限读 `MAX + 1` **再判一次**
    （上游 `movie_subtitle_service.py:69-83`）。不是「先 stat 再读」那么一次 ——
    文件可能在 stat 与 read 之间被写大。
  - 不变量二：路径逃逸校验要在**解析之后**做（只查字符串前缀会被软链绕过），
    且「文件不存在」**不是**路径非法 —— 那是 409 `subtitle_unavailable`
    （读内容）或 404 `file_not_found`（下载）。
  - 题外的坑：`os.stat` 那条注释是错的（上游用 `os.fstat`），骨架期的
    `ensure_subtitle_path` 也因此把 403 与 409 混成一个 —— 两处都已在
    `media_paths` / `movie_subtitle` 里纠正。
- `GET /movies/{n}` 详情 —— 要 playback 的进度/打点 + rankings

（`discovery` 已不再是卡死项：Qdrant 稠密/稀疏两侧都已落地，`sm-service` 16 个上游服务
已全部铺完，路由与 worker handler 已接上 3 个。剩下的是 `generate_recommendations`
与 `list_items` 两个依赖 `repo/discovery.rs` 里三张新表的 IO 部分。）

## 四、纪律（踩过坑才定的）

1. **直接推 `main`**。本地分支就是 `main`，提交后直接 `git push cnb main`，
   不开主题分支、不走 PR。

   > 这条**替换**掉了原先的「一个主题一条分支一个 PR」。原规则的理由是
   > 「PR 开着时往同一分支继续推新提交，对方中途合并会让后面的提交**静默
   > 掉队**（实测掉过 4 笔）」—— 那条风险只在**有 PR 评审**时成立：合并
   > 动作由别人触发，你的提交会落在一个已经移动过的分支上。没有 PR 就没有
   > 「别人中途合并」这个环节，风险不存在。
   >
   > 保留一句提醒：**推之前先 `git fetch` 看 `cnb/main` 有没有动过。** 多个
   > 执行体（CNB 的 auto 分支 agent、你自己）可能同时在写主线。
2. **先读跨文件依赖再开工**。三次半路撞墙（`subtitles`、`subscriptions`、proto 能力）都是因为只读了当前文件。尤其是 proto：`plugin.proto` **早就有 `enum Capability` 与 `PluginControl.Register`**，不要按「需要拆 service」的假定去改。
3. **不要发明协议**。上游没实现的（数据面）、proto 没定义的，先查证再动手。
4. **SQL 用 `QueryBuilder`**，不要拼字符串（占位符编号会静默错位）。
5. **置空只能用 SQL 字面量 `NULL`**：`UpdateSet` 的 `ValueInner::Null` 在绑定层是 text 类型，对 timestamp 列直接报错。
6. **受保护字段**（`sm_db::catalog::PROTECTED_MOVIE_FIELDS`，**6 个**：`title` / `summary` / `maker_name` / `director_name` / `is_collection` / `is_blacklisted`）必须经 `sm_db::repo::MovieOwnershipGateway`，否则自动规则会覆盖人工标记。演员侧同构（`PROTECTED_ACTOR_FIELDS` 9 个，`ActorOwnershipGateway`）。
   > ⚠️ 这里原先写的是「`is_collection` / `is_blacklisted` 两个」—— 那是 `sm-service` 里一份**杜撰的**白名单常量，与上游不符（少 4 个字段，于是插件补录会把 `title` 静默拒掉）。那份骨架连同演员的同款已删除，**网关只此一份**。
7. **重复常量是缺陷**：`ABI_MAJOR` 之类的只留一份（现在复用 `sm_plugin_api::ABI_MAJOR`）。
8. ★ **`todo!` / `panic!` 的消息里不能出现 `{...}`**。第一参数是格式串，
   `todo!("解出 {source, external_id}")` 会报「invalid format string」——
   `{a, b}` 不是合法占位符。想写字面量花括号就用中文描述或 `{{}}`。
9. ★ **注入白名单不能用 `debug_assert!`**。它在 release 下被编译掉，等于没有。
   `repo/recommendation.rs::document_frequencies` 踩过：白名单一旦消失，
   拼进 SQL 的表名/列名就成了注入面。标识符无法绑参数，所以白名单必须是
   运行时 `Err`，再按 sqlx 0.9 的要求包 `AssertSqlSafe`。
10. ★ **数「缺哪个文件」之前先看本仓是否已用另一种结构实现了它**。
    同一个上游文件可能被宏合并（`collections/ordered.rs` 用宏同时生成了
    moment 与 clip 两个收藏夹服务）或刻意不落地（`videos/mod.rs` 明确记载
    `video_cover_service` 属 svc-image/svc-probe 职责）。照着上游文件名建文件
    会造出重复实现，且两套规则会互相矛盾。
11. ★ **验证命令必须带 `--all-targets`**。`cargo check -p sm-service` 全绿时，
    `#[cfg(test)]` 里的 `mod tests` **一次都没编译过**。实测那里还藏着 6 个
    错误（`assert_eq!` 缺第二个参数、`.await` 缺失、借用冲突）。
12. ★ **`Option<T>` 里 `T` 带堆分配时不能 derive `Copy`，也不能 derive `Eq`。**
    - `Copy`：`RecomputeStats`（含 `Option<String>`）、`InterestSignals`
      （含 `Vec` / `HashSet`）、`CatalogImportResult`（含 `Vec<String>`）
      三处都踩了。**编译期拦得住**，不算危险。
    - `Eq`：`TaskTelemetry` 的 `success_rate: Option<f64>` —— `f64` 没有
      `Eq`（NaN != NaN）。同样编译期拦得住。
13. ★ **跨 await 的闭包要 `+ Send`，且通常要 `'static`**。
    `ProgressSink` 原本是 `Box<dyn FnMut(...) -> BoxFuture<'a, _> + 'a>`：
    只给内层 `BoxFuture` 加 `Send` **不够** —— 闭包对象本身进不了
    `TaskHandler`（要求 `Send`）。再进一步，handler future 隐含 `'static`，
    所以闭包不能借用外层的 reporter（要 `Clone` 一份进去），
    `text: &str` / `patch: Option<&Value>` 也要先拷成自有值再 `async move`。
    **这类错误表现为一串「lifetime may not live long enough」，看着唬人，
    成因只有一个：future 要求 `'static`。**
14. **别照抄第三方 API 的「看起来该有的形状」**。`qdrant_client::config::
    QdrantConfig::from_url` **返回 `QdrantConfig`，不是 `Result`**，且收 `&str`
    不是 `String`；解析失败发生在 `build()` 那一跳。骨架期写成
    `from_url(base.to_owned()).map_err(...)` 会产生三个错误。
    判断依据是本仓已有的调用点（`dense.rs` 的 `DenseStore::connect`）。
15. **两个不同集合的 store 进不了同一个数组**。
    `ThumbnailVectorStore` 与 `PlotImageVectorStore` 类型不同，
    `[&self.thumbnails, &self.plot_images]` 编不过 —— 写两遍，不要为了
    「统一」把它们塞进 `dyn`。同理，`Arc<DenseStore>` 上**没有**
    `upsert_records`：写侧方法在这两个具体类型上。
16. ★ **`#[allow(dead_code)]` 只许用在工作未完成的地方，且必须带注释**。
    当前 13 个结构体上有这个 allow，全部是「构造时注入、方法体还是 `todo!()`」
    的依赖（`CatalogImportService.image_downloader`、`MediaImportService.provider` …）。
    **注释里要写明哪个方法落地后删掉这行** —— 没有注释的 allow 是静音，不是待办。
    反过来：**不许**给「参与 SQL 的行结构体」加 allow（见下条）。
17. ★ **`parity/compare_schema.py` 的豁免名单是硬门槛，别绕**。
    条件 1 是「不参与任何 SQL（没有 `#[derive(FromRow)]`、不被 `query_as` 用）」。
    对「上游根本没有 Peewee 模型的表」造的行结构体，正确出路是
    **改用元组返回**，具名类型放到 `sm-service`（参考
    `sm_db::repo::movie::MovieResolutionLevelRow`）—— **不是**加进豁免名单。
18. **`Self::` 在 `//!` 模块级文档里无效**。rustdoc 会报
    `unresolved link`，而它只在 `RUSTDOCFLAGS=-D warnings` 下才成为错误。
    模块文档里引用关联函数要写全路径（`[`Foo::bar`]`）。
19. **`clippy::assertions_on_constants` 在「常量钉子」用例上是误报**。
    `movie_heat` 里 `assert!(COMMENT > WATCHED)` 两侧都是 `const`，
    而那条用例的目的**正是**把权重比例钉死。加
    `#[allow(clippy::assertions_on_constants)]` + 说明理由，
    不要为了让 lint 闭嘴而把常量改成运行时值。
20. ★ **「哪些地方引用了 X」这种清单，必须与 schema **对拍**，不许手写**。
    `IMAGE_REFERENCE_SITES` 手写的结果是 5 项（实际 8 项）且表名写错
    （`plot_image` 不存在）。它的用处是**删除路径**上的判据 —— 漏一处就是
    删掉在用的数据，且不报错。正确做法是写一条集成测试直接读
    `information_schema` 的外键约束（见
    `crates/sm-service/tests/image_reference_sites.rs`），双向对拍。
    同类风险还有：`ENUM`/状态常量与 DDL 的 `CHECK`、JSON 字段清单与前端
    DTO 的键集合 —— 凡「清单」都要有机器对拍，没有就等着它悄悄过期。
21. ★ **凡「目录/文件名怎么算」，先查上游有没有现成函数，别自己推**。
    骨架把缩略图目录猜成扁平的 `<root>/thumbnails/<media_id>/`，上游其实是
    `movies/<sha1 前2位>/<番号>/media/<media_id>/thumbnails`。这类错误**不会报错**
    —— 缩略图写进一个没人找得到的目录，只表现为「图没了」。
    同类还有：分片名必须 SHA-1（不是「随便一个哈希」）、归一化必须四类资产共用、
    包名必须与目录同级同名（`image_pack_relative_path` 靠这条反推）。
    **判断依据一律是 `common/media_paths.py` 的原文。**
22. **判据与 SQL 不许分家**。`IMAGE_REFERENCE_SITES`（清单）与
    `IMAGE_REFERENCED_SQL`（SQL）同放 `sm_db::repo::image`，
    `sm-service` 只 `pub use` 回来。分家就会出现「查的时候没人用、删的时候
    有人用」。同理「查引用」与「删记录」必须在**同一个事务**里，
    且 `SELECT ... FOR UPDATE` 钉住目标行（新建外键引用要取 `FOR KEY SHARE`，
    两者互斥）。
20. ★ **对拍门禁定性要看 `#[derive(FromRow)]`，不要看名字**（详见「一之一之三」）。
    带 `FromRow` 且被 `query_as` 用的投影行 → 改成 `pub type X = (...)`；
    手工拼的聚合值对象（没有 `FromRow`）→ 合法豁免。
    元组改造的写法：**在循环头一次解构取名**，不要在调用点散用 `.0`/`.4` ——
    `let (a, b, c, _) = item;`，之后全用名字。位置含义只在类型别名的文档里
    定义一处。
21. ★ **别用全文批量替换去改字段访问**。我做 `item.thumbnail_id` →
    `*thumbnail_id` 时误伤了另一个函数里同名的表达式（那个循环没有解构），
   靠 `cargo check` 才发现。批量替换前先确认那个字符串在本文件里只出现在
   你想到的作用域中。
22. ★ **每落完一块就跑 `pwsh -File scripts/progress.ps1 -Write` 并提交
   `docs/progress-baseline.md`。** 它是**唯一权威**的进度数字
   （`docs/service-progress.md` 是叙述，数字会滞后）；`scripts/verify.ps1|sh`
   里新加了一条 `-Diff` 门禁，改了代码不更新基线**会在门禁里失败**。
   **报进度必须两个数一起给**：已注册端点数 **与** 其中 handler 还是
   `todo!()` 的条数。2026-10-06 实测：端点 175/177「已注册」，但 63 条
   handler 还是 `todo!()` —— 只报前一个数字就是「99% 完成」这种假进度。
   同理，行数比**不是**完成度（本仓注释占大头），要判断完成度看 `todo!()`。

## 四之二、验证阶段的教训

**铺开阶段（约 10,300 行、19 轮提交）零编译**，代价在第一次 `cargo check` 时
兑现：查出 **99 个错误**（`sm-db` 24 + `sm-service` 75），其中**约 70 个是前几轮的
遗留**，只有 6 个是最后两轮写的。

这个比例说明两件事：

1. 骨架期不编译**确实会积压**技术债，但换来的是「错误能按文件聚类一次看清」——
   75 个错误里 60 个集中在 4 个文件。若每轮都编译，这些会在写下当场被拦下，
   而重构速度会明显下降。**对「大量铺面」的任务这个取舍是划算的。**
2. ★ **写 numerically plausible 的值比缺符号更危险**。`movie_heat` 的权重与
   `clamp` 是靠读上游原文抓出来的（见下条），编译器永远抓不到。
   这一轮又抓到四处**同一类**（都在缩略图任务里）：

   | 形状 | 骨架 | 上游 | 为什么难发现 |
   |---|---|---|---|
   | 百分比 | 60% 向上取整 | **85% 向下取整** | 60% 看着同样合理，且"留有余地"像是刻意的 |
   | 退避曲线 | 指数 `base << n` | **线性 `base × n`** | 前两次的值**完全一样**（900/1800），第 3 次才分叉 |
   | 边界 | `old + 1 > 2` | `new >= 2` | 只差"多一次重试"，不报错不崩 |
   | 集合成员 | 含 `provider_not_installed` | **不含** | 「provider 没装」看着就是终态 —— 而它恰恰是**装好就能过**的 |

   结论：**凡常量、百分比、退避公式、错误码集合，逐字从上游抄，并写一条测试把值
   钉住**。最后一条尤其反直觉：`provider_not_installed` 进了终态，等于用户装好插件
   之后系统**再也不会重试** —— 而这是"多一个码少一次无效重试"的直觉给出的错答案。

## 四之二之二、清错误时**没有**遇到设计层面的问题

值得记一笔：那 75 + 10 + 6 + 12 + 1 个错误，**没有一个**需要改设计。
全部属于三类：

1. **符号/形状没对齐**（`ImageSearchIndexService` 的两个字段写成了
   `Arc<DenseStore>` 而写侧方法在具体集合类型上；`MoviePlotImageSearchService`
   只挂了一个 `MovieFeatureRepository` 而它既要会话表又要剧情图回表）
2. **骨架期漏写的辅助函数**（`new_session_id` / `vector_json` /
   `parse_query_vector` / `parse_id_list` —— 四个都在 `image_search.rs` 补上了）
3. **`#[cfg(test)]` 里的手滑**

原判断「`sm-api` 会暴露骨架阶段自造的 trait 与 `sm-plugins` 真实 ABI 不匹配」
**没有发生** —— `sm-api` 只有 10 个错误，全是缺 import 与重复定义。
插件 ABI 那批要么早就被前几轮消掉了，要么压根不存在。

## 四之三、上游核对纪律（血泪）

**绝不凭印象写具体数值、权重、公式或字段名** —— 必须打开
`upstream/sakuramediabe/…` 读原文并核对行号。

反面教材：`catalog/movie_heat.rs` 我写了权重 `0.40/0.25/0.20/0.15` 并加
`clamp(0, 1)`。上游实际是 `7/34 · 5/34 · 17/34 · 5/34`（**评论数占绝对主导**）
且明确「**不设置热度上限**、参考值以上继续线性增长」。

`clamp` 的危害特别隐蔽：它会让上百部爆款影片的热度**完全相同**，把「保留头部
原始计数差异」这个设计意图彻底抹掉，而头部差异正是热度排序的主要信息 ——
**编译能过、测试也可能过、数据静默错**。

宁可只写签名 + `todo!()` 并注明「公式待照上游 `:行号` 核实」，也不要填一个
看起来合理的值。


## 五、上游的两处「缺陷」，已刻意照抄

- `/movies/latest` 的 `total` 不带黑名单过滤（与当页口径不一致）
- 订阅端点的 `updated_count` 是双重计数（跳过没写的也算进去了）

照抄理由与测试都写在对应模块文档里。**要修请单独开一个 fix**（会影响客户端已渲染的数字），不要顺手改。

## 六、待确认/待办

### 2026-10-07 新增（两条要拍板，别自己决定）

- **★ `system/telemetry.rs` 去留未定**（骨架期把整个文件建错了概念）。三条走法见
  §七.5 第 1 条。该文件两个 `todo!()` 已标注「**未定夺**」—— 别照字面实现。
- ~~**`/daily-recommendations` 是「一个功能」，不是「接线」**~~ —— ✅ **已完成**：
  读侧 `c801441`、生成侧 `2655f4d`（原清单见 §七.5 第 2 条）。实证仍见 `ce88fd1`。

### 更早登记、仍未清

- ~~`scripts/run-tests.sh`（未跟踪）引用了不存在的 `scripts/test_targets.py`~~ —— **已失效**：工作区 0 个未跟踪文件，`scripts/run-tests.sh` 本身已不存在。
- `stash@{0}` 还在（**2026-10-07 复核：仍在**），内容**不是**上面那条 —— 是「契约层拆仓」那批（根 `Cargo.toml` 改 `git + tag = "v0.1.0"` 依赖、删 `crates/sm-plugin-api/` 与 `proto/`，10 文件 -2315 行）。契约 crate 本身已验证能编译，**三个引用方（`sm-plugins` / `sm-server` / `plugin-ref-local`）当时未验证**就存起来了。取出前先跑一遍那三个 crate 的 `cargo check`。
- 前端契约对拍还没做（前端已在 `upstream/sakuramedia`）。已知两处可能与前端不一致：分页响应多一个 `synced_at: null`；时间戳是 naive UTC 而上游是运行时本地时区。
- **占位用例**：`catalog/movie_asset_pack.rs` 的
  `a_movie_without_images_is_not_an_error` 目前只是**类型级的钉子**
  （`matches!(Ok(false))`）—— 因为 `rebuild_movie_asset_pack` 还是 `todo!()`，
  真调用要 DB 与图片目录。该函数落地后**必须换成真实调用**，
  否则这条用例会一直「绿着但什么都没验」。同类占位用例在做 parity 时一并排查。

## 七、交接快照（2026-10-07，HEAD `803337f`）

**开工前先做两件事**：`pwsh -File scripts/progress.ps1 -Diff`（应回 `OK`）与
`cargo clippy --workspace --all-targets -- -D warnings`（应 exit 0）。
工作区干净、门禁绿、`todo!()` **63** 个（口径见 `docs/progress-baseline.md`）。

### 7.1 这一批刚落地的（最近 8 个提交）

| 提交 | 内容 |
|---|---|
| `c3bbdb7` | **feat(catalog)：JavDB provider 的三个核心方法**（搜索 / 详情 / 图片 URL 归一）+ `MetadataProvider` 转 async |
| `803337f` | **feat(movies)：单片订阅 / 退订 / 相似影片三条端点**（66 → 63） |
| `2b06074` | **feat(plugins)：卸载端点**（69 → 66，`system` 域清零）+ **纠正骨架期自造的「四步清理」**（见 §7.2d） |
| `8656a21` | **feat(plugins)：安装 / 升级两条端点**（71 → 69）+ 流式上传 + 版本比较子集 |
| `7933f56` | **feat(plugins)：`/system/plugins` 的列表 / 详情 / 启停三条落地**（74 → 71），并新增 `PluginAdmin` 契约层 + `installer::publish` + `inventory` |
| `0c0ef57` | feat(plugins)：插件包安装的核心机制（`manifest` 解析 + 安全解压） |
| `6a3d7d5` | docs(readme)：测试数与进度表对齐实测 |
| `aeff054` | **feat(catalog)：`import_by_number` 落地** —— 入库路径第一段通了（窄接口补两支 + `DeliverySource` 枚举） |
| `6020a8e` | **feat(abi)：`ABI_MAJOR` → 2 + 契约两仓同步门禁**（`parity/check_contract_sync.py` 接进 verify） |
| `a15a09b` | docs(tasks)：proto P1 缺口提案（纠正「三个待决策」的认知，指出契约分叉是 P0） |

⚠️ 推送状态以 `git status -sb` 为准（`cnb` 从头到尾没推过；`origin` 推不推由你定）。

### 7.2 `/media-libraries` 这批的两个决定（照做，别回退）

1. **`DELETE` 不幂等**：不存在 → `404 media_library_not_found`；被 `Media` **或**
   `DownloadClient` 引用 → `409 media_library_in_use`。骨架期写的「不存在仍 204、删除幂等」
   是错的。服务层 `delete_library` 也补上了「被下载器引用也算 in_use」（原来只查 `Media`，
   会把仅被下载器引用的库删成孤儿行）。
2. **`MediaLibraryRegistry` 全仓没有实现**：`AppState::with_media_library_registry` 这个 seam
   是活的，但组合根**故意没接**（`sm-server/src/lib.rs:160-163` 有注释）。所以生产里
   `POST /media-libraries` 现在**一律 503 `provider_not_installed`**、`providers` 目录**空表**；
   只有 DB-only 路径（列表 / PATCH / DELETE）可用。**别**把它当 bug「修」成 200 ——
   要真能用，得照上游实现注册表（属插件 ABI 那批）。

### 7.2b 插件管理这批的四个决定（照做，别回退）

1. **`PluginAdmin` 走 trait 注入，不把 `sm-plugins` 加进 `sm-service` 的依赖。**
   契约（DTO + trait）在 `sm_service::system::plugins`，实现在
   `sm_plugins::admin::PluginAdminService`，组合根 `sm-server` 注入
   `AppState::with_plugin_admin` —— 与 `storage` / `downloads` / `media_libraries`
   同一套。理由写在 `crates/sm-service/Cargo.toml` 那条「刻意不依赖」的注释上：
   反向依赖会闭掉一条**将来**的环（插件任务接 cron 触发时要
   `sm-plugins → sm-scheduler → sm-service`）。**方向只能是 `sm-plugins → sm-service`。**
2. **没注入时 500 `plugin_admin_unavailable`，绝不返回空列表。** 「组合根漏了接线」
   与「一台插件都没装」都会让插件页面空着，但前者是故障。这条有测试钉住
   （`tests/plugins_http.rs::without_injection_...`）。
3. **`installer::publish` 的三步顺序不能换**：丢包里的 `data/` → **先把旧
   `data/` 搬进暂存** → 再删旧目录、rename 就位。第 2、3 步换过来的话，
   `remove_dir_all(target)` 已经把用户数据删了，表现为「升级一次插件就失忆」，
   **不报错**。
4. **`load_status` 目前只有「清单坏掉」这一种来源**（上游还有「import 阶段加载
   失败」与「依赖装不上」两种，来自 `PLUGIN_LOAD_ERRORS`）。缺口记在
   `sm_plugins::admin` 的模块文档里 —— 补它需要一条「supervisor 运行时状态 →
   管理接口」的通道。**别把它当已完成**：插件页现在不会显示「进程没起来」。

### 7.2c 安装 / 升级这批的四个决定（照做，别回退）

1. **上传走 `extract::receive_to_file`（流式落盘），不用 `extract::Multipart`。**
   后者是「整字段读进 `Vec<u8>` + 8 MiB 上限」；插件包上限 **100 MiB**，几台并发
   上传就能把 NAS 的内存吃掉几百 MiB。上游也是 `copyfileobj` 到临时文件。
   `receive_to_file` 的总量上限是**整个请求**的（文件 + 文本字段）——只算文件的话，
   一个巨型普通字段就是绕过闸门的路。
2. **超限的码是 `plugin_too_large`（413），不是通用提取器的 `http_error`。**
   `Content-Length` 预检在**读 body 之前**（上游 `_check_upload_size`）；
   分块传输与非法 `Content-Length` 一律放行，由 `receive_to_file` 兜底。
3. **版本比较是 PEP 440 的常用子集**（`sm_plugins::versions`），不是 semver、
   也不用 `version_compare` 之类的依赖。不支持 epoch（`1!2.0`）与本地标记
   （`1.0+local`）—— 那两个形态的后果写在模块文档里，结论是「不会静默把在用的
   插件换成旧代码」。**比较要分两趟**（先全部数字段、再后缀），一趟会在
   `1.0.1` vs `1.0.post9` 上判反。
4. **`enable` 的布尔解析用 pydantic 语义**（`1/true/t/yes/y/on` 为真），与
   `sm_server::config::parse_bool`（`slow_log` 的白名单）**不是同一套**。
   混用会让 `enable=yes` 在一个端点上生效、在另一个上变成 `false`。

### 7.2d 卸载这批的一个**纠正**（照做，别把自造的加回去）

骨架期 `plugin_removal.rs` 的模块文档写「卸载一个插件要做**四件事**：停进程 → 释放
字段主权 → 清扩展点数据 → 删配置行」，并据此造了一个七字段的
`PluginRemovalReport`（含 `incomplete` / `mark_incomplete`）与两条单元测试。

**那四步在上游不存在。** 逐条核对 `plugin_removal_service.py:43-48`，
`remove()` 只有两步：

```python
cls._ensure_not_in_use(manager, plugin_id)   # 1. 占用检查 → 409 plugin_in_use
manager.remove(plugin_id)                    # 2. 停用 + 删代码（保留 data/）
```

| 骨架声称要做 | 上游实况 |
|---|---|
| 停插件进程 | **不是卸载的一部分**。`remove` 只删文件 + 写 `enabled`；进程等重启（路由回 `pending_restart: ["api","aps"]`）|
| 释放 `field_owners` | `release_plugin_owners` 全仓**只有一个**调用点：`commands.py:524`，一个 CLI 子命令（`movie_ownership_gateway.py:189` 写着「清理端点，CLI 调用」）。没有 HTTP 路由，卸载路径零调用 |
| 清扩展点数据 | `ranking_source` / `metadata_source` 的删除在卸载路径**零命中** |
| 删配置行 | 只有 `manager.remove` 内部那次 `_set_enabled(False)` |

**为什么这次纠正重要**：多做的那些事会**改变行为** —— 卸载时顺手释放字段主权，
等于替管理员做了一个他没要求的、影响**所有影片**的动作（`release_plugin_owners`
是全表 UPDATE）。所以这不是「简化」，是修一个会误伤数据的实现。CLI 那条路仍是
它自己的入口；将来若要做，应该照上游做成独立命令，别塞进
`DELETE /system/plugins/{id}`。

### 7.2e 卸载的**占用检查目前不生效**（★ 已知缺口，别当它做好了）

`DELETE` 会先问「这个插件的 provider 还挂着媒体库吗」，被引用就 409
`plugin_in_use`（details 五个键与上游 `PluginInUseError.details` 逐字一致：
`plugin_id` / `provider_keys` / `library_ids` / `media_count` /
`download_client_count`）。DB 那半是完整的、也有真库测试。

但那个问题要「`plugin_id` → `provider_key`」的**反向索引**才答得上来，
而上游的两条来源在 Rust 侧都不可用：

| 上游来源（`:74-93`） | Rust 侧 |
|---|---|
| `MEDIA_PROVIDER_REGISTRY.provider_keys_for_plugin()` | `MediaLibraryRegistry` **全仓没有实现**（§7.2），连 `library_for(provider_key)` 那一向都没人接 |
| 回落到 `check_plugin_dir()` 试加载插件目录、扫 `MEDIA_PROVIDER_EXTENSION_KEY` | Rust 没有「import 插件」；要拿扩展点得**拉起进程**问它 `Register` |

所以路由传的是 [`NoProviderKeys`](crates/sm-service/src/system/plugin_removal.rs)
（恒空）→ **检查形同虚设，一个仍被媒体库引用的插件能被删掉**。

⚠️ **这与上游不完全等价**：上游在「插件目录加载失败」时也会返回 `()` 放行，
但一个**正在服役**的插件在上游是走第一支的、查得到键。

补它的前置只有一件事：**provider 注册表带上反向索引**。本仓那条 seam
（`ProviderKeyIndex`）就是为了那时能接上；`tests/plugin_removal.rs` 里有一条
**故意断言「空索引会放行」**的测试 —— 哪天注册表接上了，那条测试会失败，
正好把它改成断言 409。

### 7.2f ★ JavBus provider **整个不存在**（阻塞 6 条端点，不只是 SSE）

2026-10-07 查清。它不是「SSE 麻烦」，而是**一整块外部客户端没写**：

| 上游 | 规模 | Rust 侧 |
|---|---|---|
| `src/metadata/_providers/javdb.py` `JavdbProvider` | **975 行** | **无**（`crates/` 里搜不到任何 JavBus 客户端）|

Rust 侧**已经有**接缝，缺的只是实现：

- `catalog::metadata_source::MetadataProvider`（`fetch_movie` 等）与
  `MetadataSourceService` / `RegisteredSource` **都在**；
- `movie_metadata_search.rs` 的 `resolve_candidate_reference` /
  `javdb_candidate_id` / `cleanup_search_assets` **都已实现且有测试**；
- 只差「按番号搜 / 按 javdb_id 取详情 / 封面 URL 归一」这三个动作。

被它阻塞的端点（6 条）：

| 端点 | 需要 JavDB 的哪部分 |
|---|---|
| `GET /metadata/javdb/search` | `_search_movie`（`javdb.py:386`）|
| `POST /movies/{}/metadata/refresh` | `get_movie_detail`（`:441`）|
| `POST /movies/search/javdb/stream`（SSE）| 同上 + 逐部入库 |
| `POST /actors/search/javdb/stream`（SSE）| `search_actor` / `search_actors`（`:326`/`:373`）|
| `GET /movies/{}/reviews` | `get_movie_reviews_by_javdb_id`（`:444`）|
| （间接）`ranking` 的两个榜单源 | `get_rank_numbers` / `get_playback_rank_numbers`（`:483`/`:611`）|

⚠️ 别把它当「照抄一个 HTTP 调用」：上游那个类还带**设备指纹 + 登录态**
（`_device_payload` `:503`、`_ensure_logged_in` `:508`）、**图片 URL 归一**
（`_normalize_image_url` `:125`，把 `covers/…` 换成 `c0.jdbstatic.com`）以及
多种资源（封面/样本图/头像）的分支。照抄要连带 Cookie 与 UA 处理。

****第二步的两个新发现（2026-10-07 晚，`c3bbdb7` 之后查的）** —— provider 有了，
但「接端点」比预想的多两道：

1. **`metadata-refresh` 的响应是 `MovieDetailResource`**
   （上游 `movies.py:207` `@router.post("/{movie_number}/metadata-refresh",
   response_model=MovieDetailResource)`）。而本仓的
   `MovieService::get_movie_detail` **不存在**（`movies.rs:774` 只有路由骨架）。
   也就是说这条端点**同时**卡在 JavBus **与** 详情汇合点上 —— 只解 JavBus
   不够。
2. **上游没有 `javdb.host` 这个配置键**（`config.py` 里搜不到 `javdb`），
   而 Rust 的 `sm_core::config_schema` 里也没有。所以「provider 用哪个 host」
   是**一个还没答案的问题** —— 要先把上游那个 host 是从哪来的查清
   （可能是插件配置或硬编码），不能凭空在配置表里加一个键。

**第一步已落地**（见 §7.1 最新一条提交）：`catalog::javdb::JavdbProvider`
实现了那三个方法，并把 `MetadataProvider` 转成 `async`（原来同步，而出网客户端是
reqwest —— 同步签名会被逼成 `block_on`）。**还没接端点**：它目前没有被组合根
构造出来；`search_actors` 显式报「尚未移植」而不是返回空列表（空列表会让
调用方报「导入 0 个」，那是谎报）。

**建议的切法（剩下的部分）****：先只做 `_search_movie` + `get_movie_by_javdb_id` +
`_normalize_image_url` 三个（够 `GET /metadata/javdb/search` 与
`metadata-refresh` 两条**非 SSE** 端点），并用回环 HTTP 服务打桩做可测的
部分（见 `tests/embedding_http.rs` 与 `wiremock` 的用法），再接 SSE。
登录态（`_ensure_logged_in`）**先不做** —— 未登录也能拿到这三条要的数据。

### 7.2g ★ 图搜那 9 条**不缺客户端**（纠正一个错判，2026-10-08）

先前把 `image_search` 那 9 条标成「卡在 Qdrant / 嵌入客户端缺失」。**那是错的** ——
客户端早就写完了，逐个核实：

| 模块 | 行数 | `todo!()` |
|---|---|---|
| `sm_service::discovery::qdrant::dense` | 631 | **0** |
| `sm_service::discovery::qdrant::similarity` | 552 | **0** |
| `sm_service::discovery::qdrant::thumbnail` | 170 | **0** |
| `sm_service::discovery::qdrant::plot_image` | 167 | **0** |
| `sm_service::discovery::embedding`（SigLIP2 HTTP） | 515 | **0** |

`EmbeddingClient` 的 `describe` / `embed_images` / `embed_texts` 都在，
`dense.rs` 里连 collection 名（`media_thumbnail_vectors_siglip2_v1`）与
payload 索引声明（`THUMBNAIL_PAYLOAD_INDEX = ["movie_id", "media_id"]`）都齐了。

**真实卡点是「上游语义没读」**，骨架期自己写下的（`image_search_space.rs:316`）：

> 「照上游 `image_search_input.py` 实现（848B；**校验规则待确认，不猜**）」

也就是说 `normalize_image_search_query` 缺的是**上游那 848 行的校验规则**
（尺寸上限、格式白名单），而不是缺一个 HTTP 客户端。骨架作者当年没读透，
所以刻意留空 —— 这个判断是对的，**别把它当成"接线"顺手填一个**。

其余几条要读的上游文件：

| 端点 | 要读的上游 |
|---|---|
| `POST /image-search/sessions`（图搜会话） | `image_search_service.py` 的会话创建段 |
| `GET …/results` 两条 | ✅ **已落地**（见下）|
| `POST …/text-sessions` 等四条 | `image_search_input.py`（**18 行**，见 §7.2 那条纠正；尺寸上限与格式白名单**不在**这里，在 `image_search_service.py`）|

#### ✅ `GET …/results` 两条已落地（2026-10-08），顺带清掉两个错记

落地时才发现两处**先前记错/没做**的事：

1. **两条不是纯接线 —— 但也不是缺零件。** `ImageSearchService::list_results`
   与 `MoviePlotImageSearchService::list_results` 都在（零 `todo!`），可**全仓
   没有一处构造过这两个服务**。真正缺的是**组合根装配**，已在
   `sm_server::build_image_search_services` 补上（照 `build_similarity_store`
   的模式：未启用 → `Ok(None)`，建连失败只 warn 不让进程起不来）。

2. **「未启用」该是 **409** 而不是空列表。** 上游把 `require_image_search()`
   挂成 **router 级依赖**（`image_search.py:30`），未启用时六个端点统一
   `ApiError(409, "feature_disabled", ...)`（`optional_services.py:22-24`）。
   本仓没有 router 级依赖机制，所以在每个 handler 里显式取一次 —— 判据是
   「组合根有没有挂上服务」。**别照 `similarity` 那侧写成降级空列表**：
   相似度只是推荐信号之一，而图搜是用户主动发起的整个功能，返回空结果会让
   用户以为「搜过了、没有」。

#### ⚠️ 纠正一处**我上一轮写错并误导了验收**的顺序（2026-10-08）

这一节先前写着「空 `cursor` 要 422，**先于**能力检查」，理由是「pydantic 参数校验
在依赖注入之前」。**那个理由不成立**，而我当时的网络测试是照自己的实现去验的，
等于自己给自己发了一张合格证。

真相（FastAPI `solve_dependencies`，`fastapi/dependencies/utils.py`）：

1. 它**先**跑 `dependant.dependencies` 的循环 —— `APIRouter(dependencies=[...])`
   里的 `require_image_search` 就在这一层；
2. `request_params_to_args`（query / form / file 的参数校验）在循环**之后**；
3. 而 `require_image_search` 是直接 `raise ApiError(409, ...)`，**不是**累积进
   `errors` 列表 —— 所以一旦未启用就立刻返回，参数校验根本没机会跑。

所以：**未启用时，一切参数错误（空 cursor、缺 file、坏 CSV、错误 content-type）
都该是 409，不是 422。**

在这个框架下把能力检查做成提取器而不是 handler 里的一行，原因也变了 —— 见下。

#### ✅ 四个建会话端点已落地（同日），三条工程要点

**1. 能力检查必须是 `FromRequestParts` 提取器，不能写在 handler 里。**

axum 的提取器按参数顺序解析，读 body 的（`Multipart` / `Form`）必须在最后。
把 `RequireImageSearch` / `RequirePlotImageSearch` 做成 `FromRequestParts` 并放在
`Multipart` / `Form` **之前**，才等价于上游那个 router 依赖的位置。

写成 handler 体内的一行则不行：`Multipart` 那时已经解析完了，
「未启用 + 错误 content-type」会得到 422 而不是 409。

**2. `extract::Multipart` 加了 `receive_file_with_fields`。**

`next_file` **跳过**非文件字段（过滤参数会丢），`receive_to_file` 把文件**写盘**
（图搜要的是立刻送进推理服务的字节，落盘再读回是绕路）。所以补了这个方法：
第一个文件进内存 + 收下所有文本字段，文件和每个文本字段各有一道上限。

**3. `parse_csv_positive_ints` 先前有两处偏差（已修）。**

| | 上游 `_utils.py:21` | 骨架期写的 |
|---|---|---|
| 状态码 | **422** | 400 |
| 空串 | **报错** | `Some(vec![])` |

空串那条尤其危险：把「客户端拼了个空变量」静默变成「排除全部」，用户看到零结果
而不知道为什么。它当时**无人调用**，所以改动零风险 —— 这也说明写它的人没去看
调用方，是直接照自己的设想写的。

#### ⚠️ 留给下一轮的契约偏差（**已知，未修**）

两个 session page DTO 与上游 resource **不齐**：

| | 上游 `...SessionPageResource` | 本仓 DTO |
|---|---|---|
| 字段 | `session_id` / `status` / `page_size` / `next_cursor` / **`expires_at`** / `items` | 只有 `session_id` + `items` + `next_cursor` |

图搜侧服务层 `list_results` 本来就只返回 `ImageSearchPage`（`items` +
`next_cursor`），剧情侧返回的是平铺的 `PlotImageSearchSessionPage`（有
`status` / `page_size` 但被拆开了）。**两边互不一致，且都与上游不齐。**

**建议留到接四个建会话端点时一并处理**：那时 `ImageSearchSessionPage` 也要凑
齐同一组字段，改一处能一起验，比现在单独改两个 DTO 更不容易漏。

**顺序建议**：先读 `image_search_service.py` 的会话创建段（四条建会话端点共用），
再读它的过滤解析。读完就把 §7.3 里那行的「Qdrant / 嵌入探测客户端缺失」
改成真实卡点，别让下一个人以为要去写客户端。

Qdrant 的**容量实测**（10 万向量 @1152 维：磁盘 497 MB、内存 534 MB /
`on_disk` 71.6 MB，峰值是稳态的 2 倍）见 [`deployment.md` §3.4](deployment.md) ——
那里也记着两处**待实测校正**的偏差（没建 payload 索引、每部媒体几张缩略图未测）。

### 7.2h ★ 一批**陈旧卡点注释**在骗人；以及一次「照文档改代码」的翻车（2026-10-08）

这一轮没动业务代码，做的全是**核对**。三条结论，都会影响后面怎么排活。

#### 1. 「本仓还没有 image store」是**假的** —— 多处注释还这么写

`crates/sm-service/src/catalog/image_store.rs` **早就存在、零 `todo!()`**，而且
`crates/sm-api/src/routes/files.rs:135` 已经在用它（`read_image_bytes`）。但下面这些
地方的注释仍在说「本仓还没有那一层」：

- `discovery/moment_recommendation.rs:378` —— 「唯一还缺的是读种子图字节 …… 本仓库
  还没有 image store 模块」。**那句话已经过期**：`image_store::read_image_bytes(root,
  path)` 就在那儿。真正缺的是 **image root 从哪来**（服务只持有 store + embedding，
  而 `media_paths::media_image_root_path(config)` 要 `ConfigService`）+ **种子行不带
  `image.origin`**（`MomentSeedRow` 是 6 个 `i32`，位置说明见 `repo/moment.rs:81`；
  `MediaThumbnailRow` 带 origin，种子行不带）。
- `catalog/catalog_import.rs:41/60/173/215/482` —— 五处同款说法。

**教训**：卡点注释写下来就会过期，而过期的卡点会让人**整片跳过**可做的工作。下次看到
「本仓还没有 X」，先 `Get-ChildItem -Filter X*` 搜一下再说。

#### 2. `status.rs` 元数据源探测：卡点不是「`metadata` 域」，是 **JavDB host 从哪来**

模块文档原先写「阻塞：`metadata` 域」。核实后：零件都在 ——
`JavdbProvider` 已实现 `MetadataProvider::get_movie_by_number`（`javdb.rs:244`，零 todo）。
真正缺的：全仓**没有任何生产代码构造过 `JavdbProvider`**，也没有 host 常量或配置键
（`config_schema.rs` 的 `metadata` 节只有两个 gfriends URL）；上游是在
`metadata/factory.py:15` **硬编码** `JAVDB_HOST = "jdforrepam.com"`。

→ 实现前要定：跟上游一样硬编码，还是补 `metadata.javdb_host`。**这是待拍板项。**
实现形状（探测用固定番号 `SSNI-888`、失败也回 200 + `error.type`）已记在
`status.rs` 的 handler 文档里，不用再读上游。

✅ **已落（2026-10-08）**：按建议 (a) 硬编码，见 `sm_service::system::status::JAVDB_HOST`。
服务层 + 路由 + 真回环集成测试一起做完（`52 -> 51`）。§7.5 项 3 结案。

⚠️ 但落地时发现：这个端点**能编译、能解析、测试全绿，却对真站一个请求都发不出去** ——
`javdb.rs` 少了 `jdsignature` 头。详见 §7.2i（那条是独立的缺陷，不属于本项）。

#### 3. ★ 我照一份**写错了动词的模块文档**去改代码，把对的改错了

`status.rs` 文件头的表格写着 `POST /status/metadata-provider/test`。我据此把
`get(test_metadata_provider)` 改成 `post(...)`，还写了注释「★ 是 POST，不是 GET」。
**上游是 `@router.get`**（`status.py:60`）—— 代码本来就对，错的是那张表（动词错、
路径也丢了 `providers` 的 `s` 和 `{provider}` 段）。

拦住我的是 **`method_not_allowed_http.rs` 的 405 用例**：改错后它报
`GET /status/metadata-providers/javdb/test 应当是 405`。已全部回滚，只留下：
表格订正、那条用例（现在锁的是「别再照文档改动词」）、以及 `422` 补上的
`details.provider`（上游 `{"provider": provider}`，回显**原始**入参）。

我的「反向验证」（把动词翻回 GET 看用例是否变红）只是证明**测试与我的实现一致**
—— 那是自己给自己发合格证，**不能**用来验证上游事实。要验上游只有一条路：读上游。

#### 4. 顺带把一条推理补上了出处：依赖先于参数校验

§7.2g 那条「未启用 → 409 先于参数校验 422」当时是**照记忆推的**（手上没有 FastAPI
源码）。这轮下到上游锁的版本核实了：`pyproject.toml` 是 `fastapi==0.110.1`，其
`solve_dependencies` 里 `for sub_dependant in dependant.dependencies` 确实在
`dependant.path_params` 之前。结论成立，注释里已补上可复核的来源。

### 7.2i ★ 零 `todo!()` 的组件里也可能藏着致命缺陷：JavDB 请求缺 `jdsignature`（2026-10-08）

`javdb.rs` 从头到尾 **0 个 `todo!()`**，带着一批单测 + 10 个 wiremock 集成测试，
交接文档里一直当它「已完成」。它确实**能编译、能解析**，但对真站**一个请求都发不出去**：

- 上游 `MetadataRequestClient._request:44` 在**每次请求**都调
  `build_request_headers()`；`JavdbProvider` 覆盖它，补上 `jdsignature` +
  `accept-language`（`javdb.py:661-667`）。签名 = `md5(f"{int(time.time())}{SECRET}")`，
  拼成 `{ts}.lpw6vgqzsp.{md5}`，**每请求重算**（服务端判新鲜度）。
- 本仓的 `request_json` 只设了 User-Agent。少了签名，JavDB 回
  `{"success":0,"action":"ParameterInvalid","message":"參數不能爲空: jdsignature"}`
  —— **HTTP 仍是 200**。
- 于是**症状取决于谁看 `success`**：详情路径看了 → 报「请求失败」（还算诚实）；
  **搜索路径不看**（上游 `_search_movie:401` 同样不看，本仓照抄）→ 候选为空 →
  报 `NotFound`。用户看到的是「JavDB 没收录**任何**番号」。

**为什么那 10 个集成测试全绿也没发现**：wiremock **不校验请求头**，断言全在响应
解析那一侧。这类「出网形状错、但解析逻辑对」的缺陷，桩测试**结构上**测不到 ——
所以本轮补了一条会**读回请求头**的用例（`every_request_carries_a_fresh_jdsignature`）。

**修法**：`crates/sm-service/src/catalog/javdb.rs` 加 `signature_at()`，在
`request_json` 挂上那两个头；MD5 自写进 `crates/hashing`（该 crate 的文档明写
「刻意不引 sha1/sha2」，且离线可编是硬要求），带 RFC 1321 七向量 + 一个**跨实现**
算出的固定签名向量 —— 用本仓自己的 md5 去验本仓自己的签名，会恰好漏掉「secret
抄错」这一类。上游头里的 `connection` / `host` **故意不搬**：都是客户端托管的
（HTTP/1.1 默认即 keep-alive；`Host` 由 hyper 按 URL 自动填），显式设置反与连接
复用打架。

**真机验证**：`get_movie_by_number("SSNI-888")` → `id=9G2v6`、真标题 / 标签 / 演员、
`release_date=2020-10-19`，0.72s。

**已知脆弱点（★）**：`SIGN_SECRET` 是上游**硬编码在客户端里的共享密钥**，随官方 App
发版而变。变了而这里没跟，症状就是「所有番号都查不到」—— 真遇到先读
`signature_at` 的文档注释，别从网络层查起。

> 这一条的教训与 §7.2h 是同一枚硬币：§7.2h 是「注释说不行、其实行」，
> 这里是「注释说行、其实不行」。**两边都不能只信文字。**

### 7.2j ★ 图搜状态端点：三个「照着抄也会错」的接法（2026-10-08）

`GET /status/image-search` 落地（`51 -> 50`，路由 `24 -> 23`）。规格逐字可抄，
但**装配层**有三处会撒谎的地方 —— 三者都能编译、单测也能过，只有真连上才看得出来：

**1.「集合不存在」被吞成「健康」。** `DenseStore::exists()` 把错误吞成 `false`
（`search` 路径要求「向量库挂了也当没结果」，见该模块文档第 1 条）。`status()`
原先**复用**了它，于是 Qdrant 连不上时返回 `Ok{exists: false}` → 状态页显示
`healthy: true` + 「还没建集合」。**这正是本端点要消灭的那类误报。** 上游
`_collection_exists`（`qdrant_thumbnail_store.py:117-126`）在这里**不吞异常**
（只有降级分支才 `try/except`）。已改为直接调 `collection_exists` 并 `map_err`。

**2. `enabled` 不等于「服务存在」。** 上游 `enabled` 取**配置开关**；而
`AppState::image_search()` 在「开关开着但 `inference_base_url` 为空」时也是 `None`
（组合根 warn 后不建）。两者合一会把**配置错误**谎报成**功能没开**。已拆成
`ImageSearchProbe { enabled, service }`。该组合下与上游有一处**已知偏差**（上游仍
能探通 Qdrant，我们没有客户端），已在代码注释里写明。

**3. 两个「待索引计数」不是一个口径。** 仓里原有的 `pending_count` 并了
`movie_plot_image`、还加了 `m.movie IS NOT NULL`，那是**任务候选口径**；上游这个
端点（`status_service.py:576-590`）是两次裸 `count()`，是**展示口径**。互换后状态页
的数字与任务实际要处理的量对不上，**且没有任何报错**。新查询单独写在
`count_thumbnails_with_status`，测试里特意播了一条「没有 movie 归属」的待处理缩略图
把它钉住（反向验证过：给计数加回 `movie_number IS NOT NULL` → 该用例红）。

**还有一处字面量不能猜。** `vector_dtype` / `collection_status` 走上游
`_enum_value()`，产出是 **REST 风格小写串** —— 证据是上游自己的黄金用例
（`tests/api/test_status_api.py:201-202`：`"float16"` / `"green"`）。猜成 `Float16`
或枚举序数，都会让客户端按 REST 值匹配时对不上，而线上只表现为状态页那两格显示
怪值。映射写完**对着真 Qdrant 验过**（`status_image_search.rs` 完整分支），并反向
验证过（改回 `Float16` → 该用例红）。

> 与 §7.2h / §7.2i 同源：**能编译、有测试、零 `todo!()`，都不等于接对了。**

### 7.3 剩余 50 条的**卡点表**（按卡点而非按文件归类）

> 总数与分域计数以 `docs/progress-baseline.md` 为准（那份由脚本生成）；下表按
> **卡点**归类，只用来判断「下一步该动哪一块」。
>
> ⚠️ 本表 2026-10-08 重写过一次。旧的「59 条」版把 `recommendations.rs` 标成
> **已解决**，其实那里还留着 2 条（见下）—— 一条「已解决」的笔记会让后面的
> 人整片跳过。**别只信「已完成」的结论，回去数一遍。**
>
> 同日又动了一次：`status.rs` 的元数据源探测落地（`52 -> 51`，路由 `25 -> 24`）。
>
> 同日再动一次：`status.rs` 的图搜状态落地（`51 -> 50`，路由 `24 -> 23`），
> 见 §7.2j。**`system` 域的路由至此清零**（`status.rs` 再无 `todo!()`）。

**路由 23 条：**

| 卡点 | 文件（条数） | 说明 |
|---|---|---|
| **插件 ABI / provider 无实现** | `media_playback.rs` 3、`videos.rs` 3、`media_import.rs` 3、`media_transfer.rs` 2、`download_tasks.rs` 2 | 要 provider 的 `library_handle` / `playback_deliveries` / 下载器注册表。**13 条**，最大一块 |
| **JavBus provider 不存在** | `movies.rs` 2（SSE）、`actors.rs` 1（SSE） | 见 §7.2f |
| **`MovieService` 缺方法** | `movies.rs` 3 | `get_movie_reviews`（要 JavBus）/ `get_merged_playback`（要 provider）/ `refresh_movie_metadata`。⚠️ 三个的**服务层方法都不存在**（不是「有方法只差接线」），别照骨架注释当成接线做。`refresh_movie_metadata` 尤其容易读错：`catalog_import.rs:468` 那个是**已实现的辅助函数** `refresh_movie_metadata_strict`，端点真正要的 `MovieMetadataRefreshService::refresh_movie_metadata`（`movie_metadata_refresh.rs:49`）**本身就是 `todo!()`** —— 所以「JavDB host 定下来」也解不开它（2026-10-08 核过）|
| ~~**`status.rs` 1**~~ | ~~`status.rs`~~ | ✅ **本文件已清零**。`GET /status/image-search` → ✅ **已落**（§7.2j）；`GET /status/metadata-providers/{provider}/test` → ✅ **已落**（§7.5 项 3，依赖 §7.2i 的 `jdsignature` 修复才真能用）|
| **`recommendations.rs` 2** | `recommendations.rs` 2 | `moment-recommendations`：要 `MomentRecommendationService::list_items`（自身也是 todo）+ API 层的 `PageContext` 实现。`hot-actress-releases`：要 `PageContext` 的两个方法（影片卡片 + 女优资料），§7.3 旧版误标已解决 |
| **插件设置 2** | `plugins.rs` 2 | 见 §7.5，要先定「Rust 插件怎么声明自己的设置项」 |

**服务层 27 条**：`transfers` 16（`download_sync` 4 / `media_transfer_task` 4 /
`import_task` 3，其余各 1：`auto_download` / `download_common` / `download_request` /
`download_task` / `provider_browse`）、`catalog` 6（`movie_metadata_refresh` 3 /
`movie_metadata_search` 2 / `catalog_import` 1）、`playback` 3（`media_file_hash_backfill` /
`media_validity_scan` / `media_video_info_backfill`）、`discovery` 2（`moment_recommendation`）。

**按卡点合并后的真相**：`transfers` 16 + `playback` 3 + 路由那 13 条 ≈ **32 条压在同一件
事上 —— 插件 provider ABI**。剩下的才各自有独立卡点。

### 7.4 结论：**逐条「接线」已经没有空间了**

剩下的路由几乎每一条都压在**一个尚未实现的 service / 仓储 / 插件 ABI** 上。
建议改成**按功能纵向推进**：挑一个端点，把「仓储 → 服务 → 路由 → 测试」一口气做完。

**这条路子已被验证**：`daily-recommendations` 按此做完（读侧 `c801441` + 生成侧
`2655f4d`，含仓储查询、服务编排、路由 DTO、17 个单测 + 6 个真库集成测试）。
下一个可照此推进的候选：`actors.rs` 那 1 条（先查卡点）、或 `movies.rs` 里
`MovieService` 缺的那批方法。

### 7.5 **待拍板**项（详版）

1. **`system/telemetry.rs` 去留未定（★）**。骨架期把整个文件建错了概念：上游
   `TelemetryService` 是**匿名心跳上报** —— env `SAKURAMEDIA_TELEMETRY_ENABLED` 控制，
   APScheduler 每小时一次（`start/aps.py:365-374`），把 `instance_id` / `plugins` /
   `managed_media_*` / CPU·内存 POST 到 Supabase 外部端点，**没有 HTTP 路由**、不碰 `task_runs`。
   而本文件的 `TaskTelemetry` / `TaskTelemetryByKey` / `success_rate` 在上游**从不存在**
   （上游 grep `success_rate` / `task_stats` 零命中）；系统级「磁盘占用 / 图搜状态」属于
   `StatusService.get_status()`（`status_service.py:353`）。三条走法：
   **(a)** 照上游重写成心跳（env **默认开** = 默认向第三方上报，需 `sysinfo` 新依赖 + 调度注册）；
   **(b)** 同 (a) 但 **env 默认关**；**(c)** 删掉该文件。
2. ~~**`/daily-recommendations` 是一个功能**~~ —— ✅ **已按 (a)~(d) 全部落地**，
   分两个提交：读侧 `c801441`（(a)(b)(c)）+ 生成侧 `2655f4d`（(d) + handler 注册）。
   生成侧顺带注册了 `daily_recommendation_generate`（worker handler 5 → 6）。

   **落地时发现并修掉的一处「上游对齐」问题**：`CandidateMovie.release_date` 骨架期
   写成 `Option<NaiveDate>`，而 `movie.release_date` 是 `timestamp`
   （上游 `DateTimeField(null=True)`）—— 那样从 SQL 解码就会 `ColumnDecode` 失败，
   而且即便转成功也会丢掉时分秒，让 `freshness` 在同一天内失去区分度。
   已改为 `Option<NaiveDateTime>`；`heat` 一并从 `Option<i64>` 收敛成 `i64`
   （列是 `integer NOT NULL`）。

3. ~~**JavDB 的 host 从哪来**~~ —— ✅ **已结案（2026-10-08）：选 (a) 硬编码**。
   上游是**硬编码**（`metadata/factory.py:15` `JAVDB_HOST = "jdforrepam.com"`），
   本仓原先既没常量也没配置键。两条路：
   **(a)** 照上游硬编码成常量（最省事、与上游逐字一致，但换域名要改代码）；
   **(b)** 补 `metadata.javdb_host` 配置键 + 默认值（部署可改，但要多写校验与文档）。
   选 **(a)**：先对齐行为，配置化等真有人要换域名再说 —— 上游自己都没做成配置。
   常量落在 `sm_service::system::status::JAVDB_HOST`，取舍写在它的文档注释里。

   ⚠️ **它只解开 `status.rs` 那一端点，没解开 `catalog` 那 6 条** —— 那 6 条缺的是
   「服务编排 / worker 接线」（见 §7.3），不是 host。别把本项结案误读成 catalog 也能动了。

### 7.6 纪律（照 §四，别松）

- **绝不凭印象写数值 / 字段名**：打开 `upstream/sakuramediabe/…` 读原文并写行号。
- 改完跑 `pwsh -File scripts/progress.ps1 -Write` 并**一起提交** `docs/progress-baseline.md`
  （门禁 `-Diff` 会拦）。
- **测试按环境分层跑**：本机有库时（`scripts/dev-services.ps1 up`）**优先跑
  `pwsh -File scripts/verify.ps1 -Tier full`** —— 11 项全绿再提交，新写的集成测试
  必须真跑过（2026-10-07 起，本轮 17 个单测 + 6 个真库集成测试就是这么验的）。
  无库环境才退回编译级验证（`cargo check --all-targets` + `clippy`）。
  **一条新测试从没被执行过，等于没写。**
- 一批一提交；提交信息写清「上游行号 + 为什么」。

---

## 八、接下来做什么（执行清单）

**权威依据**：[`deployment.md`](deployment.md)（部署形态与瘦身路线）。§一~§七 讲
「为什么」，这一节只讲**先做哪个、怎么算做完**。

> 判断依据是**卡点**不是行数：`service-progress.md` 的「阻塞地图」已经写明，
> `provider` 这条曾阻塞 24 个文件 / 8,885 行的依赖，**宿主侧早就不是瓶颈了**
> —— 真正的缺口是「还没有真实 provider 插件被移植过来」。

### 8.1 依赖图

```text
① 修契约分叉 ────────► 所有插件（改一次，全部要跟着改）
                            │
② 小插件扫尾（4 个）────────┤
                            ▼
③ 入库路径 ────────────► ②③ 合起来才让「插件产出」真的有用
                            │
④ P1-2（local_path）────────┤ 必须在 ⑥ 之前
⑤ svc-probe（ffprobe）      │（独立，可并行）
                            ▼
⑥ local_provider ──────► 本地库 + 播放闭环（分水岭）
                            │
⑦ 115_provider ────────► Python 运行时才能删（最后一关）
```

### 8.2 六件事与判据

| # | 做什么 | 为什么先它 | 完成判据 |
|---|---|---|---|
| ① | **修契约分叉**（详版见 [`tasks/proto-p1-gaps.md`](tasks/proto-p1-gaps.md)）。~~原先写的是「proto 三个缺口决策」~~ —— 核对后发现 **P1-1 / P1-3 / P1-4 都已在本仓落地**，剩下的是「两仓契约不同步」：宿主的 `proto/` 与 `src/` 已前进，而契约仓 tag `v0.1.0` 是旧版，`ABI_MAJOR` 两边还都是 1 | 旧插件**能编译但跑不通**：`GenerateThumbnails` 两侧消息类型不同（旧 `stream ProgressEvent` vs 新 `stream GenerateThumbnailsResponse`），`field 2` 的 wire type 不匹配 → 宿主报「解码失败」，而真实原因在日志里看不到 | ⚠️ **本仓侧已做完**（`6020a8e`）：9 个文件同步进契约仓（**本地提交**）+ 两仓 `ABI_MAJOR` = 2 + tag `v0.2.0`（**本地未推**）+ 门禁 `parity/check_contract_sync.py`（已接进 verify，人为漂移验证过会红）。**剩下三步：推契约仓 → 两个插件改 tag → `plugin-ref-local` 补 `done` 帧** |
| ② | **小插件扫尾**：`judge_collecttion_movie`(5.7KB) → `javdb_ranking`(10KB) → `subtitlecat`(23KB) → `actor-metadata`(30KB) | 每个插件都要缴一遍「生命周期协议 + 注册 + 交付校验」的税；**小插件把这笔税缴完**，后面的大插件才只处理业务逻辑。样板已有两个 | 二进制 `<plugin_id>` 能被宿主拉起；扩展点被 `collect_extensions` 收下；`run_job` 有实现（不返回 `unimplemented`） |
| ③ | **入库路径**（`catalog` 域的「插件元数据 → 库表」） | **当前最被低估的缺口**：`docs/tasks/javbus-metadata.md` §二 写着「拿到校验过的结果也没处写」。不补，② 的插件全是空转 | ⚠️ **第一段已通**（`aeff054`）：`import_by_number` + 窄接口补 `find_movie_id` / `import_plugin_movie` + 4 单测 + 3 个真库测试。**剩 `impl MovieMetadataImporter`** —— 卡在接口冲突（那个 trait 的方法签名**没有 config**，而 `fetch` / `fetch_plugin` 要看 `plugins.enabled` 的顺序），两条走法待拍板 |
| ④ | **P1-2 决策**：`PlaybackPlan` 加 `local_path` delivery | 同域反证：`OpenCoverSourceResponse` 早有 `oneof { local_path, url }`，唯独播放计划没有。**必须在阶段 ⑤ 之前定**，否则 `local_provider` 要先按 `file://` 写一遍再改 | `PlaybackPlan.oneof delivery` 有 `LocalPathPlan local_path = 3`；`docs/plugin-abi.md` 写明三种 delivery 的适用场景 |
| ⑤ | **`svc-probe`（ffprobe）** | 解锁 4 个文件（`media_metadata_probe` 338 / `media_video_info_backfill` 232 / `thumbnails/artifacts` 204 / `video_cover`）。上游对 PyAV 缺失是**降级**，所以它不阻塞「能用」但阻塞「完整」 | 上述文件的 `todo!()` 清零；媒体时长/分辨率被真写入 |
| ⑥ | **`local_provider` 的 Rust 版**（172KB，**分水岭**） | 做完同时发生三件事：本地库+播放闭环、**`libtorrent` 整条可扔**（`deployment.md` §3.1：三处用法全是「种子 → info hash」，`svc-hash` 已覆盖）、宿主第一次承载真干活的 provider | `Browse` / `PlanPlayback` / `GenerateThumbnails` / `ScanImportSource` 四大能力可用；`playback` 域能接上 |
| ⑦ | **`115_provider` 的 Rust 版**（251KB，**最后一关**） | 含 HLS 读取 / range reader / 加密 / 离线下载。**只有它完成，Python 运行时才能从部署里删掉**。只影响网盘用户，故最后 | `deployment.md` §五 的判据清单**全部勾上** |

### 8.3 与后端域的并行关系

| 域 | 剩余 | 卡在什么 |
|---|---|---|
| `transfers` | 10 文件 / 27 `todo!()` | **几乎全卡插件 ABI** → 依赖 ① ② ⑥ |
| `catalog` | 9 文件 / 33 | 与 ③ 同域，可一起推 |
| `playback` | 16 文件 | 卡 provider（⑥）/ PyAV（⑤）/ zip |
| `system` | 4（`telemetry` 2 / `plugin_removal` 2） | `telemetry` 待拍板（§7.5 第 1 条） |
| `discovery` | 3 | `moment_recommendation` 2 / `image_search_space` 1 |

### 8.4 我的建议：先做 ① ③ ②，④ 排在 ⑥ 之前

- **① 不是「一次决策」而是一次修复** —— 现在 2 个插件就带着「能编译但跑不通」的
  风险（`GenerateThumbnails` 两侧消息类型不同），**而且没有任何测试会告诉你**。
  修完顺带补上防复发的跨仓集成测试。
- **③ 是「让前面所有工作不白干」的那一块**，而且**可测**（一条真库端到端）。
- **② 有现成样板、风险最低**，适合与 ③ 交替推进。
- **④ 必须在 ⑥ 之前**：`local_provider` 是第一个真会用到 `local_path` 的插件。
- **⑥ 不要提前做**：它 172KB，而它最大的价值（扔掉 `libtorrent`）要等 ③ 与
  `playback` 接上才体现；先用小插件把协议税缴完，大插件才不吃亏。
