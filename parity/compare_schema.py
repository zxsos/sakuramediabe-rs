#!/usr/bin/env python3
"""对拍 Rust 结构体与 Peewee 模型的列集合。

这是「不连库也能发现漂移」的工具：40 张表的 Rust 映射是手写的，
一次疏忽就会让某个字段永远读不到默认值。逐字段比对能在提交前抓住。

比对维度：
  1. 表名是否都能对上（按结构体注释里的 `table = "xxx"` 或结构体名反推）
  2. 列名集合是否完全一致（缺失 / 多余 / 拼写漂移）
  3. 可空性是否一致（Rust 用 Option<> 表达 nullable）

类型映射（Rust -> Peewee 规范化类型）：
  String / Option<String>          -> text
  i32 / Option<i32>                -> int4
  i64 / Option<i64>                -> int8
  f64 / Option<f64>                -> float8
  bool                            -> bool
  NaiveDateTime / Option<...>      -> timestamp
  NaiveDate / Option<...>          -> date
  Json / Option<Json>              -> jsonb
  JsonText                        -> text/json
  Vec<u8>                         -> bytea

注意 f32 刻意不在映射表里：Peewee 的 FloatField 一律是 double precision
(float8)，对应 Rust 的 f64。f32 是 4 字节，PostgreSQL 里对应 real(float4)，
上游没有任何一列用到它。把 f32 也映射成 float8 会让对拍通过，但 sqlx 的
`f32: Decode` 走的是 decode_float4，运行时读 float8 列会报类型不匹配 ——
那正是「对拍通过但集成测试失败」的静默缺陷。

用法：
    python compare_schema.py            # 打印全部差异
    python compare_schema.py --summary  # 只打印统计
"""

import argparse
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from schema_contract import UNKNOWN_NULLABLE, collect  # noqa: E402

# 优先级：显式环境变量 > 工作区内的 crate > 作者本机的原始路径。
# 缺省路径指向 Windows 时，换机器跑会得到「40 处 MISSING_STRUCT」——
# 那不是真的缺结构体，只是没找到源码。
_DEFAULT_RUST_ROOT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "crates",
    "sm-db",
    "src",
)
RUST_ROOT = os.environ.get(
    "SM_DB_SRC",
    _DEFAULT_RUST_ROOT
    if os.path.isdir(_DEFAULT_RUST_ROOT)
    else r"C:\Users\29789\Desktop\sakuramedia\sakuramedia-rs\crates\sm-db\src",
)

# Rust 类型 -> Peewee 规范化类型。Option<> 视为同类型（可空性另行比对）。
TYPE_MAP = {
    "String": "text",
    "i32": "int4",
    "i64": "int8",
    "f64": "float8",
    "bool": "bool",
    "NaiveDateTime": "timestamp",
    "NaiveDate": "date",
    "DateTime<Utc>": "timestamptz",
    "Json": "jsonb",
    "JsonText": "text/json",
    "Vec<u8>": "bytea",
    "Uuid": "uuid",
    # 带限定名的写法。表模型用类型别名（`Json`、`NaiveDateTime`），
    # 而仓储层的插入 DTO 直接写全名（`serde_json::Value`、
    # `chrono::NaiveDateTime`）—— 同一个类型，两种拼法。
    #
    # 不认这两种拼法的话，插入 DTO 的字段会全部报 UNKNOWN_TYPE，而
    # 那不是类型错误，只是同一个类型的别名。
    "serde_json::Value": "jsonb",
    "chrono::NaiveDateTime": "timestamp",
    "chrono::NaiveDate": "date",
}

STRUCT_RE = re.compile(r"pub struct (\w+)\s*\{(.*?)\n\}", re.S)
FIELD_RE = re.compile(r"^\s*pub (\w+)\s*:\s*([^,]+),\s*$", re.M)
TABLE_RE = re.compile(r'table\s*=\s*"([^"]+)"')
# 宏生成 / 宏声明的类型：从 `declare!(Type, "table", "col", ...)` 取列名。
DECLARE_RE = re.compile(
    r"declare!\(\s*([\w:]+)\s*,\s*\"([\w]+)\"\s*,(.*?)\n    \);", re.S
)
STR_RE = re.compile(r'"([^"]+)"')

# 豁免名单：Rust 侧存在、但**不是**上游 Peewee 表镜像的结构体。
#
# 加入条件（必须同时满足，否则不许豁免）：
#   1. 不参与任何 SQL —— 没有 #[derive(FromRow)]，不被 query_as! 使用；
#   2. 不是数据库表的列集合（仓储、请求/响应 DTO、测试夹具、纯值对象）。
#
# 名单之外的每个未检查 struct 都会让对拍失败。这是刻意的：新增一个
# struct 却忘了给它 Python 对应物时，应该被问到，而不是安静地跳过。
#
# **条件 1 是硬门槛，投影行不在豁免范围内。** 一条聚合查询的返回行
# （`SELECT id, MAX(level) ... GROUP BY id`）带 FromRow、被 query_as
# 使用，却不是任何表的镜像 —— 它没有上游 Peewee 模型可以对照。给它豁免
# 等于在这道门禁上开一个口子，而门禁的价值恰恰是抓住「新增结构体却忘了
# 对拍」。那类结构体应当改用元组返回（见
# `sm_db::repo::movie::MovieResolutionLevelRow`），具名类型放到 sm-service。
#
# 下面这些都满足上述两条：它们是本仓库自己造的访问层与数据结构，
# 上游 Python 侧按定义就不存在对应模型。
UNCHECKED_STRUCT_EXEMPT = frozenset(
    {
        # 纯值对象：影片列表的筛选条件，只作为参数传给
        # `list_movie_card_ids` / `count_movies`；没有 FromRow、不映射任何表。
        # 单独立它是因为 15 个可选筛选位塞进位置元组无法阅读。
        "MovieListFilter",
        # 同上：下载台账列表的筛选条件（上游 `list_tasks` 的三个可选筛选位，
        # `task_service.py:43-72`）。与 `MovieListFilter` 是同一个形状 ——
        # 纯值对象，没有 FromRow、不映射任何表。
        "DownloadTaskFilter",
        # 仓储与网关：持有 PgPool，不映射任何表
        "MovieRepository",
        "MovieSeriesRepository",
        "MediaRepository",
        "DownloadTaskRepository",
        "MovieOwnershipGateway",
        # 同上：演员的字段主权网关，持有 PgPool，不映射任何表。
        # 与 `MovieOwnershipGateway` 是**两个实体一套形状**，漏登记一个就会
        # 在这里报 `UNCHECKED_STRUCT` —— 但那不是真的漏了对拍，是这个表的
        # 豁免清单没跟上。
        "ActorOwnershipGateway",
        # 插入 DTO：只列出调用方需要显式提供的列，是子集而非全表
        "NewMovie",
        "NewMedia",
        "NewDownloadTask",
        # 纯值对象：字段主权补丁与护栏，不落库
        "FieldPatch",
        "FieldGuard",
        # 聚合值对象：一部影片的「演员 id 列表 + 标签 id 列表」，用于构造稀疏向量。
        #
        # 满足豁免条件 1：**没有 `#[derive(FromRow)]`、不是 `query_as` 的目标**。
        # 它是 `MovieFeatureRepository::features_for_movies` 用两次元组查询
        # （`SELECT movie, actor` / `SELECT movie, tag`）在内存里拼出来的累加器 ——
        # 不是任何表的列集合（`movie_features` 这张表在两侧 DDL 里都不存在，
        # 上游也没有对应 Peewee 模型）。
        "MovieFeatures",
        # 纯值对象：领取范围（并发道），只作为 bind 参数传给
        # `claim_in`，没有 FromRow、不参与 query_as
        "TaskLanes",
        # 纯值对象：片段列表的筛选条件，只作为参数传给 `list_filtered`；
        # 没有 FromRow、不映射任何表
        "ClipFilter",
        # 纯值对象：演员列表的筛选条件 / 排序描述 / 动态 SET 构造器。
        # 与上面的 `ClipFilter` 同一形状 —— 全部只作为参数传给
        # `sm_db::repo::actor` 的方法，没有 FromRow、不参与 query_as、
        # 不是任何表的列集合（`ActorListFilter` 是筛选条件子集，
        # `ActorScope` 只有性别与订阅两个维度，`ActorUpdate` 是待写列的
        # 构造器，列名来自 `WRITABLE_ACTOR_COLUMNS` 白名单）。
        "ActorListFilter",
        "ActorScope",
        "ActorUpdate",
        # 仓储与网关：持有 PgPool / 会话，不映射任何表
        "StatsRepository",
        # 纯值对象：会话级 advisory lock 守卫，持有 PoolConnection
        # 而不映射任何表；无 FromRow、不参与 query_as
        "AdvisoryLock",
        # 集成测试夹具
        "TestDb",
        # P0 批次的访问层与数据结构（user.rs / task.rs）
        "UserRepository",
        "UserRefreshTokenRepository",
        "BackgroundTaskRunRepository",
        "NewUser",
        "NewRefreshToken",
        "NewTaskRun",
        # 轮换结果：包两行数据库记录，不映射任何表
        "Rotation",
        # 领取结果 / 进度 / 结果：方法参数与返回值
        "ClaimedTask",
        "TaskProgress",
        "TaskOutcome",
        # Media 族批次（playback.rs）
        "MediaThumbnailRepository",
        "MediaProgressRepository",
        "MediaPointRepository",
        "MediaClipRepository",
        "NewMediaClip",
        # 分页值对象：校验过的请求参数，不是任何表的列集合
        "PageRequest",
        # 事务编排层：执行上下文与用例结果，不映射任何表
        "Ctx",
        "CtxConnection",
        "UnitOfWork",
        "GeneratedThumbnail",
        "ImportedMovie",
        # asset 批次（asset.rs）
        "TagRepository",
        "MovieActorRepository",
        "MovieTagRepository",
        # actor 批次（actor.rs）
        "ActorRepository",
        "NewActor",
        "SyncState",
        # library 批次（library.rs）
        "MediaLibraryRepository",
        "NewMediaLibrary",
        # collection 批次（collection.rs）：宏生成，三个父表
        "PlaylistRepository",
        "MomentCollectionRepository",
        "ClipCollectionRepository",
        "NewCollection",
        "PlaylistMovieRepository",
        "ImageRepository",
        "NewImage",
        "VideoItemRepository",
        "NewVideoItem",
        "VideoCollectionRepository",
        "NewVideoCollection",
        "RankingItemRepository",
        "NewRankingItem",
        "ImageSearchSessionRepository",
        "NewImageSearchSession",
        "ImageSearchIndexStateRepository",
        "SubtitleRepository",
        "NewSubtitle",
        "MoviePlotImageRepository",
        "SystemNotificationRepository",
        "NewNotification",
        "SchemaMigrationRepository",
        "DailyRecommendationItemRepository",
        "NewDailyRecommendation",
        "MomentRecommendationRepository",
        "NewMomentRecommendation",
        # `UnitOfWork::record_thumbnail_artifacts` 的入参：一件待登记的缩略图
        # 产物（origin + 偏移）。**纯入参 DTO** —— 不带 `FromRow`、不被任何
        # `query_as` 使用，满足豁免条件 1。
        #
        # 它没有对应的上游 Peewee 模型，因为上游那里传的是 `(ThumbnailArtifact,
        # Path)` 元组、字段散在代码里；这里收成一个具名类型只是为了不被位置参数
        # 错位坑到（位置元组错位是**静默**的）。
        "ThumbnailArtifactRecord",
        # discovery / recommendation / moment 三个仓储文件的仓储类型。
        #
        # 它们与上面那批 Repository 是同一个形状：**持 `PgPool`，本身不映射
        # 任何表**（没有 `#[derive(FromRow)]`、不是 `query_as` 的目标），
        # 满足豁免条件 1。此前漏登记，于是对拍报 `UNCHECKED_STRUCT` ——
        # 那是登记遗漏，不是「门禁抓到了问题」。
        #
        # ⚠️ 同文件里的**投影行**（`CandidateRow` / `PendingThumbnail` /
        # `MovieFeatures` …）不在豁免范围：它们带 `FromRow`、被 `query_as`
        # 使用。按 `sm-db/src/repo/movie.rs` 的既定选法，那类要改成
        # `pub type XRow = (...)` 元组别名，具名类型放 `sm-service`。
        "HotActressReleaseRepository",
        "MomentSeedRepository",
        "PendingImageRepository",
        "MovieFeatureRepository",
        "MomentCollectionItemRepository",
        "ClipCollectionItemRepository",
        # transfer 批次（transfer.rs）
        "DownloadClientRepository",
        "IndexerRepository",
        "IndexerDownloadClientRepository",
        "DownloadResourceBlacklistRepository",
        "NewDownloadClient",
        "NewIndexer",
        # submission 批次（submission.rs）
        "DownloadSubmissionRepository",
        # 插入 DTO：download_submission_record 的列子集
        "NewSubmissionRecord",
    }
)

# 插入 DTO -> 它写入的那张表。
#
# 豁免这些 DTO 的理由是「它们是表列的子集」，所以不能要求字段全集。
# 但**字段类型与可空性必须对得上**：每个字段最终会被绑进一条 INSERT，
# 类型不匹配会在运行时才报，而 NOT NULL 列被绑 NULL 是必现失败。
#
# 之前类型检查和「不是表镜像」一起被免掉了，于是 NewMovie 里藏着
# `series_id: Option<i64>`（列是 integer）等七处缺陷，对拍报 0 problems。
#
# 新增插入 DTO 时必须在这里登记，否则它的字段类型无人校验。
INSERT_DTO_TARGETS = {
    "NewMovie": "Movie",
    "NewMedia": "Media",
    "NewActor": "Actor",
    "NewMediaLibrary": "MediaLibrary",
    "NewTaskRun": "BackgroundTaskRun",
    "NewUser": "User",
    "NewRefreshToken": "UserRefreshToken",
    "NewDownloadTask": "DownloadTask",
    "NewMediaClip": "MediaClip",
    "NewSubmissionRecord": "DownloadSubmissionRecord",
    "NewDownloadClient": "DownloadClient",
    "NewIndexer": "Indexer",
}


def strip_option(rust_type: str):
    t = rust_type.strip()
    optional = False
    while t.startswith("Option<") and t.endswith(">"):
        optional = True
        t = t[len("Option<"): -1].strip()
    return t, optional


def normalize(rust_type: str):
    t, optional = strip_option(rust_type)
    base = TYPE_MAP.get(t)
    if base is None:
        return None, optional, t
    return base, optional, t


def types_compatible(py_type: str, rust_base: str) -> bool:
    """判断 Python 侧列类型与 Rust 侧映射是否兼容。

    `JsonTextField` 落的是 TEXT 列（只是内容是 JSON 文本），所以 Rust 的
    `Option<String>` 是正确映射 —— 对拍时必须把 `text/json` 与 `text`
    视为同一类，否则 13 处 JsonTextField 会全部误报。
    """
    if py_type == rust_base:
        return True
    if py_type == "text/json" and rust_base == "text":
        return True
    # 插入 DTO 侧用 `serde_json::Value`（映射到 jsonb）比表模型侧的
    # `String` 更精确：那一列存的是 JSON 文本，用结构化值表达入参，
    # 由仓储负责序列化。同一个列，两种正确映射。
    if py_type == "text/json" and rust_base == "jsonb":
        return True
    # int4 列用 i64 读取在 PostgreSQL 上是安全的（只要值不越界），
    # 但仍作为差异报出，避免类型漂移无声积累。
    return False


def parse_rust() -> dict:
    """返回 {struct_name: {"table": str|None, "fields": [(name, type)]}}。"""
    out = {}
    for dirpath, _dirs, files in os.walk(RUST_ROOT):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            with open(path, "r", encoding="utf-8") as fh:
                text = fh.read()
            for m in STRUCT_RE.finditer(text):
                sname, body = m.group(1), m.group(2)
                fields = [(n, t) for n, t in FIELD_RE.findall(body)]
                start = text.rfind("///", 0, m.start())
                window = text[max(0, start - 800): m.start()] if start > 0 else text[max(0, m.start() - 800): m.start()]
                tm = TABLE_RE.search(window)
                out.setdefault(sname, {"table": None, "fields": []})
                if tm:
                    out[sname]["table"] = tm.group(1)
                if len(out[sname]["fields"]) < len(fields):
                    out[sname]["fields"] = fields

            # 宏生成 / 宏声明的类型没有可见的 `pub struct`，只能靠显式
            # 导出的清单参与对拍。少了这一步，三张合集表会游离在检查之外。
            #
            # 但清单里**只有列名，没有类型** —— 所以这些列的名字参与检查，
            # 类型不参与。下面用 COLUMNS_CONST 标记，判定处会跳过。
            #
            # 这不是「类型由别处保证」，是一个真实的盲区：`ordered_collection_item!`
            # 曾把三个外键列声明成 i64（DDL 是 integer），对拍报 0 problems
            # 整整几轮，因为这里没有类型可比。
            for dm in DECLARE_RE.finditer(text):
                sname = dm.group(1).split("::")[-1]
                table = dm.group(2)
                cols = STR_RE.findall(dm.group(3))
                if not cols:
                    continue
                out.setdefault(sname, {"table": table, "fields": []})
                out[sname]["table"] = table
                if not out[sname]["fields"]:
                    out[sname]["fields"] = [(c, "COLUMNS_CONST") for c in cols]
    return out


def read_ddl_primary_keys(path: str) -> dict:
    """{表名: {主键列名}} —— 从**生成的** DDL 里读。

    读的是 `docker/schema.sql`（gen_ddl.py 的产物），不是上游源码。
    比对「契约 vs DDL」能抓住两类问题：解析器漏了 kwarg（契约本身就错），
    以及生成器写错了（契约对、DDL 错）。只比「上游 vs DDL」则只能抓后者。

    主键既可能内联在列定义里（`id integer PRIMARY KEY`），也可能是独立的
    表级约束（`ALTER TABLE ... ADD CONSTRAINT ... PRIMARY KEY (...)`）。
    两种都要认 —— 只认内联的那种，会在生成器改用表级约束时全表误报。
    """
    out: dict = {}
    try:
        with open(path, "r", encoding="utf-8") as fh:
            ddl = fh.read()
    except OSError:
        return out

    for m in re.finditer(
        r"CREATE TABLE\s+(?:IF NOT EXISTS\s+)?(\w+)\s*\((.*?)\n\);", ddl, re.S
    ):
        table, body = m.group(1), m.group(2)
        pks = set()
        for line in body.split("\n"):
            line = line.strip().rstrip(",")
            # 内联：`id integer PRIMARY KEY DEFAULT 1`
            inline = re.match(r'"?(\w+)"?\s+\S+.*\bPRIMARY KEY\b', line)
            if inline:
                pks.add(inline.group(1))
            # 表级：`CONSTRAINT x PRIMARY KEY (a, b)`
            composite = re.match(
                r'CONSTRAINT\s+\S+\s+PRIMARY KEY\s*\(([^)]*)\)', line, re.I
            )
            if composite:
                for part in composite.group(1).split(","):
                    name = part.strip().strip('"')
                    if name:
                        pks.add(name)
        out[table] = pks

    for m in re.finditer(
        r"ALTER TABLE\s+(\w+)\s+ADD CONSTRAINT\s+\S+\s+PRIMARY KEY\s*\(([^)]*)\)",
        ddl,
        re.I,
    ):
        table = m.group(1)
        pks = out.setdefault(table, set())
        for part in m.group(2).split(","):
            name = part.strip().strip('"')
            if name:
                pks.add(name)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--summary", action="store_true")
    args = ap.parse_args()

    ddl_path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "docker",
        "schema.sql",
    )
    ddl_primary_keys = read_ddl_primary_keys(ddl_path)

    models = collect()
    rust = parse_rust()

    problems = []
    checked = 0
    # 用单元素 list 当可变计数器（Python 3 的 nonlocal 在闭包里不便于
    # 从 summarize 读取）。它只用于 summary 输出，不影响判定。
    fk_checked = [0]

    for m in models:
        sname = m["struct"]
        r = rust.get(sname)
        if r is None:
            problems.append(("MISSING_STRUCT", m["table"], sname, "Rust 侧没有这个结构体"))
            continue

        checked += 1
        rust_fields = {n: t for n, t in r["fields"]}
        py_cols = m["columns"]

        # ---- 主键：契约声明 vs 实际 DDL ----
        #
        # 这一段是被一个真实缺陷逼出来的：
        # `image_search_index_state` 上游写的是
        # `id = peewee.IntegerField(primary_key=True, default=1)`，而
        # `schema_contract.py` 的 kwargs 白名单里**没有** `primary_key`，
        # 于是它被静默忽略 —— 契约说「不是主键」，`gen_ddl.py` 就照着
        # 生成了没有 PRIMARY KEY 的表。
        #
        # 后果不只是 DDL 少一句。那张表成了全库唯一一张既无主键也无唯一
        # 约束的表，于是 `ON CONFLICT (id)` 报
        # `42P10 there is no unique or exclusion constraint matching the
        # ON CONFLICT specification`，而「单例」这个约定在数据库层面
        # **没有任何保障** —— 插两行不会有任何东西阻止。
        #
        # 为什么此前对拍没抓到：**compare_schema.py 从来不比较主键**。
        # 它比可空性、比类型、比外键，唯独不比「哪些列是主键」。
        #
        # 这里比的是**契约 vs 生成的 DDL**，而不是「上游 vs DDL」——
        # 前者能抓住「解析器漏了 kwarg」，后者只能抓住「生成器写错了」。
        # 解析器漏 kwarg 这个形状本仓库已经出现四次（外键列被 continue
        # 跳过、只遍历 Python 模型、`is False` 让 None 免检、现在是
        # primary_key 不在白名单），每次都是「检查器在无法判断时沉默」。
        ddl_pks = ddl_primary_keys.get(m["table"], set())
        contract_pks = {
            name for name, spec in py_cols.items() if spec.get("primary_key")
        }
        if contract_pks != ddl_pks:
            problems.append(
                (
                    "PRIMARY_KEY_MISMATCH",
                    m["table"],
                    ",".join(sorted(contract_pks)) or "-",
                    "契约声明主键 %s，DDL 里是 %s"
                    % (
                        ",".join(sorted(contract_pks)) or "（无）",
                        ",".join(sorted(ddl_pks)) or "（无）",
                    ),
                )
            )

        for col in py_cols:
            if col not in rust_fields:
                problems.append(("MISSING_FIELD", m["table"], col, "Python 有，Rust 没有"))
                continue
            py_type = py_cols[col]["type"]
            if py_type == "fk":
                # 外键列**不是没有类型**，它的类型就是被引用列的类型。
                #
                # 此前这里直接 continue，于是 `media_thumbnail.media_id: i64`
                # 长期「通过」对拍，而 DDL 里那一列是 integer（gen_ddl.py
                # 跟随被引用表的 id 类型）。读列时 sqlx 会把 int4 解码进
                # i64 而失败 —— 与之前 f32 缺陷完全同类，只是发生在集成
                # 测试而不是对拍阶段。
                #
                # 现在比对该 fk 指向的列在 Python 侧的规范化类型。
                #
                # 两个键名细节：
                #  - 是 `ref_table` 而非 `ref_model`：`collect()` 的第二遍
                #    会把类名解析成表名后 `pop("ref_model")`。
                #  - `ref_field` 为空是常态：Peewee 的
                #    `ForeignKeyField(Model)` 不写 `field=` 时默认指向
                #    `Model.id`，所以要自己补上这个默认值。
                ref = py_cols[col].get("ref_table")
                if not ref:
                    continue
                target = next((x for x in models if x["table"] == ref), None)
                if target is None:
                    continue
                if rust_fields[col] == "COLUMNS_CONST":
                    # 宏生成类型只声明了列名，没有类型信息 —— 与非 fk
                    # 分支同样的理由无法判定，跳过。
                    continue
                ref_field = py_cols[col].get("ref_field") or "id"
                ref_col = target["columns"].get(ref_field)
                if not ref_col or ref_col["type"] in ("implicit", "fk", "bare"):
                    continue
                base, _, raw = normalize(rust_fields[col])
                if base is None:
                    problems.append(
                        ("UNKNOWN_TYPE", m["table"], col, "Rust 类型 %s 未在映射表中" % raw)
                    )
                else:
                    # 记账：这个数字一旦归零，说明上面的规则被改坏或
                    # 解析路径失效了，而没有任何 problem 会暴露出来。
                    fk_checked[0] += 1
                if base is not None and not types_compatible(ref_col["type"], base):
                    problems.append(
                        (
                            "FK_TYPE_MISMATCH",
                            m["table"],
                            col,
                            "引用 %s.%s(Python=%s) 但 Rust=%s"
                            % (ref, ref_field, ref_col["type"], raw),
                        )
                    )
                # fk 列的可空性同样要查。此前这个分支以 `continue` 收尾，
                # 于是所有外键列都**绕过**了下面的可空性判定 ——
                # 上一轮补上 fk 类型一致性时只修了类型，漏了这一半。
                #
                # RatingItem.movie_id 就是漏网的：Python 侧 NOT NULL，
                # Rust 侧是 `Option<i32>`，而对拍报 0 problems。
                #
                # 判定条件与非 fk 分支一致，但**不能**用 `text/json` 豁免：
                # 外键列不会是 JSON 文本。
                fk_nullable = py_cols[col].get("nullable")
                fk_optional = normalize(rust_fields[col])[1]
                if fk_nullable is UNKNOWN_NULLABLE:
                    problems.append(
                        (
                            "NULLABILITY_UNKNOWN",
                            m["table"],
                            col,
                            "Python 侧可空性未能解析，需查清上游声明",
                        )
                    )
                elif fk_nullable is False and fk_optional:
                    problems.append(
                        (
                            "NULLABILITY",
                            m["table"],
                            col,
                            "Python NOT NULL，Rust 却是 Option<>（外键列）",
                        )
                    )
                continue
            if py_type in ("implicit", "bare"):
                continue
            if rust_fields[col] == "COLUMNS_CONST":
                # 宏生成类型只声明了列名，没有类型信息；类型正确性由
                # 编译与 collections 模块自身的单元测试保证。
                continue
            base, optional, raw = normalize(rust_fields[col])
            if base is None:
                problems.append(("UNKNOWN_TYPE", m["table"], col, "Rust 类型 %s 未在映射表中" % raw))
                continue
            if not types_compatible(py_type, base):
                problems.append(
                    ("TYPE_MISMATCH", m["table"], col, "Python=%s Rust=%s(%s)" % (py_type, base, raw))
                )
            py_nullable = py_cols[col].get("nullable")
            if py_nullable is UNKNOWN_NULLABLE:
                # 解析不出可空性 —— 报错，不猜。
                #
                # 这里是第三个「不知道被当成没问题」的洞，形状与前两个完全
                # 一致：外键列被 continue 跳过、只遍历 Python 模型导致 Rust
                # 多出的 struct 无人检查、现在是 `is False` 让 None 免检。
                #
                # 三处的共同点都是「检查器在无法判断时选择了沉默」，而每一处
                # 都藏着一个真实缺陷：37 个外键类型错误、11 个未验证 struct、
                # 以及 actor.javdb_id 被声明成 Option<String> 而 DDL 是 NOT
                # NULL —— 插入必然失败，对拍却报 0 problems。
                problems.append(
                    (
                        "NULLABILITY_UNKNOWN",
                        m["table"],
                        col,
                        "Python 侧可空性未能解析，需查清上游声明",
                    )
                )
            elif py_nullable is False and optional and py_type != "text/json":
                # `text/json`（上游的 `JsonTextField`）排除在外，理由与
                # `types_compatible` 里的既有立场一致：那一列落的是 TEXT，
                # 只是内容是 JSON 文本，Rust 用 `Option<String>` 表达
                # 「调用方没提供这个 JSON 字段」是正确的类型映射。
                #
                # 但上游自己有个陷阱：`JsonTextField.db_value` 遇到 None
                # 会返回 None，而列是 NOT NULL —— 也就是说传 None 会违反
                # 约束。对应的责任落在仓储层把它们兜成 DEFAULT 值
                # （task.rs 的 `result_summary`、media.rs 的 `storage_ref`
                # 都是这么做的），而不是让模型放弃 `Option`。
                #
                # 这里豁免的是**可空性**，不是类型：类型仍由
                # `types_compatible` 的 `text/json` 对 `text` 规则管着。
                problems.append(
                    ("NULLABILITY", m["table"], col, "Python NOT NULL，Rust 却是 Option<>")
                )

        for fname in rust_fields:
            if fname not in py_cols:
                problems.append(("EXTRA_FIELD", m["table"], fname, "Rust 有，Python 没有"))

    # 反向检查：Rust 里多出来的结构体，**一个都没被上面验证过**。
    #
    # 主循环遍历的是 Python 模型，所以任何只有 Rust 侧存在的 struct
    # 都被静默跳过。此前一直报 40/40 全通过，而 rust_structs=51 ——
    # 11 个 struct 从未与任何东西比对过，包括 media_thumbnail 的
    # `media_id: i64`（DDL 是 integer），那正是 f32 缺陷的同一类问题：
    # 类型错了要等到运行时读列才报。
    #
    # 这里不试图给它们补 Python 模型（那需要上游确实有对应表），只保证
    # 差异**可见**：每个未被检查的 struct 都会列出来，附带一个显式的
    # 豁免名单，名单之外的必须处理。
    #
    # 豁免的判断依据是「它不是 Peewee 表的镜像」：
    #   - 纯值对象 / 枚举包装（没有 #[FromRow]，不参与任何 SQL）
    #   - 请求/响应 DTO（由 sm-api 拥有，不落库）
    checked_structs = {m["struct"] for m in models}
    unchecked = sorted(set(rust) - checked_structs)
    unexempt = [s for s in unchecked if s not in UNCHECKED_STRUCT_EXEMPT]
    for s in unexempt:
        r = rust[s]
        problems.append(
            (
                "UNCHECKED_STRUCT",
                r["table"] or s,
                s,
                "Rust 侧结构体没有对应的 Python 模型，列与类型均未验证",
            )
        )

    # 豁免的是「不是表镜像」这件事，不是「字段类型可以不查」。
    #
    # 插入 DTO（`NewMovie` 等）是表列的**子集**，所以不能要求字段全集 ——
    # 那是豁免的正当理由。但它每个字段最终会被绑进某条 INSERT，所以类型
    # 必须与目标列一致。
    #
    # 之前两者一起被免掉了，代价是 NewMovie 里藏着三处真实缺陷：
    # `series_id` / `cover_image_id` / `thin_cover_image_id` 是 `i64`，
    # 而 `movie.series_id` 等列是 `integer`（模型层早已是 i32）；
    # `summary` / `duration_minutes` / `score` / `score_number` 声明成
    # `Option`，而那些列是 NOT NULL。任何一次带 None 的 insert 都会失败。
    #
    # 这一层只比对「DTO 里确实存在的字段」，缺失的列不报。
    dto_checked = 0
    for dto, target in INSERT_DTO_TARGETS.items():
        d = rust.get(dto)
        t = next((x for x in models if x["struct"] == target), None)
        if d is None or t is None:
            problems.append(
                (
                    "DTO_TARGET_UNKNOWN",
                    target,
                    dto,
                    "插入 DTO 或其目标表无法解析，无法校验字段类型",
                )
            )
            continue
        table_cols = t["columns"]
        for field, raw in d["fields"]:
            if field not in table_cols:
                # DTO 里的非列字段（派生值、关联 id）不在校验范围。
                continue
            col = table_cols[field]
            dto_checked += 1
            base, optional, _ = normalize(raw)
            if base is None:
                problems.append(
                    ("UNKNOWN_TYPE", t["table"], field, "%s 的 %s 未在映射表中" % (dto, raw))
                )
                continue
            py_type = col["type"]
            if py_type == "fk":
                ref = col.get("ref_table")
                ref_field = col.get("ref_field") or "id"
                ref_model = next((x for x in models if x["table"] == ref), None)
                ref_col = (
                    ref_model["columns"].get(ref_field) if ref_model else None
                )
                py_type = (
                    ref_col["type"]
                    if ref_col and ref_col["type"] not in ("implicit", "fk", "bare")
                    else None
                )
            if py_type and not types_compatible(py_type, base):
                problems.append(
                    (
                        "DTO_FIELD_TYPE",
                        t["table"],
                        field,
                        "%s 声明 %s，但该列是 %s" % (dto, raw, py_type),
                    )
                )
            elif col.get("nullable") is False and optional and col["type"] != "text/json":
                problems.append(
                    (
                        "DTO_FIELD_NULLABILITY",
                        t["table"],
                        field,
                        "%s 声明 %s，但该列 NOT NULL" % (dto, raw),
                    )
                )

    if args.summary:
        print(
            "models=%d  rust_structs=%d  checked=%d  unchecked=%d (exempt %d)  "
            "fk_cols_checked=%d  dto_fields_checked=%d  problems=%d"
            % (
                len(models),
                len(rust),
                checked,
                len(unchecked),
                len(unchecked) - len(unexempt),
                fk_checked[0],
                dto_checked,
                len(problems),
            )
        )
        return 1 if problems else 0

    if not problems:
        print(
            "schema 对拍：通过 %d/%d 张表，列名/类型/可空性全部一致"
            "（含 %d 个外键列与被引用列的类型一致性）" % (checked, len(models), fk_checked[0])
        )
        return 0

    buckets = {}
    for kind, table, col, detail in problems:
        buckets.setdefault(kind, []).append((table, col, detail))

    print("schema 对拍：%d 张表中有 %d 处差异\n" % (len(models), len(problems)))
    for kind in sorted(buckets):
        rows = buckets[kind]
        print("[%s] %d 处" % (kind, len(rows)))
        for table, col, detail in rows:
            print("  %-32s %-28s %s" % (table, col, detail))
        print()
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
