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

/// Result of a batch extraction operation.
///
/// Unlike single `extract()` which returns `Result<u64, CoreError>`,
/// batch extraction continues past per-entry failures (CRC mismatch,
/// decompression errors) and collects them here. Only unrecoverable
/// errors (cancellation, I/O transport) cause an `Err` return.
#[derive(Debug)]
pub struct BatchResult {
    /// Total uncompressed bytes successfully written.
    pub bytes_written: u64,
    /// Number of entries successfully extracted.
    pub succeeded: usize,
    /// Entries that failed extraction: (entry_name, error_message).
    /// Error is stringified at collection time — failures are informational
    /// for display, not for programmatic routing (see ERRORS.md).
    pub failed: Vec<(String, String)>,
}

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

    /// Compression dictionary size in bytes for this entry, if known.
    ///
    /// RAR 7.x supports dictionaries up to 64 GB — the decompressor allocates
    /// this much RAM. Returns `Some(bytes)` for RAR5, `None` for formats
    /// where the dictionary is fixed/small (ZIP, RAR4).
    fn dict_size(&self, _entry: &ArchiveEntry) -> Option<u64> {
        None
    }

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
        progress: Arc<Mutex<dyn ProgressReporter>>,
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

    /// Extract multiple entries in a single batch operation.
    ///
    /// Sorts entries by physical offset and coalesces nearby entries into
    /// clustered I/O operations to minimize HTTP requests for cloud archives.
    ///
    /// `sink_factory` is called once per non-directory entry — return
    /// `Some(writer)` to extract, `None` to skip.
    ///
    /// Per-entry failures are collected in `BatchResult.failed` — the loop
    /// continues. Returns `Err` only for unrecoverable errors (cancellation).
    ///
    /// Default: loops `extract()` per entry (correct for local files).
    /// Format parsers override with optimized coalescing.
    fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> impl std::future::Future<Output = Result<BatchResult, CoreError>> + Send
    where
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>
            + Send,
        Self: Send + Sync + Sized,
    {
        extract_selected_default(self, entries, sink_factory, progress, password)
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

/// Default `extract_selected` implementation — loop of `extract()` calls.
///
/// Public so format parsers can delegate to it when they don't need
/// optimization (e.g., local file extraction).
pub async fn extract_selected_default<R, SinkFn>(
    reader: &R,
    entries: &[&ArchiveEntry],
    mut sink_factory: SinkFn,
    progress: Arc<Mutex<dyn ProgressReporter>>,
    password: Option<&[u8]>,
) -> Result<BatchResult, CoreError>
where
    R: ArchiveReader + Send + Sync,
    SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> + Send,
{
    let mut bytes_written = 0u64;
    let mut succeeded = 0usize;
    let mut failed = Vec::new();

    for &entry in entries {
        if entry.is_directory {
            continue;
        }

        // Check cancellation between entries.
        {
            let p = progress.lock().unwrap();
            if p.should_cancel() {
                return Err(CoreError::Cancelled);
            }
        }

        let mut sink = match sink_factory(entry) {
            Some(s) => s,
            None => continue,
        };

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry.name, entry.uncompressed_size);
        }

        match reader.extract(entry, &mut sink, progress.clone(), password).await {
            Ok(bytes) => {
                bytes_written += bytes;
                succeeded += 1;
                let mut p = progress.lock().unwrap();
                p.on_entry_complete(&entry.name, true);
            }
            Err(CoreError::Cancelled) => {
                // Cancellation is unrecoverable — propagate immediately.
                return Err(CoreError::Cancelled);
            }
            Err(e) => {
                // Per-entry failure — log, report, continue.
                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_complete(&entry.name, false);
                }
                failed.push((entry.name.clone(), e.to_string()));
            }
        }
    }

    Ok(BatchResult { bytes_written, succeeded, failed })
}
