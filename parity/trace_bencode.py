"""逐行模拟 svc-hash 的 parse_torrent，定位 Rust 侧解析失败点。

只用标准库，逻辑与 crates/svc-hash/src/bencode.rs 一一对应。
"""

from __future__ import annotations

MAX_DEPTH = 32
MAX_INTEGER_DIGITS = 24


class Err(Exception):
    def __init__(self, kind: str):
        super().__init__(kind)
        self.kind = kind


def read_string(data: bytes, cursor: int) -> tuple[bytes, int]:
    start = cursor
    while cursor < len(data) and data[cursor : cursor + 1] != b":":
        if not chr(data[cursor]).isdigit():
            raise Err("InvalidStringLength")
        cursor += 1
    if cursor >= len(data) or data[cursor : cursor + 1] != b":":
        raise Err("UnexpectedEnd")
    digits = data[start:cursor]
    if not digits or len(digits) > 20:
        raise Err("InvalidStringLength")
    declared = int(digits)
    cursor += 1
    available = len(data) - cursor
    if declared > available:
        raise Err(f"StringLengthOutOfRange(declared={declared},available={available})")
    out = data[cursor : cursor + declared]
    return out, cursor + declared


def is_valid_integer(digits: bytes) -> bool:
    negative = digits[:1] == b"-"
    body = digits[1:] if negative else digits
    if not body or len(body) > MAX_INTEGER_DIGITS:
        return False
    if not all(chr(b).isdigit() for b in body):
        return False
    if len(body) > 1 and body[0:1] == b"0":
        return False
    if negative and body == b"0":
        return False
    return True


def skip_value(data: bytes, cursor: int, depth: int) -> int:
    if depth > MAX_DEPTH:
        raise Err("DepthLimitExceeded")
    if cursor >= len(data):
        raise Err("UnexpectedEnd")
    marker = data[cursor : cursor + 1]

    if marker == b"i":
        cursor += 1
        start = cursor
        while cursor < len(data):
            byte = data[cursor : cursor + 1]
            if byte == b"e":
                if not is_valid_integer(data[start:cursor]):
                    raise Err("InvalidInteger")
                return cursor + 1
            if byte == b"-" or (b"0" <= byte <= b"9"):
                cursor += 1
                continue
            raise Err("InvalidInteger")
        raise Err("UnexpectedEnd")

    if marker == b"l":
        cursor += 1
        while True:
            if cursor >= len(data):
                raise Err("UnexpectedEnd")
            if data[cursor : cursor + 1] == b"e":
                return cursor + 1
            cursor = skip_value(data, cursor, depth + 1)

    if marker == b"d":
        cursor += 1
        while True:
            if cursor >= len(data):
                raise Err("UnexpectedEnd")
            if data[cursor : cursor + 1] == b"e":
                return cursor + 1
            if b"0" <= data[cursor : cursor + 1] <= b"9":
                _, cursor = read_string(data, cursor)
                cursor = skip_value(data, cursor, depth + 1)
                continue
            raise Err("NonStringDictionaryKey")

    if b"0" <= marker <= b"9":
        _, cursor = read_string(data, cursor)
        return cursor

    raise Err(f"InvalidTypeMarker({marker!r})")


def read_dictionary_keys(data: bytes, start: int, end: int) -> list[bytes]:
    if data[start : start + 1] != b"d":
        raise Err("TopLevelNotDictionary")
    cursor = start + 1
    keys: list[bytes] = []
    while cursor < end:
        marker = data[cursor : cursor + 1]
        if marker == b"e":
            break
        if b"0" <= marker <= b"9":
            key, cursor = read_string(data, cursor)
            cursor = skip_value(data, cursor, 1)
            keys.append(key)
            continue
        raise Err("NonStringDictionaryKey")
    return keys


def parse_torrent(data: bytes) -> tuple[tuple[int, int], list[bytes]]:
    if data[:1] != b"d":
        raise Err("TopLevelNotDictionary")
    cursor = 1
    info_span = None
    info_keys: list[bytes] = []
    while True:
        marker = data[cursor : cursor + 1]
        if marker == b"":
            raise Err("UnexpectedEnd")
        if marker == b"e":
            cursor += 1
            break
        if b"0" <= marker <= b"9":
            key, cursor = read_string(data, cursor)
            value_start = cursor
            cursor = skip_value(data, cursor, 1)
            if key == b"info":
                info_span = (value_start, cursor)
                info_keys = read_dictionary_keys(data, value_start, cursor)
            continue
        raise Err(f"InvalidTypeMarker({marker!r})")
    if cursor != len(data):
        raise Err(f"UnexpectedEnd(cursor={cursor},len={len(data)})")
    if info_span is None:
        raise Err("MissingInfoKey")
    return info_span, info_keys


CASES = {
    "v2_only": b"d4:infod12:meta versioni2ee",
    "hybrid": b"d4:infod12:meta versioni2e6:pieces20:01234567890123456789ee",
    "no_info": b"d8:announce15:http://tracker/e",
    "v1_only": b"d4:infod6:lengthi1e4:name1:a6:pieces20:01234567890123456789ee",
    "nested": b"lll1:a1:bi-3eeee",
    "int_leading_zero": b"i01e",
    "int_negative_zero": b"i-0e",
    "int_empty": b"ie",
    "int_garbage": b"i1x2e",
}


def main() -> None:
    for name, payload in CASES.items():
        try:
            (start, end), keys = parse_torrent(payload)
            has_v1 = b"pieces" in keys
            has_v2 = b"meta version" in keys
            info = payload[start:end]
            print(f"{name:18} OK   span={start}..{end} keys={keys} v1={has_v1} v2={has_v2}")
        except Err as exc:
            print(f"{name:18} ERR  {exc.kind}")


if __name__ == "__main__":
    main()
