//! `transfers` 域模型。
//!
//! 对应后端 `src/model/transfers/` 的 6 张表，全部集中在 `downloads.py`。

pub mod downloads;

pub use downloads::{
    download_state, import_status, indexer_kind, DownloadClient, DownloadResourceBlacklist,
    DownloadSubmissionRecord, DownloadTask, Indexer, IndexerDownloadClient,
};
