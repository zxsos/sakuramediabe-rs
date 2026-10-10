"""生成并校验 Rust 测试用的 bencode 片段。

bencode 的长度前缀极易写错（`13:meta version` 里的 13 是**字符数**，
而 `meta version` 有 13 个字符、`file tree` 只有 10 个），手写极易出错。
本脚本用标准编码器生成，再自己解回来验证，最后打印可直接粘进
Rust 测试的字节串字面量。
"""

from __future__ import annotations


def encode(value: object) -> bytes:
    if isinstance(value, bool):  # bool 是 int 的子类，必须先拦
        raise TypeError("bool is not a bencode type")
    if isinstance(value, int):
        return b"i%de" % value
    if isinstance(value, bytes):
        return b"%d:" % len(value) + value
    if isinstance(value, str):
        return encode(value.encode())
    if isinstance(value, list):
        return b"l" + b"".join(encode(item) for item in value) + b"e"
    if isinstance(value, dict):
        body = b"".join(encode(k) + encode(v) for k, v in sorted(value.items()))
        return b"d" + body + b"e"
    raise TypeError(f"unsupported type: {type(value)!r}")


def decode(data: bytes, index: int = 0) -> tuple[object, int]:
    marker = data[index : index + 1]
    if marker == b"i":
        end = data.index(b"e", index)
        return int(data[index + 1 : end]), end + 1
    if marker == b"l":
        index += 1
        items: list[object] = []
        while data[index : index + 1] != b"e":
            item, index = decode(data, index)
            items.append(item)
        return items, index + 1
    if marker == b"d":
        index += 1
        mapping: dict[bytes, object] = {}
        while data[index : index + 1] != b"e":
            key, index = decode(data, index)
            value, index = decode(data, index)
            assert isinstance(key, bytes)
            mapping[key] = value
        return mapping, index + 1
    end = data.index(b":", index)
    length = int(data[index:end])
    return data[end + 1 : end + 1 + length], end + 1 + length


PIECES20 = b"01234567890123456789"

CASES: dict[str, bytes] = {
    # 纯 v2：info 内没有 pieces，宿主必须判为「缺少 v1 hash」。
    "v2_only": encode(
        {
            "info": {
                "meta version": 2,
                "name": "t.txt",
                "file tree": {"length": 1024, "attrs": ["path", ["a"]]},
            }
        }
    ),
    # hybrid：同时有 meta version 与 pieces，v1/v2 都应存在。
    "hybrid": encode({"info": {"meta version": 2, "pieces": PIECES20}}),
    # 完全没有 info 键。
    "no_info": encode({"announce": b"http://tracker/"}),
    # 普通 v1 单文件种子。
    "v1_only": encode({"info": {"length": 1, "name": "a", "pieces": PIECES20}}),
    # 嵌套 list + 负整数。
    "nested": encode([[[b"a", b"b", -3]]]),
    # 畸形整数：前导零 / 负零 / 空 / 尾随垃圾。
    # 这些是**故意构造的非法输入**，Rust 侧必须拒绝它们，所以自校验跳过。
    "int_leading_zero": b"i01e",
    "int_negative_zero": b"i-0e",
    "int_empty": b"ie",
    "int_garbage": b"i1x2e",
}

# 这些用例是故意畸形的。它们的存在意义是喂给 Rust 验证「必须拒绝」。
#
# 注意它们**不能**在这里做 Python 侧自校验：Python 的 int() 接受前导零与
# 负零（int("01") == 1、int("-0") == 0），而 Rust 侧的 bencode 解析器按
# libtorrent 的行为严格拒绝。这道语义差异本身正是对拍要验证的东西，
# 在生成器里"修正"它等于把要测的差异抹掉。
MALFORMED = {"int_leading_zero", "int_negative_zero", "int_empty", "int_garbage"}


def main() -> None:
    for name, payload in CASES.items():
        if name in MALFORMED:
            print(f"{name}")
            print(f"  bytes  = {payload!r}")
            print(f"  rust   = b\"{payload.decode('ascii')}\"")
            print("  expect = 拒绝（不与 Python 对拍，语义故意不同）")
            print()
            continue

        # 自校验：合法用例解回来必须消耗全部字节，否则长度前缀写错了。
        value, end = decode(payload)
        assert end == len(payload), f"{name}: 残留 {len(payload) - end} 字节"
        print(f"{name}")
        print(f"  bytes  = {payload!r}")
        print(f"  rust   = b\"{payload.decode('ascii')}\"")
        print(f"  len    = {len(payload)}")
        print(f"  parsed = {value!r}")
        print()


if __name__ == "__main__":
    main()
