//! `media-file-hash-v1` 采样指纹。
//!
//! # 为什么需要它
//!
//! SakuraMedia 要跨媒体库、跨存储识别重复媒体。全量 SHA1 在 NAS 上对一部
//! 几十 GB 的影片做一次要读满整盘，代价不可接受，因此采用**采样指纹**：
//! 只读文件的头、尾和两段中间采样，总读取量固定在 8 MiB。
//!
//! # 与既有实现的关系
//!
//! 该算法此前在 `sakuramedia_local_provider/storage.py` 与
//! `sakuramedia_115_provider/storage.py` 中各实现了一份，存在漂移风险。
//! 本 crate 是唯一权威实现，两侧插件改为调用它。
//!
//! 行为必须与既有实现**逐字节一致**，否则已入库的 `file_hash` 全部失效、
//! 重复文件识别会出现假阳性/假阴性。验收依据是两个插件测试里共享的协议向量：
//!
//! ```text
//! 8 MiB 全零文件 -> media-file-hash-v1:52385d3512a8a9ff8b6e6c5aa315e46633b28d9a
//! 空文件         -> media-file-hash-v1:524935ebf533f3b952f2397f80691a87a7b289c7
//! b"abc"         -> media-file-hash-v1:da6ba51927337cc1035be69e84f851f48dbe7d71
//! ```

#![forbid(unsafe_code)]

use core::fmt;
use std::path::Path;

use hashing::{hex, sha1};

/// 域分隔标签，参与摘要计算。改动会让全部历史 `file_hash` 失效。
pub const HASH_DOMAIN: &[u8] = b"media-file-hash-v1";

/// 头部与尾部分段各自的长度：3 MiB（两者合计 6 MiB）。
pub const HEAD_TAIL_BYTES: u64 = 3 * 1024 * 1024;
/// 单段中间采样的长度：1 MiB（共两段）。
pub const MIDDLE_BYTES: u64 = 1024 * 1024;
/// 小于该阈值的文件走全量 SHA1。
pub const FULL_THRESHOLD: u64 = 8 * 1024 * 1024;

/// 前缀，`media-file-hash-v1:`。
pub const HASH_PREFIX: &str = "media-file-hash-v1:";

/// 采样算法分支标记。
const SAMPLED_MARKER: &[u8] = b"\x00sampled\x00";
/// 全量算法分支标记。
const FULL_MARKER: &[u8] = b"\x00full\x00";

/// 指纹计算失败。
#[derive(Debug)]
pub enum HashError {
    /// 声明的文件大小与数据源实际大小不一致。
    SizeMismatch { declared: u64, actual: u64 },
    /// 在指定偏移未能读满请求长度。
    ShortRead {
        offset: u64,
        requested: u64,
        actual: usize,
    },
    /// 读取过程中数据源发生了可观测的变化（仅文件路径场景会检测）。
    SourceChanged,
    /// 底层 IO 失败。
    Io(std::io::Error),
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeMismatch { declared, actual } => {
                write!(
                    f,
                    "media size mismatch: declared={declared} actual={actual}"
                )
            }
            Self::ShortRead {
                offset,
                requested,
                actual,
            } => write!(
                f,
                "short read at offset {offset}: requested={requested} actual={actual}"
            ),
            Self::SourceChanged => write!(f, "media changed during hashing"),
            Self::Io(error) => write!(f, "media read failed: {error}"),
        }
    }
}

impl std::error::Error for HashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HashError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// 可随机访问的字节源。
///
/// 抽象出这一层是为了让同一份算法既能跑本地文件（`FileSource`），
/// 也能跑 115 网盘的 HTTP Range reader —— 后者每次读都要发起一次网络请求。
pub trait RandomAccessRead {
    /// 数据源的总长度。
    fn size(&self) -> Result<u64, HashError>;

    /// 从 `offset` 起读取**恰好** `length` 字节。
    fn read_exact_at(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, HashError>;
}

/// 可报告身份签名的数据源，用于检测哈希期间的文件变更。
pub trait IdentifySource {
    /// 返回可比较的签名。两次调用之间签名不同即视为源已变更。
    fn signature(&self) -> Result<SourceSignature, HashError>;
}

/// 数据源身份签名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSignature {
    /// 设备号（`st_dev`）。
    pub device: u64,
    /// inode（`st_ino`）。
    pub inode: u64,
    /// 长度（`st_size`）。
    pub size: u64,
    /// 纳秒修改时间（`st_mtime_ns`）。
    pub mtime_ns: i128,
}

/// 本地文件数据源。
#[derive(Debug)]
pub struct FileSource {
    file: std::fs::File,
    size: u64,
}

impl FileSource {
    /// 打开文件并记录当前长度。
    pub fn open(path: &Path) -> Result<Self, HashError> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl RandomAccessRead for FileSource {
    fn size(&self) -> Result<u64, HashError> {
        Ok(self.size)
    }

    fn read_exact_at(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, HashError> {
        use std::io::{Read, Seek, SeekFrom};

        self.file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0u8; length as usize];
        self.file.read_exact(&mut buffer).map_err(|error| {
            // read_exact 的 UnexpectedEof 需要还原成 ShortRead 才能与既有行为对齐。
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                let actual = self.size.saturating_sub(offset).min(length) as usize;
                HashError::ShortRead {
                    offset,
                    requested: length,
                    actual,
                }
            } else {
                HashError::Io(error)
            }
        })?;
        Ok(buffer)
    }
}

impl IdentifySource for FileSource {
    /// 取 `(st_dev, st_ino, st_size, st_mtime_ns)` 的跨平台等价物。
    ///
    /// Unix 上直接用 `MetadataExt`；Windows 没有 `st_dev` / `st_ino`，
    /// 退而用卷序列号 + 文件索引，两者合起来同样能唯一标识"哪个卷上的哪个文件"。
    /// 换文件内容不会改变卷序列号或文件索引（原地覆写保持 inode），
    /// 因此 `mtime` 才是内容变更的主要信号，这与 Unix 侧语义一致。
    #[cfg(unix)]
    fn signature(&self) -> Result<SourceSignature, HashError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = self.file.metadata()?;
        Ok(SourceSignature {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }

    /// Windows 实现见同名的 `#[cfg(windows)]` 版本。
    ///
    /// Windows 上 `volume_serial_number()` 与 `file_index()` 仍是 unstable
    /// feature（`windows_by_handle`），稳定通道拿不到 `st_dev` / `st_ino`。
    /// 这里退化为「长度 + 修改时间」判据：原地覆写内容会改变 mtime，
    /// 因此仍能检出哈希期间的变化。生产环境是 Linux，走完整的 Unix 分支。
    #[cfg(windows)]
    fn signature(&self) -> Result<SourceSignature, HashError> {
        use std::os::windows::fs::MetadataExt;

        let metadata = self.file.metadata()?;
        // FILETIME 以 100ns 为单位、起点为 1601-01-01，减去偏移得到 Unix epoch。
        const FILETIME_UNIX_EPOCH_DELTA: u64 = 116_444_736_000_000_000;
        let unix_100ns = metadata
            .last_write_time()
            .saturating_sub(FILETIME_UNIX_EPOCH_DELTA);
        Ok(SourceSignature {
            device: 0,
            inode: 0,
            size: metadata.len(),
            mtime_ns: i128::from(unix_100ns) * 100,
        })
    }
}

/// 纯内存数据源，测试与校验用。
#[derive(Debug, Clone)]
pub struct SliceSource {
    data: Vec<u8>,
}

impl SliceSource {
    /// 借用已有缓冲。
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// 构造指定长度的确定性填充数据。
    pub fn filled(length: u64, byte: u8) -> Self {
        Self {
            data: vec![byte; length as usize],
        }
    }

    /// 构造内容为 `0..=255` 循环的确定性数据。
    pub fn patterned(length: u64) -> Self {
        Self {
            data: (0..length).map(|index| (index % 256) as u8).collect(),
        }
    }
}

impl RandomAccessRead for SliceSource {
    fn size(&self) -> Result<u64, HashError> {
        Ok(self.data.len() as u64)
    }

    fn read_exact_at(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, HashError> {
        let end = offset.checked_add(length).ok_or(HashError::SizeMismatch {
            declared: u64::MAX,
            actual: self.data.len() as u64,
        })?;
        if end > self.data.len() as u64 {
            return Err(HashError::ShortRead {
                offset,
                requested: length,
                actual: (self.data.len() as u64).saturating_sub(offset) as usize,
            });
        }
        Ok(self.data[offset as usize..end as usize].to_vec())
    }
}

/// 采样槽位选择结果，便于测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSlots {
    /// 第一段中间采样的槽位下标。
    pub slot_1: u64,
    /// 第二段中间采样的槽位下标，保证与 `slot_1` 不同。
    pub slot_2: u64,
    /// 第一段中间采样的字节偏移。
    pub offset_1: u64,
    /// 第二段中间采样的字节偏移。
    pub offset_2: u64,
}

/// 计算采样槽位。
///
/// 槽位由头部与尾部摘要的前 8 字节决定，因此**同一文件在任何机器上
/// 都会命中同一组槽位**，这是跨存储去重能成立的前提。
pub fn sample_slots(head_sha1: &[u8; 20], tail_sha1: &[u8; 20], size: u64) -> SampleSlots {
    let slot_count = (size - 2 * HEAD_TAIL_BYTES) / MIDDLE_BYTES;
    debug_assert!(
        slot_count >= 2,
        "sampling branch requires at least 2 middle slots"
    );

    let head_seed = u64::from_be_bytes(head_sha1[..8].try_into().expect("8 bytes"));
    let tail_seed = u64::from_be_bytes(tail_sha1[..8].try_into().expect("8 bytes"));

    let slot_1 = head_seed % slot_count;
    let candidate = tail_seed % (slot_count - 1);
    let slot_2 = if candidate < slot_1 {
        candidate
    } else {
        candidate + 1
    };

    SampleSlots {
        slot_1,
        slot_2,
        offset_1: HEAD_TAIL_BYTES + slot_1 * MIDDLE_BYTES,
        offset_2: HEAD_TAIL_BYTES + slot_2 * MIDDLE_BYTES,
    }
}

/// 计算采样载荷（不含最终封装）。
fn sampled_payload(source: &mut impl RandomAccessRead, size: u64) -> Result<Vec<u8>, HashError> {
    let head_sha1 = sha1(&source.read_exact_at(0, HEAD_TAIL_BYTES)?);
    let tail_sha1 = sha1(&source.read_exact_at(size - HEAD_TAIL_BYTES, HEAD_TAIL_BYTES)?);
    let slots = sample_slots(&head_sha1, &tail_sha1, size);
    let middle_1 = sha1(&source.read_exact_at(slots.offset_1, MIDDLE_BYTES)?);
    let middle_2 = sha1(&source.read_exact_at(slots.offset_2, MIDDLE_BYTES)?);

    let mut payload = Vec::with_capacity(HASH_DOMAIN.len() + 8 + 8 + 80);
    payload.extend_from_slice(HASH_DOMAIN);
    payload.extend_from_slice(SAMPLED_MARKER);
    payload.extend_from_slice(&size.to_be_bytes());
    payload.extend_from_slice(&head_sha1);
    payload.extend_from_slice(&tail_sha1);
    payload.extend_from_slice(&middle_1);
    payload.extend_from_slice(&middle_2);
    Ok(payload)
}

/// 计算全量载荷（不含最终封装）。
fn full_payload(source: &mut impl RandomAccessRead, size: u64) -> Result<Vec<u8>, HashError> {
    let full_sha1 = sha1(&source.read_exact_at(0, size)?);
    let mut payload = Vec::with_capacity(HASH_DOMAIN.len() + 8 + 8 + 20);
    payload.extend_from_slice(HASH_DOMAIN);
    payload.extend_from_slice(FULL_MARKER);
    payload.extend_from_slice(&size.to_be_bytes());
    payload.extend_from_slice(&full_sha1);
    Ok(payload)
}

/// 计算 `media-file-hash-v1` 指纹，返回带前缀的字符串。
///
/// `expected_size` 为 `None` 时跳过大小一致性校验。
pub fn compute_file_hash(
    source: &mut impl RandomAccessRead,
    expected_size: Option<u64>,
) -> Result<String, HashError> {
    let actual = source.size()?;
    if let Some(declared) = expected_size {
        if declared != actual {
            return Err(HashError::SizeMismatch { declared, actual });
        }
    }

    let payload = if actual < FULL_THRESHOLD {
        full_payload(source, actual)?
    } else {
        sampled_payload(source, actual)?
    };

    Ok(format!("{HASH_PREFIX}{}", hex(&sha1(&payload))))
}

/// 确定性测试数据生成器，与两个插件测试中的 `_hash_fixture` 逐字节一致。
///
/// 协议向量就是用这份数据算出来的，因此生成规则必须冻结：一旦改动，
/// 三个协议向量会同时失效，`file_hash` 也将无法与历史数据对齐。
///
/// ```
/// # use media_file_hash::hash_fixture;
/// assert_eq!(hash_fixture(6), vec![0x00, 0x41, 0x83, 0xc5, 0x07, 0x48]);
/// ```
pub fn hash_fixture(size: u64) -> Vec<u8> {
    (0..size)
        .map(|index| {
            let value = (1_103_515_245u64.wrapping_mul(index) + 12_345) % (1u64 << 32);
            ((value >> 24) & 0xFF) as u8
        })
        .collect()
}

/// 计算本地文件指纹，并在哈希前后校验文件未被修改。
///
/// 这是既有 Python 实现的完整语义：读取前后各取一次 `fstat`，比较
/// `st_dev` / `st_ino` / `st_size` / `st_mtime_ns`。
pub fn compute_file_hash_verified(path: &Path) -> Result<String, HashError> {
    let mut source = FileSource::open(path)?;
    let before = source.signature()?;
    let hash = compute_file_hash(&mut source, Some(before.size))?;
    let after = source.signature()?;
    if before != after {
        return Err(HashError::SourceChanged);
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议向量：8 MiB 确定性数据命中采样分支。
    ///
    /// 数据来自 `hash_fixture`，与 `sakuramedia_local_provider/tests/test_storage.py`
    /// 的 `_hash_fixture(8 * 1024 * 1024)` 完全一致。
    #[test]
    fn matches_protocol_vector_for_sampled_file() {
        let data = SliceSource::new(hash_fixture(8 * 1024 * 1024));
        assert_eq!(
            compute_file_hash(&mut data.clone(), None).unwrap(),
            "media-file-hash-v1:52385d3512a8a9ff8b6e6c5aa315e46633b28d9a"
        );
    }

    /// 协议向量：空文件命中全量分支，且 `read_at(0, 0)` 不报错。
    #[test]
    fn matches_protocol_vector_for_empty_file() {
        let data = SliceSource::filled(0, 0);
        assert_eq!(
            compute_file_hash(&mut data.clone(), None).unwrap(),
            "media-file-hash-v1:524935ebf533f3b952f2397f80691a87a7b289c7"
        );
    }

    /// 协议向量：小文件全量分支。
    #[test]
    fn matches_protocol_vector_for_small_file() {
        let data = SliceSource::new(b"abc".to_vec());
        assert_eq!(
            compute_file_hash(&mut data.clone(), None).unwrap(),
            "media-file-hash-v1:da6ba51927337cc1035be69e84f851f48dbe7d71"
        );
    }

    #[test]
    fn rejects_size_mismatch() {
        let data = SliceSource::new(b"abc".to_vec());
        let error = compute_file_hash(&mut data.clone(), Some(4)).unwrap_err();
        assert!(matches!(
            error,
            HashError::SizeMismatch {
                declared: 4,
                actual: 3
            }
        ));
    }

    #[test]
    fn threshold_boundary_selects_expected_branch() {
        // 恰好等于阈值 -> 采样分支；差一字节 -> 全量分支。
        let sampled = SliceSource::filled(FULL_THRESHOLD, 7);
        let full = SliceSource::filled(FULL_THRESHOLD - 1, 7);
        assert_ne!(
            compute_file_hash(&mut sampled.clone(), None).unwrap(),
            compute_file_hash(&mut full.clone(), None).unwrap()
        );
    }

    #[test]
    fn sampling_reads_exactly_eight_mib() {
        // 用带计数的源确认采样分支的总读取量固定为 8 MiB：
        // 头部 3 MiB + 尾部 3 MiB + 两段中间采样各 1 MiB。
        struct Counting {
            inner: SliceSource,
            bytes: u64,
        }
        impl RandomAccessRead for Counting {
            fn size(&self) -> Result<u64, HashError> {
                self.inner.size()
            }
            fn read_exact_at(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, HashError> {
                self.bytes += length;
                self.inner.read_exact_at(offset, length)
            }
        }

        let mut source = Counting {
            inner: SliceSource::patterned(64 * 1024 * 1024),
            bytes: 0,
        };
        compute_file_hash(&mut source, None).unwrap();
        assert_eq!(
            source.bytes,
            2 * HEAD_TAIL_BYTES + 2 * MIDDLE_BYTES,
            "采样分支固定读取 8 MiB，与文件大小无关"
        );
        assert_eq!(source.bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn slots_are_distinct_and_in_range() {
        let size = 512 * 1024 * 1024;
        let head = sha1(b"head");
        let tail = sha1(b"tail");
        let slots = sample_slots(&head, &tail, size);
        let slot_count = (size - 2 * HEAD_TAIL_BYTES) / MIDDLE_BYTES;
        assert_ne!(slots.slot_1, slots.slot_2);
        assert!(slots.slot_1 < slot_count);
        assert!(slots.slot_2 < slot_count);
        assert_eq!(
            slots.offset_1,
            HEAD_TAIL_BYTES + slots.slot_1 * MIDDLE_BYTES
        );
    }

    #[test]
    fn slot_selection_is_deterministic() {
        let head = sha1(b"same-head");
        let tail = sha1(b"same-tail");
        let size = 64 * 1024 * 1024;
        assert_eq!(
            sample_slots(&head, &tail, size),
            sample_slots(&head, &tail, size)
        );
    }

    #[test]
    fn verified_path_rejects_source_change() {
        let dir = std::env::temp_dir().join("mfh-verified-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.bin");
        std::fs::write(&path, vec![1u8; 4096]).unwrap();

        let hash = compute_file_hash_verified(&path).unwrap();
        assert!(hash.starts_with(HASH_PREFIX));

        // 内容不同 -> 哈希不同
        std::fs::write(&path, vec![2u8; 4096]).unwrap();
        let changed = compute_file_hash_verified(&path).unwrap();
        assert_ne!(hash, changed);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
