//! 对拍工具：把 Rust 侧结果以 `key: value` 行输出，供 Python 逐字段比对。
//!
//! 输出刻意用最朴素的纯文本格式（无依赖手写序列化），
//! 因为它的唯一消费者是 `parity/compare.py`，稳定比优雅重要。
//!
//! 约定：出错时输出 `ok: false` + `status` + `code`，与 Python 侧
//! `ApiError.code` 同名，这样两侧的失败路径也能逐条对齐。

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use media_file_hash::{compute_file_hash_verified, RandomAccessRead, SliceSource};
use svc_hash::{
    canonical_info_hash, magnet_info_hash, resolve_from_source, torrent_v1_info_hash, ResolveError,
};

fn emit_error(error: ResolveError) -> ExitCode {
    println!("ok: false");
    println!("status: {}", error.status_and_code().0);
    println!("code: {}", error.status_and_code().1);
    ExitCode::SUCCESS
}

fn emit_ok(entries: &[(&str, String)]) -> ExitCode {
    println!("ok: true");
    for (key, value) in entries {
        println!("{key}: {value}");
    }
    ExitCode::SUCCESS
}

fn emit_simple(hash: String) -> ExitCode {
    emit_ok(&[("hash", hash)])
}

fn emit_failure(reason: &str) -> ExitCode {
    println!("ok: false");
    println!("reason: {reason}");
    ExitCode::SUCCESS
}

fn read_hex(hex: &str) -> Result<Vec<u8>, ExitCode> {
    hashing::unhex(hex).map_err(|error| {
        println!("ok: false");
        println!("bad_hex: {error}");
        ExitCode::SUCCESS
    })
}

/// 本地文件指纹（走 `compute_file_hash_verified`，含 fstat 前后校验）。
fn cmd_fingerprint(path: &str) -> ExitCode {
    match compute_file_hash_verified(Path::new(path)) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_failure(&format!("{error}")),
    }
}

/// 内存字节指纹，避免对拍时为每个用例落盘。
fn cmd_fingerprint_bytes(hex: &str) -> ExitCode {
    let bytes = match read_hex(hex) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    let mut source = SliceSource::new(bytes);
    match media_file_hash::compute_file_hash(&mut source, None) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_failure(&format!("{error}")),
    }
}

/// 统计采样分支实际发起的读次数与总字节数。
///
/// 这一对拍维度验证的是「采样恒定读 8 MiB」这一算法不变量 ——
/// 它与文件大小无关，是跨存储去重能做到定长 IO 的根本。
struct Counting {
    inner: Box<dyn RandomAccessRead>,
    reads: u64,
    bytes: u64,
}

impl Counting {
    fn new(inner: Box<dyn RandomAccessRead>) -> Self {
        Self {
            inner,
            reads: 0,
            bytes: 0,
        }
    }
}

impl RandomAccessRead for Counting {
    fn size(&self) -> Result<u64, media_file_hash::HashError> {
        self.inner.size()
    }
    fn read_exact_at(
        &mut self,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, media_file_hash::HashError> {
        self.reads += 1;
        self.bytes += length;
        self.inner.read_exact_at(offset, length)
    }
}

fn fingerprint_with_reads(mut source: Counting) -> ExitCode {
    match media_file_hash::compute_file_hash(&mut source, None) {
        Ok(hash) => emit_ok(&[
            ("hash", hash),
            ("reads", source.reads.to_string()),
            ("bytes", source.bytes.to_string()),
        ]),
        Err(error) => emit_failure(&format!("{error}")),
    }
}

fn cmd_fingerprint_reads(hex: &str) -> ExitCode {
    let bytes = match read_hex(hex) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    fingerprint_with_reads(Counting::new(Box::new(SliceSource::new(bytes))))
}

/// 文件版读取量统计：避开 Windows 命令行 32 KiB 长度上限。
fn cmd_fingerprint_reads_file(path: &str) -> ExitCode {
    match media_file_hash::FileSource::open(Path::new(path)) {
        Ok(source) => fingerprint_with_reads(Counting::new(Box::new(source))),
        Err(error) => emit_failure(&format!("{error}")),
    }
}

/// `.torrent` 文件的 BT v1 info hash。
fn cmd_torrent(path: &str) -> ExitCode {
    match std::fs::read(path) {
        Ok(bytes) => finish_torrent(&bytes),
        Err(error) => emit_failure(&format!("{error}")),
    }
}

/// `.torrent` 字节（十六进制）的 BT v1 info hash。
fn cmd_torrent_bytes(hex: &str) -> ExitCode {
    let bytes = match read_hex(hex) {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    finish_torrent(&bytes)
}

fn finish_torrent(bytes: &[u8]) -> ExitCode {
    match torrent_v1_info_hash(bytes) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_error(error),
    }
}

/// 磁力链接 info hash。
fn cmd_magnet(uri: &str) -> ExitCode {
    match magnet_info_hash(uri) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_error(error),
    }
}

/// info hash 规范化（hex 大小写 / base32）。
fn cmd_canonical(value: &str) -> ExitCode {
    match canonical_info_hash(value) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_error(error),
    }
}

/// 完整分派入口，语义与 Python 的 `resolve_resource_hash` 一致：
/// 磁力链接直接解析，http(s) 链接必须附带已抓取的 `.torrent` 字节。
fn cmd_resolve(uri: &str, torrent_path: Option<&str>) -> ExitCode {
    let payload = match torrent_path {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) => return emit_failure(&format!("{error}")),
        },
        None => None,
    };
    match resolve_from_source(uri, payload.as_deref()) {
        Ok(hash) => emit_simple(hash),
        Err(error) => emit_error(error),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  \
         parity-cli fingerprint <path>\n  \
         parity-cli fingerprint-bytes <hex>\n  \
         parity-cli fingerprint-reads <hex>\n  \
         parity-cli fingerprint-reads-file <path>\n  \
         parity-cli torrent <path>\n  \
         parity-cli torrent-bytes <hex>\n  \
         parity-cli magnet <uri>\n  \
         parity-cli canonical <value>\n  \
         parity-cli resolve <uri> [torrent-path]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        return usage();
    };
    let arg = args.get(1).map(String::as_str);

    match (command.as_str(), arg) {
        ("fingerprint", Some(path)) => cmd_fingerprint(path),
        ("fingerprint-bytes", Some(hex)) => cmd_fingerprint_bytes(hex),
        ("fingerprint-reads", Some(hex)) => cmd_fingerprint_reads(hex),
        ("fingerprint-reads-file", Some(path)) => cmd_fingerprint_reads_file(path),
        ("torrent", Some(path)) => cmd_torrent(path),
        ("torrent-bytes", Some(hex)) => cmd_torrent_bytes(hex),
        ("magnet", Some(uri)) => cmd_magnet(uri),
        ("canonical", Some(value)) => cmd_canonical(value),
        ("resolve", Some(uri)) => cmd_resolve(uri, args.get(2).map(String::as_str)),
        _ => usage(),
    }
}
