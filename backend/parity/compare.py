"""Rust <-> Python 对拍：逐条比对指纹与 BT info hash。

用法：
    python parity/compare.py            # 全量对拍
    python parity/compare.py --quick    # 跳过 8 MiB 级用例
    python parity/compare.py --debug    # 打印逐条明细

设计要点：
1. Python 侧**不复用**后端源码，而是照 `sakuramedia_local_provider/storage.py`
   与 `sakuramediabe/.../resource_hash.py` 的语义独立重写。两份独立实现
   一致，才说明 Rust 侧正确。
2. 同时比对**成功路径与失败路径**。错误码必须逐条对齐 —— 迁移最容易漏的
   恰恰是「本该报错却成功了」这种反向失败。
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import random
import re
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import unquote

REPO_ROOT = Path(__file__).resolve().parent.parent
CLI = None

HEAD_TAIL_BYTES = 3 * 1024 * 1024
MIDDLE_BYTES = 1024 * 1024
FULL_THRESHOLD = 8 * 1024 * 1024
HASH_DOMAIN = b"media-file-hash-v1"


class PyApiError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


def py_fingerprint(data: bytes) -> str:
    """media-file-hash-v1 的 Python 参照实现。"""
    size = len(data)
    if size < FULL_THRESHOLD:
        payload = (
            HASH_DOMAIN
            + b"\x00full\x00"
            + size.to_bytes(8, "big")
            + hashlib.sha1(data).digest()
        )
    else:
        head = hashlib.sha1(data[:HEAD_TAIL_BYTES]).digest()
        tail = hashlib.sha1(data[size - HEAD_TAIL_BYTES :]).digest()
        slots = (size - 2 * HEAD_TAIL_BYTES) // MIDDLE_BYTES
        slot_1 = int.from_bytes(head[:8], "big") % slots
        cand = int.from_bytes(tail[:8], "big") % (slots - 1)
        slot_2 = cand if cand < slot_1 else cand + 1
        mid_1 = hashlib.sha1(
            data[HEAD_TAIL_BYTES + slot_1 * MIDDLE_BYTES :][:MIDDLE_BYTES]
        ).digest()
        mid_2 = hashlib.sha1(
            data[HEAD_TAIL_BYTES + slot_2 * MIDDLE_BYTES :][:MIDDLE_BYTES]
        ).digest()
        payload = (
            HASH_DOMAIN
            + b"\x00sampled\x00"
            + size.to_bytes(8, "big")
            + head
            + tail
            + mid_1
            + mid_2
        )
    return "media-file-hash-v1:" + hashlib.sha1(payload).hexdigest()


def py_canonical(value: str) -> str:
    value = value.strip()
    if re.fullmatch(r"[0-9a-fA-F]{40}", value):
        return value.lower()
    if re.fullmatch(r"[A-Za-z2-7]{32}", value):
        return base64.b32decode(value.upper()).hex()
    raise PyApiError("invalid_download_resource_hash")


def py_magnet_hash(uri: str) -> str:
    match = re.search(r"urn:btih:([A-Za-z0-9]+)", unquote(uri), re.IGNORECASE)
    if match is None:
        raise PyApiError("invalid_download_resource_hash")
    return py_canonical(match.group(1))


def bencode(value) -> bytes:
    if isinstance(value, int):
        return b"i%de" % value
    if isinstance(value, bytes):
        return b"%d:" % len(value) + value
    if isinstance(value, str):
        return bencode(value.encode())
    if isinstance(value, list):
        return b"l" + b"".join(bencode(v) for v in value) + b"e"
    if isinstance(value, dict):
        body = b"".join(bencode(k) + bencode(v) for k, v in sorted(value.items()))
        return b"d" + body + b"e"
    raise TypeError(type(value))


def _bdecode(data: bytes, i: int):
    marker = data[i : i + 1]
    if marker == b"i":
        j = data.index(b"e", i)
        body = data[i + 1 : j]
        if not re.fullmatch(rb"-?(0|[1-9][0-9]*)", body or b""):
            raise ValueError("bad integer")
        return int(body), j + 1
    if marker == b"l":
        i += 1
        out = []
        while data[i : i + 1] != b"e":
            v, i = _bdecode(data, i)
            out.append(v)
        return out, i + 1
    if marker == b"d":
        i += 1
        out = {}
        while data[i : i + 1] != b"e":
            k, i = _bdecode(data, i)
            v, i = _bdecode(data, i)
            out[k] = v
        return out, i + 1
    j = data.index(b":", i)
    n = int(data[i:j])
    return data[j + 1 : j + 1 + n], j + 1 + n


def _info_span(data: bytes) -> tuple[int, int] | None:
    """定位顶层字典中 `info` 值的**原始字节**区间。

    v1 info hash 是对 info 字典的原始编码字节做 SHA-1，任何重新编码
    （哪怕语义等价）都会得到不同哈希，因此必须取原始切片。
    """
    if data[:1] != b"d":
        return None
    i = 1
    while True:
        marker = data[i : i + 1]
        if marker in (b"", b"e"):
            return None
        if not (b"0" <= marker <= b"9"):
            return None
        try:
            key, i = _bdecode(data, i)
            start = i
            _, i = _bdecode(data, i)
        except Exception:
            return None
        if key == b"info":
            return (start, i)


def py_torrent_v1_hash(payload: bytes) -> str:
    """替代 libtorrent：定位 info 字典取 v1 hash；缺少 pieces 即视为无 v1。"""
    try:
        value, end = _bdecode(payload, 0)
    except Exception:
        raise PyApiError("invalid_download_torrent") from None
    if end != len(payload) or not isinstance(value, dict):
        raise PyApiError("invalid_download_torrent")
    info = value.get(b"info")
    if not isinstance(info, dict) or b"pieces" not in info:
        raise PyApiError("invalid_download_torrent")
    span = _info_span(payload)
    if span is None:
        raise PyApiError("invalid_download_torrent")
    return hashlib.sha1(payload[span[0] : span[1]]).hexdigest()


def find_cli() -> Path:
    names = ["parity-cli.exe", "parity-cli"]
    for folder in ("release", "debug"):
        for name in names:
            candidate = REPO_ROOT / "target" / folder / name
            if candidate.exists():
                return candidate
    raise SystemExit(
        "找不到 parity-cli，请先运行: cargo build --release -p parity-cli"
    )


class Parity:
    def __init__(self, cli: Path, verbose: bool = False):
        self.cli = cli
        self.passed = 0
        self.failures: list[str] = []
        self.verbose = verbose

    def run(self, *args: str) -> dict[str, str]:
        proc = subprocess.run(
            [str(self.cli), *args], capture_output=True, text=True, check=False
        )
        fields: dict[str, str] = {}
        for line in proc.stdout.splitlines():
            key, sep, value = line.partition(": ")
            if sep:
                fields[key.strip()] = value.strip()
        return fields

    def ok(self, label: str, rust: dict[str, str], expected: str) -> None:
        if rust.get("ok") == "true" and rust.get("hash") == expected:
            self.passed += 1
            if self.verbose:
                print(f"  PASS {label}")
        else:
            got = rust.get("hash") or rust.get("reason") or rust
            self.failures.append(f"{label}: 期望 {expected}，实得 {got}")
            print(f"  FAIL {label}: 期望 {expected}，实得 {got}")

    def err(self, label: str, rust: dict[str, str], code: str) -> None:
        if rust.get("ok") == "false" and rust.get("code") == code:
            self.passed += 1
            if self.verbose:
                print(f"  PASS {label}")
        else:
            got = rust.get("code") or rust.get("reason") or rust
            self.failures.append(f"{label}: 期望错误 {code}，实得 {got}")
            print(f"  FAIL {label}: 期望错误 {code}，实得 {got}")

    def either(self, label: str, rust: dict, expected, code) -> None:
        if code is None:
            self.ok(label, rust, expected)
        else:
            self.err(label, rust, code)


def fingerprint_cases(quick: bool) -> list[tuple[str, bytes]]:
    def blob(n: int, seed: int) -> bytes:
        return random.Random(seed).randbytes(n)

    cases = [
        ("empty", b""),
        ("one-byte", b"a"),
        ("abc", b"abc"),
        ("64KiB", blob(64 * 1024, 1)),
    ]
    if not quick:
        cases += [
            ("threshold-minus-1", blob(FULL_THRESHOLD - 1, 2)),
            ("threshold", blob(FULL_THRESHOLD, 3)),
            ("threshold+1", blob(FULL_THRESHOLD + 1, 4)),
            ("9MiB-odd", blob(9 * 1024 * 1024 + 12345, 5)),
            ("64MiB", blob(64 * 1024 * 1024, 6)),
        ]
    return cases


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--debug", action="store_true")
    args = parser.parse_args()

    parity = Parity(find_cli(), verbose=args.debug)
    hex_hash = "DD8255ECDC7CA55FB0FBF81323D87062DB1F6D1C"
    b32_hash = base64.b32encode(bytes.fromhex(hex_hash)).decode()

    print("== 指纹算法 (media-file-hash-v1) ==")
    # 大块数据必须走文件：Windows 命令行有 32 KiB 上限，8 MiB 的十六进制
    # 远超限制，因此除小数据外一律用 fingerprint <path>。
    with tempfile.TemporaryDirectory() as tmp:
        for label, data in fingerprint_cases(args.quick):
            path = Path(tmp) / f"{label}.bin"
            path.write_bytes(data)
            parity.ok(
                f"fingerprint/{label}",
                parity.run("fingerprint", str(path)),
                py_fingerprint(data),
            )

    print("== 内存字节路径（不落盘）==")
    for label, data in (("empty", b""), ("abc", b"abc"), ("4KiB", random.Random(1).randbytes(4096))):
        parity.ok(
            f"bytes/{label}",
            parity.run("fingerprint-bytes", data.hex()),
            py_fingerprint(data),
        )

    print("== 采样读取量不变量（恒定 8 MiB / 4 次读）==")
    with tempfile.TemporaryDirectory() as tmp:
        for label, size in (
            ("16MiB", 16 * 1024 * 1024),
            ("64MiB", 64 * 1024 * 1024),
            ("threshold+1", FULL_THRESHOLD + 1),
        ):
            path = Path(tmp) / f"probe-{label}.bin"
            path.write_bytes(random.Random(9).randbytes(size))
            rust = parity.run("fingerprint-reads-file", str(path))
            if rust.get("ok") == "true" and rust.get("bytes") == str(8 * 1024 * 1024) and rust.get("reads") == "4":
                parity.passed += 1
                print(f"  PASS {label}: 8 MiB / 4 reads")
            else:
                parity.failures.append(f"reads/{label}: {rust}")
                print(f"  FAIL reads/{label}: {rust}")

    print("== 小文件走全量分支（1 次读）==")
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "small.bin"
        path.write_bytes(random.Random(11).randbytes(1024))
        rust = parity.run("fingerprint-reads-file", str(path))
        if rust.get("ok") == "true" and rust.get("reads") == "1" and rust.get("bytes") == "1024":
            parity.passed += 1
            print("  PASS small: 1 read / 1024 bytes")
        else:
            parity.failures.append(f"reads/small: {rust}")
            print(f"  FAIL reads/small: {rust}")

    print("== info hash 规范化 ==")
    for label, value in [
        ("lower-hex", hex_hash.lower()),
        ("upper-hex", hex_hash),
        ("whitespace", f"  {hex_hash} "),
        ("base32", b32_hash),
        ("base32-lower", b32_hash.lower()),
        ("too-short", "abc"),
        ("non-hex-40", "z" * 40),
        ("base32-bad-digit", "0" * 32),
    ]:
        try:
            expected, code = py_canonical(value), None
        except PyApiError as exc:
            expected, code = None, exc.code
        parity.either(f"canonical/{label}", parity.run("canonical", value), expected, code)

    print("== 磁力链接 ==")
    for label, uri in [
        ("plain", f"magnet:?xt=urn:btih:{hex_hash}&dn=x"),
        ("upper-scheme", f"MAGNET:?XT=URN:BTIH:{hex_hash}"),
        ("base32-body", f"magnet:?xt=urn:btih:{b32_hash}"),
        ("no-hash", "magnet:?dn=x"),
    ]:
        try:
            expected, code = py_magnet_hash(uri), None
        except PyApiError as exc:
            expected, code = None, exc.code
        parity.either(f"magnet/{label}", parity.run("magnet", uri), expected, code)

    print("== .torrent info hash ==")
    pieces = b"01234567890123456789"
    torrents = [
        ("v1", bencode({"info": {"length": 1, "name": "a", "pieces": pieces}})),
        ("v2-only", bencode({"info": {"meta version": 2, "name": "t"}})),
        ("hybrid", bencode({"info": {"meta version": 2, "pieces": pieces}})),
        ("no-info", bencode({"announce": b"http://tracker/"})),
        ("garbage", b"not a torrent at all"),
        ("empty", b""),
        ("truncated", bencode({"info": {"length": 1}})[:-3]),
        ("bad-int", b"d4:infod5:piecesi01eee"),
        ("top-level-list", b"li1ee"),
    ]
    with tempfile.TemporaryDirectory() as tmp:
        for label, payload in torrents:
            path = Path(tmp) / f"{label}.torrent"
            path.write_bytes(payload)
            try:
                expected, code = py_torrent_v1_hash(payload), None
            except PyApiError as exc:
                expected, code = None, exc.code
            parity.either(f"torrent/{label}", parity.run("torrent", str(path)), expected, code)

    print("== scheme 白名单 ==")
    for label, uri, code in [
        ("http", "http://example.com/a.torrent", "invalid_download_source"),
        ("https-upper", "HTTPS://example.com/a.torrent", "invalid_download_source"),
        ("ftp", "ftp://example.com/a.torrent", "invalid_download_source"),
        ("file", "file:///etc/passwd", "invalid_download_source"),
        ("no-scheme", "example.com/a.torrent", "invalid_download_source"),
        ("empty-host", "https://", "invalid_download_source"),
        ("magnet-shortcut", "magnet:?dn=x", "invalid_download_resource_hash"),
    ]:
        parity.err(f"scheme/{label}", parity.run("resolve", uri), code)

    print()
    total = parity.passed + len(parity.failures)
    print(f"对拍结果：通过 {parity.passed}/{total}，失败 {len(parity.failures)}")
    for failure in parity.failures:
        print(f"  - {failure}")
    return 0 if not parity.failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
