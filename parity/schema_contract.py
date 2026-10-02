#!/usr/bin/env python3
"""从 Peewee 模型源码提取数据库 schema 契约。

只用 `ast` 解析，不用正则 —— 正则会在跨行参数、嵌套调用上误判，
而这份契约是 40 张表的唯一权威来源，解析必须精确。

提取内容：
  * 表名（Meta.table_name，缺省时用 Peewee 的 snake_case 推导）
  * 列名与列类型
  * 可空性、默认值、唯一约束
  * 显式 Meta.indexes

用法：
    python schema_contract.py            # 打印 JSON 契约
    python schema_contract.py --tables   # 只打印表名与列数
"""

import argparse
import ast
import json
import os
import re
import sys

MODEL_ROOT = os.environ.get(
    "PEEWEE_ROOT",
    r"C:\Users\29789\Desktop\sakuramedia\sakuramediabe\src\model",
)

# Peewee 字段类型 -> 规范化类型名。
#
# 两个 JSON 变体必须区分：JsonTextField 落 TEXT，JsonbField 落 JSONB。
# Rust 侧前者映射 Option<String>，后者映射 serde_json::Value —— 混淆
# 会让 SQL 写入失败或读出类型不符。
FIELD_TYPES = {
    "CharField": "text",
    "CaseSensitiveCharField": "text",
    "TextField": "text",
    "IntegerField": "int4",
    "BigIntegerField": "int8",
    "FloatField": "float8",
    "DecimalField": "numeric",
    "BooleanField": "bool",
    "DateTimeField": "timestamp",
    "DateField": "date",
    "TimeField": "time",
    "UUIDField": "uuid",
    "JsonTextField": "text/json",
    "JsonbField": "jsonb",
    "ForeignKeyField": "fk",
    "BareField": "bare",
}

# 继承链带来隐式列的地方。
IMPLICIT_COLUMNS = {
    "BaseModel": ["id"],
    "TimestampedMixin": ["id", "created_at", "updated_at"],
}


def snake_case(name: str) -> str:
    """Peewee 的默认表名推导：类名转 snake_case。"""
    out = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    out = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", out)
    return out.lower()


def call_name(node) -> str:
    """取 `peewee.CharField(...)` 里的 `CharField`。"""
    if not isinstance(node, ast.Call):
        return ""
    func = node.func
    if isinstance(func, ast.Attribute):
        return func.attr
    if isinstance(func, ast.Name):
        return func.id
    return ""


def literal(node):
    """尽力求值字面量；求不出返回 None。"""
    try:
        return ast.literal_eval(node)
    except Exception:
        return None


def column_name_of(field_name: str, call: ast.Call) -> str:
    """实际列名：显式 `column_name` 优先，否则字段名本身。

    `column_name` 不是外键专有 —— `DownloadTask.movie` 就是
    `CharField(..., column_name="movie_number")`，列名与字段名不同。
    只在外键上读 column_name 会让对拍误报「多一个字段、少一个字段」。
    """
    for kw in call.keywords:
        if kw.arg == "column_name":
            value = literal(kw.value)
            if value:
                return value
    return field_name


def parse_model_file(path: str) -> list:
    with open(path, "r", encoding="utf-8") as fh:
        tree = ast.parse(fh.read(), filename=path)

    models = []
    for node in tree.body:
        if not isinstance(node, ast.ClassDef):
            continue
        bases = []
        for base in node.bases:
            if isinstance(base, ast.Name):
                bases.append(base.id)
            elif isinstance(base, ast.Attribute):
                bases.append(base.attr)

        if not any(b in ("BaseModel", "TimestampedMixin") for b in bases):
            continue
        # TimestampedMixin 自身只是 mixin，不落表。它继承 BaseModel 只是为了
        # 复用 save() 覆写，若不过滤会凭空多出一张 timestamped_mixin 表。
        if node.name == "TimestampedMixin":
            continue

        # 继承链带来的隐式列。
        columns = {}
        for base in bases:
            for col in IMPLICIT_COLUMNS.get(base, []):
                columns[col] = {"name": col, "type": "implicit", "from": base}

        table_name = None
        indexes = []
        for stmt in node.body:
            if isinstance(stmt, ast.Assign):
                fname = stmt.targets[0].id
                cname = call_name(stmt.value)
                if cname in FIELD_TYPES:
                    kind = FIELD_TYPES[cname]
                    actual = column_name_of(fname, stmt.value)
                    if kind == "fk" and actual == fname:
                        # 外键未显式指定列名时 Peewee 用 `<field>_id`
                        actual = "%s_id" % fname
                    entry = {
                        "name": actual,
                        "type": kind,
                        "field": fname,
                        "nullable": None,
                        "unique": False,
                        "index": False,
                        "default": None,
                    }
                    if kind == "fk":
                        for kw in stmt.value.keywords:
                            if kw.arg == "null":
                                entry["nullable"] = literal(kw.value)
                            if kw.arg == "unique":
                                entry["unique"] = bool(literal(kw.value))
                    else:
                        for kw in stmt.value.keywords:
                            if kw.arg == "null":
                                entry["nullable"] = literal(kw.value)
                            elif kw.arg == "unique":
                                entry["unique"] = bool(literal(kw.value))
                            elif kw.arg == "index":
                                entry["index"] = bool(literal(kw.value))
                            elif kw.arg == "default":
                                entry["default"] = literal(kw.value)
                    columns[actual] = entry
            elif isinstance(stmt, ast.ClassDef) and stmt.name == "Meta":
                for meta in stmt.body:
                    if not isinstance(meta, ast.Assign):
                        continue
                    mname = meta.targets[0].id
                    if mname == "table_name":
                        table_name = literal(meta.value)
                    elif mname == "indexes":
                        try:
                            indexes = ast.literal_eval(meta.value)
                        except Exception:
                            indexes = []

        models.append(
            {
                "table": table_name or snake_case(node.name),
                "struct": node.name,
                "bases": bases,
                "columns": columns,
                "indexes": indexes,
                "source": os.path.relpath(path, MODEL_ROOT),
            }
        )
    return models


def collect():
    models = []
    for dirpath, _dirnames, filenames in os.walk(MODEL_ROOT):
        for name in sorted(filenames):
            if not name.endswith(".py") or name == "__init__.py":
                continue
            models.extend(parse_model_file(os.path.join(dirpath, name)))
    models.sort(key=lambda m: m["table"])
    return models


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tables", action="store_true", help="只打印表名与列数")
    ap.add_argument("--out", help="把 JSON 契约写到该路径")
    args = ap.parse_args()

    if not os.path.isdir(MODEL_ROOT):
        print("model root not found: %s" % MODEL_ROOT, file=sys.stderr)
        print("set PEEWEE_ROOT to the backend's src/model", file=sys.stderr)
        return 2

    models = collect()
    if args.tables:
        for m in models:
            print("  %-34s %2d cols  %s" % (m["table"], len(m["columns"]), m["struct"]))
        print("total: %d models" % len(models))
        return 0

    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(models, fh, ensure_ascii=False, indent=2)
        print("wrote %s (%d models)" % (args.out, len(models)))
        return 0

    print(json.dumps(models, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
