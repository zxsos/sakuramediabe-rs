sakuramediabe-rs
================

本项目是 **sakuramediabe** 的 Rust 重写实现。

上游项目
--------

    项目名：  sakuramediabe
    仓库：    https://github.com/tinypinglite/sakuramediabe
    作者：    tinypinglite
    许可证：  GNU General Public License v3.0

许可证
------

本项目与上游项目采用**相同的许可证**：GNU General Public License v3.0
（见同目录 `LICENSE`）。

本项目包含大量直接取自上游项目的代码，涵盖但不限于：

  - 数据模型定义与表结构（40 张表的字段、索引、唯一约束、删除行为）
  - 文件指纹算法 `media-file-hash-v1`（协议规范与测试向量）
  - BT info hash 规范化规则（`canonical_info_hash`）
  - bencode 解析与 `.torrent` 校验语义
  - 缩略图生成状态机与图搜索索引状态取值
  - JWT 签发与校验约定、Argon2 密码哈希参数
  - 统一错误信封与 API 错误码（`{error: {code, message, details}}`）

上述内容构成 GPL-3.0 意义上的**衍生作品**，因此本项目整体同样以 GPL-3.0
授权分发。

本项目作者对上游项目商标、名称与数据内容的归属不作任何主张。
SakuraMedia 相关的商标与权利归其各自所有者所有。

兼容性说明
----------

GPL-3.0 是 copyleft 许可证：发布本项目的二进制或衍生版本时，必须：

  1. 保留本 `LICENSE` 与本文件中的归属声明；
  2. 以 GPL-3.0 授权分发全部源码；
  3. 不得施加与 GPL-3.0 冲突的附加限制。

若你希望以更宽松的许可证（如 MIT / Apache-2.0）分发本项目，则**不可行**：
那将违反上游项目的 copyleft 条款，也无法合法地声称代码所有权。

第三方依赖
----------

本项目自身依赖的 crates 各自遵循其上游许可证，见对应 `Cargo.toml`。
完整依赖清单可用以下命令查看：

    cargo tree

各依赖的许可证可通过 `cargo license`（需安装 `cargo-license` 插件）审计。
