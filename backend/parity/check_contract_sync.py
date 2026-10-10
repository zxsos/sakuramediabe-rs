#!/usr/bin/env python3
"""契约层的**两仓同步**检查。

# 为什么需要它（这不是假想的问题，是已经发生过一次的事）

同一份契约被维护在两个仓库里：

    宿主侧（本仓）：      proto/*.proto  +  crates/sm-plugin-api/src/*.rs
    插件侧（契约仓）：    sakuramedia-plugin-api/proto/*.proto  +  src/*.rs

插件按契约仓的 **git tag** 依赖它
（`sm-plugin-api = { git = "https://github.com/zxsos/sakuramedia-plugin-api.git", tag = "v0.2.0" }`），
而宿主按**本仓**的 `proto/` 编译 —— 两者靠手工同步。

P1-1（`GenerateThumbnails` 的流从 `stream ProgressEvent` 换成
`stream GenerateThumbnailsResponse`）落地时只改了这一边，于是：

  * 插件发 `ProgressEvent{text=1:LEN, current=2:VARINT}`；
  * 宿主按 `GenerateThumbnailsResponse{progress=1:LEN, done=2:LEN}` 解；
  * `field 2` 的 **wire type 不匹配** → 解码失败；
  * 宿主报的是一个「解码错误」，而真正的原因（两仓契约不同步）在日志里
    一个字都看不到。

更糟的是当时**没有任何检查能拦住它**：`ABI_MAJOR` 两边都是 1，注册校验通过；
插件的自测是两端同版本，全绿；宿主侧没有跨仓集成测试。

# 它检查两条

  1. 受管文件在两仓**逐字节相同**；
  2. 两侧的**文件集合**与下面的清单一致 —— 「新增了一个 src 模块却忘了同步」
     由第 2 条抓（清单是硬编码的，不做目录扫描：扫描会把 `target/` 之类算进来）。

# 契约仓不在时

它按 `SM_CONTRACT_ROOT` 环境变量找，缺省是**兄弟目录** `../sakuramedia-plugin-api`。
CI 或别的布局下它可能不存在 —— 那时打印 `SKIP` 并 **exit 0**：
「没有对照物」与「对照物不一致」是两件事，后者才该红。

用法：
    python check_contract_sync.py
    python check_contract_sync.py --quiet    # 只在失败时输出
"""

import argparse
import hashlib
import os
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
"""本仓（宿主）根目录。"""

# 受管文件：相对路径在两个仓库里**不同**，所以写成 (宿主路径, 契约仓路径) 对。
#
# 宿主侧多一层 `crates/sm-plugin-api/`，proto 又在仓根（见两边的 build.rs：
# 宿主的 proto 在 workspace 根，契约仓的 proto 在 crate 根）。
MANAGED = [
    ("proto/common.proto", "proto/common.proto"),
    ("proto/storage.proto", "proto/storage.proto"),
    ("proto/plugin.proto", "proto/plugin.proto"),
    ("proto/host.proto", "proto/host.proto"),
    ("crates/sm-plugin-api/src/lib.rs", "src/lib.rs"),
    ("crates/sm-plugin-api/src/provider.rs", "src/provider.rs"),
    ("crates/sm-plugin-api/src/error.rs", "src/error.rs"),
    ("crates/sm-plugin-api/src/json_struct.rs", "src/json_struct.rs"),
    ("crates/sm-plugin-api/src/movie_delivery.rs", "src/movie_delivery.rs"),
]

HOST_PROTO_DIR = "proto"
HOST_SRC_DIR = "crates/sm-plugin-api/src"
CONTRACT_PROTO_DIR = "proto"
CONTRACT_SRC_DIR = "src"


def digest(path):
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def listing(directory, suffix):
    """目录下指定后缀的文件名集合（不含子目录）。"""
    if not os.path.isdir(directory):
        return None
    return {name for name in os.listdir(directory) if name.endswith(suffix)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--quiet", action="store_true", help="只在失败时输出")
    args = parser.parse_args()

    def say(text):
        if not args.quiet:
            print(text)

    contract = os.environ.get("SM_CONTRACT_ROOT") or os.path.join(
        os.path.dirname(REPO), "sakuramedia-plugin-api"
    )

    if not os.path.isdir(contract):
        say(
            "SKIP  契约仓不在 {0}（设 SM_CONTRACT_ROOT 指定）。\n"
            "      没有对照物 ≠ 对照物不一致，所以这里不失败。".format(contract)
        )
        return 0

    problems = []

    # ① 文件集合：两侧都应与清单一致。
    for label, root, proto_dir, src_dir in [
        ("宿主", REPO, HOST_PROTO_DIR, HOST_SRC_DIR),
        ("契约仓", contract, CONTRACT_PROTO_DIR, CONTRACT_SRC_DIR),
    ]:
        for suffix, directory, expected in [
            (".proto", os.path.join(root, proto_dir),
             {path.split("/")[-1] for path, _ in MANAGED if path.endswith(".proto")}),
            (".rs", os.path.join(root, src_dir),
             {path.split("/")[-1] for _, path in MANAGED if path.endswith(".rs")}),
        ]:
            actual = listing(directory, suffix)
            if actual is None:
                problems.append("{0} 缺目录：{1}".format(label, directory))
                continue
            for missing in sorted(expected - actual):
                problems.append("{0} 缺文件：{1}".format(label, os.path.join(directory, missing)))
            for extra in sorted(actual - expected):
                # 这一条是关键：新增模块忘了登记进 MANAGED（因而忘了同步）时会命中。
                problems.append(
                    "{0} 多出未登记的文件：{1}（要么同步过去，要么登记进本脚本的 MANAGED）".format(
                        label, os.path.join(directory, extra)
                    )
                )

    # ② 逐文件逐字节比对。
    for host_rel, contract_rel in MANAGED:
        host_path = os.path.join(REPO, host_rel)
        contract_path = os.path.join(contract, contract_rel)
        if not os.path.isfile(host_path):
            problems.append("宿主缺文件：{0}".format(host_rel))
            continue
        if not os.path.isfile(contract_path):
            problems.append("契约仓缺文件：{0}".format(contract_rel))
            continue
        if digest(host_path) == digest(contract_path):
            continue
        problems.append(
            "DRIFT  {0}  ≠  {1}\n"
            "       宿主 {2} / 契约仓 {3}".format(
                host_rel, contract_rel, digest(host_path)[:12], digest(contract_path)[:12]
            )
        )

    if problems:
        print("契约两仓不同步（{0} 处）：".format(len(problems)))
        for problem in problems:
            print("  " + problem)
        print()
        print(
            "修法：把宿主侧的文件同步到契约仓（proto/ 与 src/ 各一份），\n"
            "      改了 rpc 签名还要递增 `ABI_MAJOR`、打新 tag、更新两个插件的 tag 引用。\n"
            "      步骤见 docs/tasks/proto-p1-gaps.md §二。"
        )
        return 1

    say("OK  {0} 个受管文件在两仓一致（{1}）".format(len(MANAGED), contract))
    return 0


if __name__ == "__main__":
    sys.exit(main())
