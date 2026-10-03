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
from schema_contract import collect  # noqa: E402

RUST_ROOT = os.environ.get(
    "SM_DB_SRC",
    r"C:\Users\29789\Desktop\sakuramedia\sakuramedia-rs\crates\sm-db\src",
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
# 下面 11 个都满足上述两条：它们是本仓库自己造的访问层与数据结构，
# 上游 Python 侧按定义就不存在对应模型。
UNCHECKED_STRUCT_EXEMPT = frozenset(
    {
        # 仓储与网关：持有 PgPool，不映射任何表
        "MovieRepository",
        "MovieSeriesRepository",
        "MediaRepository",
        "DownloadTaskRepository",
        "MovieOwnershipGateway",
        # 插入 DTO：只列出调用方需要显式提供的列，是子集而非全表
        "NewMovie",
        "NewMedia",
        "NewDownloadTask",
        # 纯值对象：字段主权补丁与护栏，不落库
        "FieldPatch",
        "FieldGuard",
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
    }
)


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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--summary", action="store_true")
    args = ap.parse_args()

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
            if py_nullable is False and optional:
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

    if args.summary:
        print(
            "models=%d  rust_structs=%d  checked=%d  unchecked=%d (exempt %d)  "
            "fk_cols_checked=%d  problems=%d"
            % (
                len(models),
                len(rust),
                checked,
                len(unchecked),
                len(unchecked) - len(unexempt),
                fk_checked[0],
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
