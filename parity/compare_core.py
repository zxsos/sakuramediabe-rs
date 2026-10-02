"""sm-core 契约对拍：Rust 实现 vs Python 参照实现。

参照实现严格照客户端 Dart 代码的语义重写（不读 Rust 源码），
两份独立实现一致，才说明 Rust 侧正确。

对照的客户端文件：
  lib/core/network/api_error_dto.dart
  lib/core/network/paginated_response_dto.dart
  lib/core/json/json_parse.dart

用法：
  python parity/compare_core.py
  python parity/compare_core.py --debug
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CLI = REPO_ROOT / "target" / "debug" / "core_parity.exe"
if not CLI.exists():
    CLI = REPO_ROOT / "target" / "debug" / "core_parity"

# 客户端 Dart 的默认回��值
DEFAULT_CODE = "unknown_error"
DEFAULT_MESSAGE = "Unknown error"
DEFAULT_PAGE = 1
DEFAULT_PAGE_SIZE = 20
DEFAULT_TOTAL = 0

# Dart 的 DateTime.tryParse 只接受 ISO-8601，且非法值静默返回 null。
ISO_RE = re.compile(
    r"^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?$"
)

def dart_as_int(value, fallback):
    """对应 Dart json_parse.asInt(value, fallback: fallback)。"""
    if isinstance(value, bool):
        return fallback  # Dart: bool 不是 int
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return fallback
    return fallback

def dart_as_datetime(value):
    """对应 Dart asDateTime：非字符串或 trim 后为空返回 None，非法格式也 None。"""
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    return text if ISO_RE.match(text) else None

def py_error_from_body(body):
    """对应 ApiErrorDto.fromJson。"""
    if not isinstance(body, dict):
        return DEFAULT_CODE, DEFAULT_MESSAGE, None
    envelope = body.get("error")
    if not isinstance(envelope, dict):
        return DEFAULT_CODE, DEFAULT_MESSAGE, None
    code = envelope.get("code")
    message = envelope.get("message")
    details = envelope.get("details")
    return (
        code if isinstance(code, str) else DEFAULT_CODE,
        message if isinstance(message, str) else DEFAULT_MESSAGE,
        details if isinstance(details, dict) else None,
    )

def py_page_from_body(body):
    """对应 PaginatedResponseDto.fromJson。

    注意客户端用 whereType<Map>() 强制过滤非对象元素。
    """
    if not isinstance(body, dict):
        return [], DEFAULT_PAGE, DEFAULT_PAGE_SIZE, DEFAULT_TOTAL, None
    raw = body.get("items")
    items = [item for item in raw if isinstance(item, dict)] if isinstance(raw, list) else []
    return (
        items,
        dart_as_int(body.get("page"), DEFAULT_PAGE),
        dart_as_int(body.get("page_size"), DEFAULT_PAGE_SIZE),
        dart_as_int(body.get("total"), DEFAULT_TOTAL),
        dart_as_datetime(body.get("synced_at")),
    )


class Parity:
    def __init__(self, cli: Path, verbose: bool = False):
        self.cli = cli
        self.passed = 0
        self.failures: list[str] = []
        self.verbose = verbose

    def run(self, *args: str) -> dict[str, str]:
        proc = subprocess.run(
            [str(self.cli), *args], capture_output=True, text=True, check=False, encoding="utf-8", errors="replace"
        )
        fields: dict[str, str] = {}
        for line in proc.stdout.splitlines():
            key, sep, value = line.partition(": ")
            if sep:
                fields[key.strip()] = value.strip()
        return fields

    def expect(self, label: str, rust: dict, expected: dict[str, str]) -> None:
        bad = [
            f"{k}: 期望 {v!r}，实得 {rust.get(k)!r}"
            for k, v in expected.items()
            if rust.get(k) != v
        ]
        if not bad:
            self.passed += 1
            if self.verbose:
                print(f"  PASS {label}")
        else:
            joined = "; ".join(bad)
            self.failures.append(f"{label}: {joined}")
            print(f"  FAIL {label}: {joined}")

    def call(self, command: str, body: object, *rest: str) -> dict[str, str]:
        return self.run(command, json.dumps(body, ensure_ascii=False), *rest)

ERROR_CASES = [
    ("well-formed", {"error": {"code": "not_found", "message": "未找到", "details": {"id": 7}}}),
    ("no-details", {"error": {"code": "e", "message": "m"}}),
    ("empty-error", {"error": {}}),
    ("code-not-string", {"error": {"code": 42, "message": True}}),
    ("details-not-object", {"error": {"code": "x", "message": "y", "details": [1, 2]}}),
    ("error-null", {"error": None}),
    ("no-error-key", {"message": "孤儿"}),
    ("top-level-array", [1, 2, 3]),
    ("top-level-null", None),
    ("top-level-string", "text"),
]

PAGE_CASES = [
    ("well-formed", {"items": [{"id": 1}, {"id": 2}], "page": 1, "page_size": 2, "total": 7, "synced_at": "2026-10-02T12:00:00+08:00"}),
    ("empty-body", {}),
    ("numbers-as-strings", {"page": "2", "page_size": "50", "total": "120"}),
    ("items-not-list", {"items": "nope", "total": 3}),
    ("items-mixed", {"items": [{"id": 1}, "text", 5, None]}),
    ("synced-empty", {"items": [], "synced_at": ""}),
    ("synced-malformed", {"items": [], "synced_at": "not-a-date"}),
    ("synced-not-string", {"items": [], "synced_at": 20261002}),
    ("float-page", {"page": 2.9, "page_size": 1.5, "total": 9.9}),
    ("negative-total", {"items": [], "total": -5}),
]

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--debug", action="store_true")
    args = parser.parse_args()

    if not CLI.exists():
        print("找不到 core_parity，请先运行: cargo build -p sm-core --bin core_parity")
        return 2

    parity = Parity(CLI, verbose=args.debug)

    print("== 错误信封 ApiError.from_body ==")
    for label, body in ERROR_CASES:
        rust = parity.call("error-from-body", body)
        code, message, details = py_error_from_body(body)
        expected = {"code": code, "message": message, "has_details": str(details is not None).lower()}
        if details is not None:
            expected["details"] = json.dumps(details, ensure_ascii=False, separators=(",", ":"))
        parity.expect(f"error/{label}", rust, expected)

    print("== 错误信封序列化（details 为 None 时省略键）==")
    for label, with_details in (("without-details", False), ("with-details", True)):
        rust = parity.call("make-error", {"with_details": with_details}, "some_code")
        # 用 json.dumps 构造期望值，避免手写 JSON 时的引号转义陷阱。
        expected_obj = {"code": "some_code", "message": "消息"}
        if with_details:
            expected_obj["details"] = {"k": "v"}
        expected = json.dumps(expected_obj, ensure_ascii=False, separators=(",", ":"))
        parity.expect(f"serialize/{label}", rust, {"serialized": expected})

    print("== 分页响应 Paginated.from_body ==")
    for label, body in PAGE_CASES:
        rust = parity.call("page-from-body", body)
        items, page, page_size, total, synced = py_page_from_body(body)
        parity.expect(
            f"page/{label}",
            rust,
            {
                "items": json.dumps(items, ensure_ascii=False, separators=(",", ":")),
                "page": str(page),
                "page_size": str(page_size),
                "total": str(total),
                "has_synced_at": str(synced is not None).lower(),
            },
        )

    print()
    total = parity.passed + len(parity.failures)
    print(f"对拍结果：通过 {parity.passed}/{total}，失败 {len(parity.failures)}")
    for failure in parity.failures:
        print(f"  - {failure}")
    return 0 if not parity.failures else 1

if __name__ == "__main__":
    raise SystemExit(main())
