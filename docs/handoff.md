# 续做交接

给「下一次接着干」的人（或新会话里的 AI）看。按本文档开工，不需要重读整个会话历史。

## 一、当前状态

| 项 | 值 |
|---|---|
| 铺开阶段 | ✅ **已全部完成**（2026-10-05）。路由模块 32/32、端点路径 **117/117**、服务层 **104/113** 文件（缺的全是空 `__init__.py` 与一处刻意不落地） |
| 验证阶段 | ✅ **编译 + clippy + rustdoc 三门全绿**（2026-10-05）。`cargo check/clippy --workspace --all-targets --all-features -- -D warnings` 与 `RUSTDOCFLAGS=-D warnings cargo doc` 全部 exit 0；`cargo fmt --all` 已跑（原本 81 个文件不整齐） |
| 端点方法体 | **实测 `todo!()` 共 199 个**：`sm-service` **126** + `sm-api` **73**。`sm-db` 与 `sm-scheduler` **各 0 个**（原 247：… + 缩略图任务计数/重置 4 + `generate_pending_thumbnails` 1 已落地） |
| 已清零的文件 | `sm-db` 全部、`thumbnails/artifacts.rs`、`image_cleanup.rs`、`movie_asset_pack.rs`；`sm-api` 侧 media / videos / moment-collections / video-collections 四族全通 |
| 调度 | 19 个内建任务，cron **16/16 全注册**；worker **handler 3/19**（`activity_record_cleanup` / `image_search_index` / `movie_similarity_recompute`） |
| 门禁 | ✅ **六道全绿**（2026-10-05）：`fmt` / `doc -D warnings` / `clippy --all-targets --all-features -D warnings` / `compare_schema.py` / `compare.py` / `compare_core.py`（64/64）/ `check_paged_wrappers.py`。**只剩测试未跑**（按用户要求不主动跑套件） |
| 提交 | 24 个，全部已推 `cnb/main`。`origin`（GitHub）**未推**，一直只推 `cnb` |

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

**`cargo fmt --all` 不是「可选的美化」**：它跑之前有 **81 个文件**不符合
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

## 一之二、下一步：**继续接方法体（238 个 `todo!()`）**

验证阶段已收口。接下来的活只有一件：把 `todo!()` 换成真实实现。

| 位置 | `todo!()` | 备注 |
|---|---|---|
| `sm-service` | **150** | 业务规则主场 |
| `sm-api` | **88** | 大多是「取参数 → 调 service → 拼响应」的薄壳 |
| `sm-db` | **0** | ✅ 全部实现完 |
| `sm-scheduler` | **0** | ✅ |

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

### 下一批：`transfers` 与 `catalog` 两块

| 候选 | 备注 |
|---|---|
| `catalog/movie_subscription_search_state.rs`(7) | **纯 DB，无阻塞** —— 建议先做 |
| `catalog/movie_metadata_search.rs`(4) | 纯 DB |
| `transfers/download_client.rs`(9) + `download_common.rs`(8) | ⚠️ **被下载器插件 ABI 挡**（与上面同一个 registry）|
| `catalog/catalog_import.rs`(7) | 依赖 `metadata_source`（插件 ABI）+ `image_cleanup`（已就绪）|

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
| `download_tasks`(4) | `transfers/download_task.rs` 还剩 3 个 `todo` |
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

- ~~`GET /movies/{n}/subtitles`~~ —— **已解阻塞**（2026-10-05）。原判断「要读媒体
  文件系统（provider 族）」是**错的**：读字幕只读宿主自己的字幕目录，provider 参与
  的是「把字幕搬过来」那一步（写侧 `subtitle_asset.rs`）。读侧
  `movie_subtitle.rs` 已铺，其中两处不变量：10 MiB 上限**先 stat 再读**、
  路径逃逸校验要在 `canonicalize` 之后做（只查字符串前缀会被软链绕过）。
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

- ~~`scripts/run-tests.sh`（未跟踪）引用了不存在的 `scripts/test_targets.py`~~ —— **已失效**：工作区 0 个未跟踪文件，`scripts/run-tests.sh` 本身已不存在。
- `stash@{0}` 还在，内容**不是**上面那条 —— 是「契约层拆仓」那批（根 `Cargo.toml` 改 `git + tag = "v0.1.0"` 依赖、删 `crates/sm-plugin-api/` 与 `proto/`，10 文件 -2315 行）。契约 crate 本身已验证能编译，**三个引用方（`sm-plugins` / `sm-server` / `plugin-ref-local`）当时未验证**就存起来了。取出前先跑一遍那三个 crate 的 `cargo check`。
- 前端契约对拍还没做（前端已在 `upstream/sakuramedia`）。已知两处可能与前端不一致：分页响应多一个 `synced_at: null`；时间戳是 naive UTC 而上游是运行时本地时区。
- **占位用例**：`catalog/movie_asset_pack.rs` 的
  `a_movie_without_images_is_not_an_error` 目前只是**类型级的钉子**
  （`matches!(Ok(false))`）—— 因为 `rebuild_movie_asset_pack` 还是 `todo!()`，
  真调用要 DB 与图片目录。该函数落地后**必须换成真实调用**，
  否则这条用例会一直「绿着但什么都没验」。同类占位用例在做 parity 时一并排查。
