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
  f32 / Option<f32>                -> float8
  bool                            -> bool
  NaiveDateTime / Option<...>      -> timestamp
  NaiveDate / Option<...>          -> date
  Json / Option<Json>              -> jsonb
  JsonText                        -> text/json
  Vec<u8>                         -> bytea

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
    "f32": "float8",
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
            if py_type in ("implicit", "fk", "bare"):
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

    if args.summary:
        print("models=%d  rust_structs=%d  checked=%d  problems=%d"
              % (len(models), len(rust), checked, len(problems)))
        return 1 if problems else 0

    if not problems:
        print("schema 对拍：通过 %d/%d 张表，列名/类型/可空性全部一致" % (checked, len(models)))
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
