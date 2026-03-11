use archive_core::{ArchiveCore, ArchiveEntry};
use bytes::Bytes;
use tokio::io::AsyncWrite;

use crate::entry_tree::EntryTree;
use crate::error::{translate_core_error, ArchiveError};

/// Product-facing handle to an open archive.
///
/// Wraps a live [`ArchiveCore`] and provides error translation,
/// entry tree building, and caching. Products interact with this
/// type — never with `ArchiveCore` directly.
pub struct Archive {
    core: ArchiveCore,
}

impl Archive {
    /// Open an archive from a local path or URL.
    pub async fn open(source: &str) -> Result<Self, ArchiveError> {
        // TODO: check ArchiveCache before parsing (requires fingerprint collection).
        let core = ArchiveCore::open(source)
            .await
            .map_err(translate_core_error)?;
        Ok(Self { core })
    }

    /// All entries in the archive including OS-generated junk.
    pub fn entries(&self) -> &[ArchiveEntry] {
        self.core.entries()
    }

    /// Entries with OS-generated junk files filtered out.
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.core.entries_clean()
    }

    /// Build a virtual folder tree from the flat entry list.
    pub fn entry_tree(&self) -> EntryTree {
        EntryTree::build(self.core.entries())
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written.
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
    ) -> Result<u64, ArchiveError> {
        self.core
            .extract(entry, sink)
            .await
            .map_err(translate_core_error)
    }

    /// Extract a single entry into memory.
    ///
    /// Convenience wrapper around [`extract`](Self::extract) for small entries.
    /// For large entries, use `extract()` with a file sink instead.
    pub async fn extract_to_bytes(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError> {
        self.core
            .extract_to_bytes(entry)
            .await
            .map_err(translate_core_error)
    }

    /// Bytes that must be fetched and decompressed to produce this entry.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        self.core.extraction_cost(entry)
    }

    /// Decompress and verify CRC without retaining output.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError> {
        self.core
            .test(entry)
            .await
            .map_err(translate_core_error)
    }
}
