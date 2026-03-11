//! The `ArchiveReader` trait — unified interface for all format parsers.
//!
//! CLI, server, and UI code depends only on this trait, not on format-specific
//! types like `ZipArchive`. Adding a new format (RAR, 7z, TAR) means implementing
//! this trait — consumers see no change.

use tokio::io::AsyncWrite;

use crate::junk;
use crate::{ArchiveEntry, CoreError};

/// Unified read interface implemented by every format parser.
///
/// Consumers (CLI, UI, server) use only this trait. Format-specific types
/// (`ZipArchive`, `SevenZArchive`, …) are hidden behind [`ArchiveCore`].
///
/// [`ArchiveCore`]: crate::facade::ArchiveCore
pub trait ArchiveReader {
    /// All entries in the archive, including OS-generated junk.
    fn entries(&self) -> &[ArchiveEntry];

    /// Entries with OS-generated junk filtered out.
    ///
    /// Default implementation uses the shared [`junk`] module.
    fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries()
            .iter()
            .filter(|e| !junk::is_junk(e))
            .collect()
    }

    /// Bytes that must be transferred and decompressed to produce this entry.
    ///
    /// Used by the "extraction tax" UX to warn users before expensive cloud
    /// snipe operations.
    ///
    /// - **ZIP**: `entry.compressed_size` (every entry is independently seekable)
    /// - **7z solid block**: sum of compressed sizes for all entries preceding
    ///   the target entry in the same folder, plus the target's size
    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64;

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written to `sink`.
    ///
    /// CRC verification is mandatory — this method never writes bytes that
    /// fail the integrity check.
    fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
    ) -> impl std::future::Future<Output = Result<u64, CoreError>> + Send;

    /// Decompress and verify CRC without retaining output.
    ///
    /// Equivalent to `extract()` to a no-op sink.
    /// Used for archive integrity checking (`archive-cli test` command).
    fn test(
        &self,
        entry: &ArchiveEntry,
    ) -> impl std::future::Future<Output = Result<(), CoreError>> + Send;
}
