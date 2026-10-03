#!/usr/bin/env python3
"""从 Peewee 模型源码提取数据库 schema 契约。

只用 `ast` 解析，不用正则 —— 正则会在跨行参数、嵌套调用上误判，
而这份契约是 40 张表的唯一权威来源，解析必须精确。

提取内容（DDL 生成与对拍都需要）：
  * 表名（Meta.table_name，缺省时用 Peewee 的 snake_case 推导）
  * 列名、列类型、长度、可空性
  * 默认值（Python 侧 default=）与服务端默认值（constraints=[SQL("DEFAULT ...")]）
  * 唯一约束、字段级索引
  * 外键：目标模型、目标列、ON DELETE 动作
  * Meta.indexes（复合索引）
  * Meta.constraints（CHECK 等，原样透传）
  * 模块级 add_index（函数索引 / 排序索引 / 修饰符索引三种形式）

用法：
    python schema_contract.py            # 打印 JSON 契约
    python schema_contract.py --tables   # 只打印表名与列数
    python schema_contract.py --out X    # 导出契约供 gen_ddl.py 使用
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

# ON DELETE 动作 -> PostgreSQL 参照动作。
ON_DELETE = {
    "CASCADE": "CASCADE",
    "RESTRICT": "RESTRICT",
    "SET NULL": "SET NULL",
    "SET DEFAULT": "SET DEFAULT",
    "NO ACTION": "NO ACTION",
    "PROTECT": "RESTRICT",   # peewee 3.17+ 新增；PG 无 PROTECT
    "": "NO ACTION",
}

# 继承链带来隐式列的地方。
#
# 隐式列的 `type` 不是占位符：Peewee 4.x 里 `class AutoField(IntegerField)`
# 且 field_type 'AUTO' 的 DDL 落点是 INTEGER，所以隐式 id 就是 4 字节 integer
# （不是 bigint）。早先把它标成 "implicit" 让对拍直接跳过，等于放任 id 的
# 类型在两侧漂移 —— 现在给出真实类型，让 L1 对拍真正覆盖到主键。
IMPLICIT_COLUMNS = {
    "BaseModel": ["id"],
    "TimestampedMixin": ["id", "created_at", "updated_at"],
}

# 隐式列的具体类型与额外属性。
IMPLICIT_COLUMN_TYPES = {
    # Peewee: class AutoField(IntegerField)，AUTO 在 PG 上落 INTEGER
    # 主键必然 NOT NULL —— 显式写出来，不让它落到 UNKNOWN_NULLABLE。
    "id": {"type": "int4", "primary_key": True, "auto_increment": True, "nullable": False},
    "created_at": {"type": "timestamp", "nullable": True},
    "updated_at": {"type": "timestamp", "nullable": True},
}

# 「这条列的可空性没能从源码解析出来」的哨兵值。
#
# 刻意**不**用 None：那与「Peewee 默认 NOT NULL」在真假判断里无法区分，
# 而把两者混同正是可空性对拍一直空转的原因（None is False 为假，于是
# 省略 null= 的列全部免检）。
#
# 任何以 UNKNOWN_NULLABLE 为可空性的列都会让对拍失败，直到有人查清上游
# 到底怎么声明的。这比「猜 NOT NULL 然后静默」安全：前者会问你，后者
# 只是让一个 Option<String> 混进模型层，等到插入时才炸。
UNKNOWN_NULLABLE = "unknown"


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


def dotted_name(node) -> str:
    """把 `peewee.fn.UPPER` 这样的属性链还原成点号路径。"""
    parts = []
    cur = node
    while isinstance(cur, ast.Attribute):
        parts.append(cur.attr)
        cur = cur.value
    if isinstance(cur, ast.Name):
        parts.append(cur.id)
    return ".".join(reversed(parts))


def literal(node):
    """尽力求值字面量；求不出返回 None。

    `ast.literal_eval` 不认裸名字，而 peewee 的 JSON 字段默认值恰恰写成
    名字 —— `JsonTextField(default=dict)` / `JsonbField(default=list)`。
    这些名字在这里显式映射成对应的空 JSON 值，否则默认值会被整个丢掉，
    DDL 里就出现 `NOT NULL` 而没有 DEFAULT。

    丢掉的后果不是「少一个默认值」那么轻：列变成 NOT NULL 且无默认，
    任何 INSERT 都必须显式传值，连「留空」都做不到。
    """
    if isinstance(node, ast.Name):
        # 只认 JSON 语义里明确的空容器，不做通用名字解析 ——
        # 把 `default=some_constant` 猜成字面量会造出错误的 schema。
        builtin = {
            "dict": {},
            "list": [],
            "str": "",
            "int": 0,
            "float": 0.0,
            "bool": False,
        }
        return builtin.get(node.id)
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


def sql_text_of(node) -> str | None:
    """从 `SQL("...")` 里取出 SQL 文本。"""
    if isinstance(node, ast.Call) and call_name(node) == "SQL":
        return literal(node.args[0]) if node.args else None
    return None


def parse_server_default(constraints_node) -> str | None:
    """从 `constraints=[SQL("DEFAULT ...")]` 里提取服务端默认值。

    这与 Peewee kwargs 里的 Python 侧 `default=` 是两回事：
    前者进 DDL（裸 INSERT 也有兜底），后者只影响 peewee 写路径。
    """
    if constraints_node is None:
        return None
    if isinstance(constraints_node, ast.List):
        items = constraints_node.elts
    else:
        items = [constraints_node]
    for item in items:
        text = sql_text_of(item)
        if text and text.strip().upper().startswith("DEFAULT"):
            return text.strip()
    return None


def parse_check_constraints(meta_body) -> list[str]:
    """提取 `Meta.constraints` 里的完整 SQL（CHECK 等），原样保留。"""
    out = []
    for meta in meta_body:
        if not isinstance(meta, ast.Assign):
            continue
        if meta.targets[0].id != "constraints":
            continue
        if not isinstance(meta.value, ast.List):
            continue
        for item in meta.value.elts:
            text = sql_text_of(item)
            if text:
                out.append(text.strip())
    return out


def parse_index_expression(node) -> str | None:
    """把 add_index 里的表达式渲染成 PostgreSQL 索引列片段。

    四种形式都要覆盖：
      * `peewee.fn.UPPER(Movie.movie_number)` -> `(UPPER(movie_number))`
      * `peewee.Ordering(Movie.release_date, "DESC", nulls="last")`
        -> `release_date DESC NULLS LAST`
      * `"origin text_pattern_ops"` -> `origin text_pattern_ops`
      * `Model.column`（表达式里的裸列引用）-> `column`
    """
    # 形式 1：字符串（带或不带 opclass 修饰符）
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value

    # 形式 4：裸列引用 `Model.column` —— 只取列名，模型前缀在索引里无意义。
    if isinstance(node, ast.Attribute):
        return node.attr

    if not isinstance(node, ast.Call):
        return None

    func = dotted_name(node.func)

    # 形式 2：函数索引，如 peewee.fn.UPPER(col)。
    # 判断必须用「路径里含 fn 段」而不是「以 fn. 开头」——dotted_name 返回的
    # 是完整路径 `peewee.fn.UPPER`，后者永远不匹配，会静默丢掉函数索引。
    segments = func.split(".")
    if "fn" in segments or func.startswith("SQL"):
        fn_name = segments[-1]
        if not node.args:
            return None
        inner = parse_index_expression(node.args[0])
        if inner is None:
            return None
        return "%s(%s)" % (fn_name.lower(), inner)

    # 形式 3：Ordering(col, "DESC", nulls="last")
    if func.endswith("Ordering") or func == "Ordering":
        if not node.args:
            return None
        col = parse_index_expression(node.args[0])
        if col is None:
            return None
        direction = literal(node.args[1]) if len(node.args) > 1 else None
        out = col
        if direction:
            out += " " + str(direction).upper()
        for kw in node.keywords:
            if kw.arg == "nulls":
                nulls = literal(kw.value)
                if nulls:
                    out += " NULLS " + str(nulls).upper()
        return out

    return None


def parse_module_indexes(tree) -> dict[str, list[dict]]:
    """扫模块级 `Model.add_index(peewee.ModelIndex(...))` 调用。

    这类索引不在 `Meta.indexes` 里，必须单独扫。返回 {类名: [索引...]}。
    """
    out: dict[str, list[dict]] = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        if call_name(node) != "add_index" or not node.args:
            continue
        # 接收者是 `Model.add_index`，即 node.func.value 是模型名
        target = node.func.value
        model_name = target.id if isinstance(target, ast.Name) else None
        if not model_name:
            continue
        inner = node.args[0]
        if call_name(inner) != "ModelIndex" or len(inner.args) < 2:
            continue
        fields_node = inner.args[1]
        if not isinstance(fields_node, ast.Tuple):
            continue
        expressions = []
        ok = True
        for elt in fields_node.elts:
            rendered = parse_index_expression(elt)
            if rendered is None:
                ok = False
                break
            expressions.append(rendered)
        if not ok:
            continue
        index_name = None
        for kw in inner.keywords:
            if kw.arg == "name":
                index_name = literal(kw.value)
        if not index_name:
            index_name = "%s_%s" % (
                snake_case(model_name),
                "_".join(re.sub(r"\W+", "_", e) for e in expressions)[:40].strip("_"),
            )
        out.setdefault(model_name, []).append(
            {"name": index_name, "expressions": expressions, "unique": False}
        )
    return out


def parse_model_file(path: str) -> list:
    with open(path, "r", encoding="utf-8") as fh:
        source = fh.read()
    tree = ast.parse(source, filename=path)
    module_indexes = parse_module_indexes(tree)

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
        columns: dict[str, dict] = {}
        for base in bases:
            for col in IMPLICIT_COLUMNS.get(base, []):
                spec = IMPLICIT_COLUMN_TYPES.get(col, {})
                columns[col] = {
                    "name": col,
                    "type": spec.get("type", "implicit"),
                    "field": col,
                    "nullable": spec.get("nullable", UNKNOWN_NULLABLE),
                    "unique": bool(spec.get("primary_key")),
                    "index": False,
                    "default": None,
                    "max_length": None,
                    "server_default": None,
                    "on_delete": None,
                    "ref_model": None,
                    "ref_field": None,
                    "primary_key": bool(spec.get("primary_key")),
                    "auto_increment": bool(spec.get("auto_increment")),
                }

        table_name = None
        indexes: list = []
        check_constraints: list = []
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
                        # 默认 False，不是 None。
                        #
                        # Peewee 的 Field 默认 null=False，所以省略 null= 的列
                        # **确实**是 NOT NULL —— 这是完全确定的，不是「解析
                        # 不出来」。之前这里填 None，而 compare_schema.py 的
                        # 判定是 `if py_nullable is False and optional`：
                        # None 不是 False，于是省略 null= 的列全部免检。
                        #
                        # 代价是整个可空性对拍形同虚设：actor 表 26 列里 9 列
                        # 属于这种情况，包括 javdb_id。Rust 侧把它声明成
                        # Option<String>，归一化时空串变 None，插入必然违反
                        # NOT NULL —— 而对拍报 0 problems，集成测试还断言
                        # 这次插入成功。
                        #
                        # 真正「解析不出来」的情形用 UNKNOWN_NULLABLE 表达，
                        # 与 False 区分开：那会让对拍失败并要求有人查清上游，
                        # 而不是猜一个值蒙混过关。
                        "nullable": False,
                        "unique": False,
                        "index": False,
                        "default": None,
                        "max_length": None,
                        "server_default": None,
                        "on_delete": None,
                        "ref_model": None,
                        "ref_field": None,
                    }
                    for kw in stmt.value.keywords:
                        if kw.arg == "null":
                            entry["nullable"] = literal(kw.value)
                        elif kw.arg == "unique":
                            entry["unique"] = bool(literal(kw.value))
                        elif kw.arg == "index":
                            entry["index"] = bool(literal(kw.value))
                        elif kw.arg == "default":
                            entry["default"] = literal(kw.value)
                        elif kw.arg == "max_length":
                            entry["max_length"] = literal(kw.value)
                        elif kw.arg == "constraints":
                            entry["server_default"] = parse_server_default(kw.value)
                        elif kw.arg == "on_delete":
                            raw = literal(kw.value)
                            entry["on_delete"] = ON_DELETE.get(
                                raw if isinstance(raw, str) else "", "NO ACTION"
                            )
                    if kind == "fk":
                        # ForeignKeyField(Model, ...) 第一个位置参数是目标模型。
                        # 自引用写成字符串 "self"（Actor.merged_into 就是这样），
                        # 用 ast.Constant 而非 ast.Name，必须单独处理。
                        explicit_field = None
                        if stmt.value.args:
                            target = stmt.value.args[0]
                            if isinstance(target, ast.Name):
                                entry["ref_model"] = target.id
                            elif isinstance(target, ast.Attribute):
                                entry["ref_model"] = target.attr
                            elif isinstance(target, ast.Constant) and target.value == "self":
                                entry["ref_model"] = node.name
                                entry["self_ref"] = True
                        # field= 显式指定被引用字段时才用它。
                        #
                        # 两种写法都要认：
                        #   field="movie_number"          -> ast.Constant
                        #   field=Movie.movie_number      -> ast.Attribute
                        # 后者在 Media 上是主力写法（media.movie 指向
                        # Movie.movie_number 而不是 Movie.id），literal() 处理不了
                        # ast.Attribute，早期因此把它当成「没写 field=」，
                        # 生成的 DDL 变成 REFERENCES movie (id) —— 类型也对不上，
                        # PostgreSQL 报 "foreign key constraint cannot be
                        # implemented"。
                        for kw in stmt.value.keywords:
                            if kw.arg != "field":
                                continue
                            if isinstance(kw.value, ast.Attribute):
                                explicit_field = kw.value.attr
                            elif isinstance(kw.value, ast.Constant):
                                explicit_field = kw.value.value
                            else:
                                explicit_field = literal(kw.value)
                        # **默认值是目标模型的主键名，不是源列名。**
                        #
                        # Peewee 的 ForeignKeyField(Model) 不带 field= 时指向
                        # Model.id。早期这里错写成 entry["ref_field"] = actual
                        # （源列名），于是 media.library_id 生成的 DDL 是
                        # `REFERENCES media_library (library_id)` —— 目标表根本没
                        # 这一列。而且因为 media.movie_number 的目标字段恰好同名，
                        # 只有它看起来是对的，把问题掩盖住了。
                        #
                        # 这个 bug 靠静态检查发现不了：契约自洽、DDL 能生成、
                        # 对拍也过（对拍只比对 Rust 结构体与 Peewee 字段，不校验
                        # 外键指向）。只有真正 CREATE TABLE 时 PostgreSQL 才报错。
                        entry["ref_field"] = explicit_field
                        entry["ref_field_explicit"] = explicit_field is not None
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
                            raw = ast.literal_eval(meta.value)
                            # 索引元组里写的是**字段名**（download_task 的
                            # ('client', 'remote_id')），实际列名是 client_id。
                            # 映射必须在字段解析完成之后构建 —— 提前建会拿到
                            # 空字典，索引就会引用不存在的列。
                            f2c = {
                                c["field"]: c["name"] for c in columns.values()
                            }
                            indexes = [
                                (
                                    tuple(f2c.get(col, col) for col in cols),
                                    uniq,
                                )
                                for cols, uniq in raw
                            ]
                        except Exception:
                            indexes = []
                check_constraints = parse_check_constraints(stmt.body)

        models.append(
            {
                "table": table_name or snake_case(node.name),
                "struct": node.name,
                "bases": bases,
                "columns": columns,
                "indexes": indexes,
                "check_constraints": check_constraints,
                "module_indexes": module_indexes.get(node.name, []),
                "source": os.path.relpath(path, MODEL_ROOT),
            }
        )
    return models


def collect():
    """解析全部模型，并把外键的 ref_model（类名）解析成真实表名。

    两遍是必要的：ForeignKeyField(Model) 里的 `Model` 是**类名**，
    而列里要的是表名；表名可能定义在另一个文件里（跨域引用很常见）。
    """
    models: list = []
    for dirpath, _dirnames, filenames in os.walk(MODEL_ROOT):
        for name in sorted(filenames):
            if not name.endswith(".py") or name == "__init__.py":
                continue
            models.extend(parse_model_file(os.path.join(dirpath, name)))
    models.sort(key=lambda m: m["table"])

    # 第一遍结果建立 类名 -> 表名 映射。
    class_to_table = {m["struct"]: m["table"] for m in models}
    table_to_model = {m["table"]: m for m in models}

    # 回填外键目标。找不到映射时保留类名并在 report 里暴露，不静默丢弃。
    for m in models:
        for col in m["columns"].values():
            if col["type"] != "fk" or not col["ref_model"]:
                continue
            target_table = class_to_table.get(col["ref_model"])
            if target_table:
                col["ref_table"] = target_table
            else:
                # 自引用或跨文件未收录：按 snake_case 兜底。
                col["ref_table"] = snake_case(col["ref_model"])
            col.pop("ref_model", None)

    # 第二遍：外键的列类型必须从**目标列**推导，不能一律当 int4。
    #
    # `Media.movie` 声明为 `ForeignKeyField(Movie, field=Movie.movie_number,
    # column_name="movie_number")` —— 它指向 movie_number（varchar 255 的
    # 字符串），不是 movie.id。若按 int4 建表，这张表根本建不出来。
    #
    # 还要解递归：外键可能指向另一个外键的列（链式），此时要一路查到终点。
    def resolve_type(model, column, depth=0):
        # 基例：目标列本身不是外键，它的类型就是答案。
        # 早先这里返回 column.get("ref_column_type")，而普通列根本没有这个
        # 键，于是 falls back 到 int4 —— media.movie_number 指向
        # movie.movie_number（varchar 255）却被当成 integer。
        if column["type"] != "fk":
            return column["type"], column.get("max_length")
        if depth > 8:
            # 链式外键异常深，保守按 int4 处理。
            return "int4", None
        target = table_to_model.get(column.get("ref_table"))
        if target is None:
            return "int4", None
        nxt = target["columns"].get(column.get("ref_field") or column["name"])
        if nxt is None:
            return "int4", None
        return resolve_type(target, nxt, depth + 1)

    for m in models:
        for col in m["columns"].values():
            if col["type"] != "fk":
                continue
            ctype, max_len = resolve_type(m, col)
            col["ref_column_type"] = ctype
            col["ref_column_max_length"] = max_len
    return models


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tables", action="store_true", help="只打印表名与列数")
    ap.add_argument("--out", help="把 JSON 契约写到该路径")
    ap.add_argument(
        "--indexes", action="store_true", help="打印索引与约束清单"
    )
    args = ap.parse_args()

    if not os.path.isdir(MODEL_ROOT):
        print("model root not found: %s" % MODEL_ROOT, file=sys.stderr)
        print("set PEEWEE_ROOT to the backend's src/model", file=sys.stderr)
        return 2

    models = collect()

    if args.tables:
        for m in models:
            print(
                "  %-34s %2d cols  %s"
                % (m["table"], len(m["columns"]), m["struct"])
            )
        print("total: %d models" % len(models))
        return 0

    if args.indexes:
        total_idx = total_chk = 0
        for m in models:
            n_idx = len(m["indexes"]) + len(m["module_indexes"])
            n_chk = len(m["check_constraints"])
            total_idx += n_idx
            total_chk += n_chk
            if not n_idx and not n_chk:
                continue
            print("%s" % m["table"])
            for entry in m["indexes"]:
                cols, uniq = entry
                print(
                    "    [meta] %s (%s)"
                    % ("UNIQUE" if uniq else "INDEX", ", ".join(cols))
                )
            for entry in m["module_indexes"]:
                print(
                    "    [module] %s (%s)"
                    % (entry["name"], ", ".join(entry["expressions"]))
                )
            for text in m["check_constraints"]:
                print("    [check] %s" % text)
        print("\ntotal: %d indexes, %d check constraints" % (total_idx, total_chk))
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
