//! The `ArchiveReader` trait — unified interface for all format parsers.
//!
//! CLI, server, and UI code depends only on this trait, not on format-specific
//! types like `ZipArchive`. Adding a new format (RAR, 7z, TAR) means implementing
//! this trait — consumers see no change.

use std::sync::{Arc, Mutex};

use tokio::io::AsyncWrite;

use crate::junk;
use crate::progress::ProgressReporter;
use crate::scan::ScanInfo;
use crate::{ArchiveEntry, CoreError};

/// Unified read interface implemented by every format parser.
///
/// Consumers (CLI, UI, server) use only this trait. Format-specific types
/// (`ZipArchive`, `RarArchive`, …) are hidden behind [`Archive`](archive::Archive).
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
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> impl std::future::Future<Output = Result<u64, CoreError>> + Send;

    /// Decompress and verify CRC without retaining output.
    ///
    /// Equivalent to `extract()` to a no-op sink.
    /// Used for archive integrity checking (`archive-cli test` command).
    fn test(
        &self,
        entry: &ArchiveEntry,
    ) -> impl std::future::Future<Output = Result<(), CoreError>> + Send;

    /// Whether this archive's entry list is ready or requires a scan first.
    /// Default: Ready (entry list was built during open).
    fn scan_info(&self) -> ScanInfo {
        ScanInfo::Ready
    }

    /// Run the metadata scan, populating the entry list.
    /// Default: no-op (entry list was already built during open).
    /// Implementors that override this should call progress callbacks.
    fn scan_metadata(
        &mut self,
        _progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> impl std::future::Future<Output = Result<(), CoreError>> + Send {
        async { Ok(()) }
    }

    /// Extract entries in a single streaming pass.
    ///
    /// Scans the archive linearly, populating `entries()` as a side effect
    /// (like `scan_metadata`), while simultaneously extracting entries that
    /// pass `filter` via `sink_factory`.
    ///
    /// - **Local archives:** delegates to the existing `unrar` direct path.
    /// - **Cloud archives:** uses the pipelined streaming splitter (one HTTP
    ///   stream, per-entry snippet micro-archives, bounded-channel backpressure).
    ///
    /// Returns total uncompressed bytes written.
    ///
    /// Default: returns `UnsupportedFeature` (ZIP has random access).
    fn extract_all_streaming<Filter, SinkFn>(
        &mut self,
        filter: Filter,
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> impl std::future::Future<Output = Result<u64, CoreError>> + Send
    where
        Filter: Fn(&ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        async move {
            let _ = (filter, sink_factory, progress, password);
            Err(CoreError::UnsupportedFeature {
                detail: "streaming extraction not supported for this format".into(),
            })
        }
    }
}
