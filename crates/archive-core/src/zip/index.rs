//! Serializable ZIP index for caching parsed Central Directory entries.
//!
//! After parsing a remote ZIP's Central Directory (which costs 2 HTTP
//! range requests), the result can be saved to disk as JSON. Subsequent
//! accesses load the index instantly — zero network requests to list files.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::io::{
    ReadError,
    read_error::{ReadErrorKind, ReadFailure, ReadOperation, SourceInfo},
};
use crate::{ArchiveEntry, ArchiveError};

/// A cached, serializable snapshot of a ZIP archive's file listing.
///
/// **Superseded by [`archive_state::ArchiveCache`]** — format-agnostic and
/// richer staleness detection. `ZipIndex` is kept for backwards compatibility
/// and will be removed in a future release.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZipIndex {
    /// Where the source archive lives (URL or file path).
    pub source: String,

    /// Total size of the source archive in bytes.
    ///
    /// Used to detect if the archive has changed since the index was created.
    pub source_size: u64,

    /// The parsed file entries.
    pub entries: Vec<ArchiveEntry>,

    /// Unix timestamp (seconds) of when this index was created.
    pub created_at: u64,
}

impl ZipIndex {
    /// Create a new index from parsed entries.
    pub fn new(source: impl Into<String>, source_size: u64, entries: Vec<ArchiveEntry>) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();

        Self {
            source: source.into(),
            source_size,
            entries,
            created_at,
        }
    }

    /// Serialize this index to a JSON file on disk.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ArchiveError> {
        let json = serde_json::to_string_pretty(self).map_err(|e| ArchiveError::InvalidFormat {
            message: format!("failed to serialize index: {e}"),
        })?;
        std::fs::write(path.as_ref(), &json).map_err(|e| {
            ArchiveError::Io(ReadError::Failed(Box::new(ReadFailure {
                source: SourceInfo::LocalFile {
                    path: path.as_ref().to_path_buf(),
                },
                operation: ReadOperation::ReadRange {
                    offset: 0,
                    len: json.len() as u64,
                },
                kind: ReadErrorKind::DiskIoError {
                    os_code: e.raw_os_error(),
                },
                diagnostic: e.to_string(),
            })))
        })?;
        Ok(())
    }

    /// Load an index from a JSON file on disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ArchiveError> {
        let json = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            ArchiveError::Io(ReadError::Failed(Box::new(ReadFailure {
                source: SourceInfo::LocalFile {
                    path: path.as_ref().to_path_buf(),
                },
                operation: ReadOperation::Open,
                kind: ReadErrorKind::DiskIoError {
                    os_code: e.raw_os_error(),
                },
                diagnostic: e.to_string(),
            })))
        })?;
        let index: ZipIndex =
            serde_json::from_str(&json).map_err(|e| ArchiveError::InvalidFormat {
                message: format!("failed to deserialize index: {e}"),
            })?;
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_serialization() {
        let entries = vec![
            ArchiveEntry {
                name: "hello.txt".into(),
                compressed_size: 10,
                uncompressed_size: 13,
                is_directory: false,
                is_encrypted: false,
                modified_at: None,
                compression_method: Some(crate::CompressionMethod::Deflate),
                index: 0,
            },
            ArchiveEntry {
                name: "data/".into(),
                compressed_size: 0,
                uncompressed_size: 0,
                is_directory: true,
                is_encrypted: false,
                modified_at: None,
                compression_method: Some(crate::CompressionMethod::Store),
                index: 1,
            },
        ];

        let index = ZipIndex::new("https://example.com/test.zip", 999, entries);

        // Save to a temp file, then load it back.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        index.save(tmp.path()).unwrap();
        let loaded = ZipIndex::load(tmp.path()).unwrap();

        assert_eq!(loaded.source, "https://example.com/test.zip");
        assert_eq!(loaded.source_size, 999);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].name, "hello.txt");
        assert_eq!(loaded.entries[0].uncompressed_size, 13);
        assert_eq!(loaded.entries[1].name, "data/");
        assert!(loaded.entries[1].is_directory);
    }
}
