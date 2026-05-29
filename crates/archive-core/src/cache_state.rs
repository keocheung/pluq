use serde::{Deserialize, Serialize};

use crate::{Rar4EntryLocation, RarEntryLocation, ZipEntryLocation};

/// Format-specific state needed to reconstruct a parser from cache without any I/O.
///
/// Each variant carries the minimum data that extraction requires beyond the
/// generic entry list.  RAR formats need preamble + end-block bytes for
/// synthetic-archive construction during cloud extraction.
#[derive(Debug, Serialize, Deserialize)]
pub enum FormatCacheState {
    Zip {
        locations: Vec<ZipEntryLocation>,
    },
    Rar {
        locations: Vec<RarEntryLocation>,
        /// RAR5 signature + main header bytes (stored as `Vec<u8>` because `Bytes` is not Serialize).
        archive_preamble: Vec<u8>,
        /// End-of-archive marker block.
        end_block: Vec<u8>,
        /// Total archive size needed to reconstruct RarArchive.
        archive_size: u64,
    },
    Rar4 {
        locations: Vec<Rar4EntryLocation>,
        archive_preamble: Vec<u8>,
        end_block: Vec<u8>,
        archive_size: u64,
    },
}
