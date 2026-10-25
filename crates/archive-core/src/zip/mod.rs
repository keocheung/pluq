//! ZIP archive reader — built on top of [`ReadAt`].
//!
//! ```ignore
//! let reader = LocalReadAt::new("archive.zip");
//! let zip = ZipArchive::open(reader, &crate::ResourceLimits::default()).await?;
//!
//! for entry in zip.entries() {
//!     println!("{} ({} bytes)", entry.name, entry.uncompressed_size);
//! }
//!
//! let mut sink = tokio::io::sink();
//! zip.extract(&zip.entries()[0], &mut sink).await?;
//! ```

mod aes;
mod crypto;
pub mod index;
mod parse;

use crypto::{ZipCryptoDecryptor, ZipCryptoReader};
pub use index::ZipIndex;
pub use parse::{AesExtraInfo, ZipEntryLocation};

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWrite;
use tokio_util::io::SyncIoBridge;

use tracing::{debug, info, trace, warn};

use crate::archive_reader::{ArchiveReader, BatchResult};
use crate::codec::{CompressionMethod, build_decoder};
use crate::io::{FetchRegion, build_clusters};
use crate::junk;
use crate::progress::ProgressReporter;
use crate::{ArchiveEntry, CoreError, ReadAt};

struct DecompressParams<'a> {
    entry_name: &'a str,
    compressed_size: u64,
    method_id: u16,
    aes_info: Option<AesExtraInfo>,
    expected_crc: u32,
    mod_time: u16,
    is_encrypted: bool,
    password: Option<&'a [u8]>,
    progress: &'a Arc<Mutex<dyn ProgressReporter>>,
    limits: &'a crate::ResourceLimits,
}

/// A parsed ZIP archive backed by any [`ReadAt`] source.
///
/// Created via [`ZipArchive::open`], which reads the EOCD and Central
/// Directory to build the file listing. Extraction calls issue additional
/// targeted range reads.
///
/// # Internal layout
///
/// Holds two parallel `Vec`s at the same indices:
/// - `entries` — public, format-agnostic [`ArchiveEntry`] metadata
/// - `locations` — private [`ZipEntryLocation`] with byte offsets, method IDs, CRC values
///
/// `entry.index` is the key from `entries` into `locations`.
pub struct ZipArchive<R: ReadAt> {
    reader: R,
    entries: Vec<ArchiveEntry>,
    locations: Vec<ZipEntryLocation>,
}

impl<R: ReadAt> ZipArchive<R> {
    /// Open a ZIP archive by reading its EOCD and Central Directory.
    ///
    /// This issues exactly **2 range reads** (or 1 if the file is small).
    pub async fn open(reader: R, limits: &crate::ResourceLimits) -> Result<Self, CoreError> {
        Self::open_with_encoding(reader, limits, crate::FilenameEncoding::default()).await
    }

    /// Open with an optional forced filename encoding, including for UTF-8 flagged names.
    pub async fn open_with_encoding(
        reader: R,
        limits: &crate::ResourceLimits,
        filename_encoding: crate::FilenameEncoding,
    ) -> Result<Self, CoreError> {
        info!("opening ZIP archive");
        let eocd = parse::find_eocd(&reader).await?;

        if eocd.cd_size > limits.max_header_bytes {
            return Err(CoreError::ResourceLimitExceeded {
                message: format!(
                    "ZIP central directory size {} exceeds max_header_bytes {}",
                    eocd.cd_size, limits.max_header_bytes
                ),
            });
        }

        if eocd.entry_count as usize > limits.max_entries {
            return Err(CoreError::ResourceLimitExceeded {
                message: format!(
                    "ZIP entry count {} exceeds max_entries {}",
                    eocd.entry_count, limits.max_entries
                ),
            });
        }

        debug!(
            cd_offset = eocd.cd_offset,
            cd_size = eocd.cd_size,
            entry_count = eocd.entry_count,
            "EOCD found"
        );
        let (entries, locations) =
            parse::parse_central_directory(&reader, &eocd, filename_encoding).await?;
        info!(entries = entries.len(), "ZIP archive opened");

        Ok(Self {
            reader,
            entries,
            locations,
        })
    }

    /// Return all entries (files and directories) in the archive, including
    /// OS-generated junk (`.DS_Store`, `__MACOSX/`, `Thumbs.db`, etc.).
    ///
    /// Prefer [`entries_clean`](Self::entries_clean) for user-facing output.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Return entries with OS-generated junk files filtered out.
    ///
    /// This is the **default** for user-facing output (CLI, UI).
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries.iter().filter(|e| !junk::is_junk(e)).collect()
    }

    /// Per-entry location data, parallel to `entries()`. Used by the cache layer.
    pub fn locations(&self) -> &[ZipEntryLocation] {
        &self.locations
    }

    /// Reconstruct a `ZipArchive` from cached data, skipping all I/O.
    pub fn from_cached(
        reader: R,
        entries: Vec<ArchiveEntry>,
        locations: Vec<ZipEntryLocation>,
    ) -> Self {
        Self {
            reader,
            entries,
            locations,
        }
    }

    /// Bytes that must be downloaded and decompressed to produce this entry.
    ///
    /// For ZIP every entry is independently seekable, so this is always
    /// `entry.compressed_size`.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        entry.compressed_size
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// CRC verification is mandatory and structural — it is the primary
    /// corruption and wrong-password detection mechanism.
    ///
    /// Follows the mandated extraction pattern (ARCHITECTURE.md §Async/Sync Boundary):
    /// 1. **async**: `read_range()` for LFH header (small, bounded)
    /// 2. **async**: `read_stream()` → compressed `AsyncRead` (could be GBs)
    /// 3. **`spawn_blocking`**: `SyncIoBridge` wraps stream → sync `Read`;
    ///    decompress via `build_decoder`, CRC-verify, write to sync side of bridge
    /// 4. **async**: flush sink
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        if entry.is_directory {
            return Ok(0);
        }

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry.name, entry.uncompressed_size);
        }

        let progress_clone = progress.clone();
        let result = async {
            limits.check_extraction_limits(
                &entry.name,
                entry.compressed_size,
                entry.uncompressed_size,
                &progress_clone,
                false,
                false,
            )?;

            info!(
                name = %entry.name,
                compressed = entry.compressed_size,
                uncompressed = entry.uncompressed_size,
                encrypted = entry.is_encrypted,
                "ZIP extract start"
            );

            if entry.is_encrypted && password.is_none() {
                debug!(name = %entry.name, "password required but not provided");
                return Err(CoreError::PasswordRequired);
            }

            let location = &self.locations[entry.index];

            // Extract ZIP-specific fields before the async boundary so the
            // spawn_blocking closure can own them without borrowing `self`.
            let location_method_id = location.method_id;
            let location_mod_time = location.mod_time;
            let location_aes_info: Option<AesExtraInfo> = location.aes_info.clone();

            // ── Step 1 (async): resolve data offset via Local File Header ─────
            // LFH is 30 bytes — small, use read_range.
            let data_offset = parse::resolve_data_offset(&self.reader, location)
                .await
                .map_err(|e| match e {
                    // LFH signature mismatch — structural corruption
                    crate::io::ReadError::Transport(msg) => CoreError::InvalidFormat {
                        message: msg.to_string(),
                    },
                    other => CoreError::Io(other),
                })?;

            let compressed_size = entry.compressed_size;

            // ── Step 2 (async): get streaming AsyncRead for compressed data ────
            // read_stream returns a live byte stream — no buffering of multi-GB data.
            let compressed_stream = self
                .reader
                .read_stream(data_offset, compressed_size)
                .await
                .map_err(CoreError::Io)?;

            // ── Step 3 (spawn_blocking): decompress + CRC verify ─────────────
            // SyncIoBridge wraps the AsyncRead into a sync Read by driving the
            // async polling from the current runtime handle.
            let entry_name = entry.name.clone();
            let expected_crc = location.crc32;
            let is_encrypted = entry.is_encrypted;
            let password_bytes: Option<Vec<u8>> = password.map(|p| p.to_vec());
            let handle = tokio::runtime::Handle::current();
            let sync_reader = SyncIoBridge::new_with_handle(compressed_stream, handle);
            let limits_clone = limits.clone();

            let temp_file: tempfile::NamedTempFile = tokio::task::spawn_blocking(
                move || -> Result<tempfile::NamedTempFile, CoreError> {
                    use std::io::Write;

                    let temp_file =
                        tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                            detail: format!("failed to create temp file: {e}"),
                        })?;

                    {
                        let mut temp_writer = std::io::BufWriter::new(temp_file.as_file());
                        let limits = &limits_clone;
                        decompress_and_verify(
                            sync_reader,
                            &mut temp_writer,
                            DecompressParams {
                                entry_name: &entry_name,
                                compressed_size,
                                method_id: location_method_id,
                                mod_time: location_mod_time,
                                aes_info: location_aes_info,
                                expected_crc,
                                is_encrypted,
                                password: password_bytes.as_deref(),
                                progress: &progress_clone,
                                limits,
                            },
                        )?;
                        temp_writer.flush().map_err(|e| CoreError::BitstreamError {
                            detail: format!("temp file flush failed: {e}"),
                        })?;
                    }

                    Ok(temp_file)
                },
            )
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("decompression task panicked: {e}"),
            })??;

            // ── Step 4 (async): stream verified temp file to sink ──────────────
            let mut temp_reader = tokio::fs::File::open(temp_file.path()).await.map_err(|e| {
                CoreError::BitstreamError {
                    detail: format!("failed to reopen temp file: {e}"),
                }
            })?;

            let bytes_written = tokio::io::copy(&mut temp_reader, sink).await.map_err(|e| {
                CoreError::BitstreamError {
                    detail: format!("copy to sink failed: {e}"),
                }
            })?;

            info!(name = %entry.name, bytes_written, "ZIP extract complete");
            Ok(bytes_written)
        }
        .await;

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_complete(&entry.name, result.is_ok());
        }

        result
    }

    /// Decompress and verify CRC without writing output anywhere.
    ///
    /// Equivalent to [`extract`](Self::extract) to a no-op async sink.
    /// Used for archive integrity checking.
    pub async fn test(
        &self,
        entry: &ArchiveEntry,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<(), CoreError> {
        self.extract(entry, &mut tokio::io::sink(), progress, None, limits)
            .await
            .map(|_| ())
    }

    /// Extract multiple entries in a single streaming pass.
    ///
    /// Opens **one** `read_stream` covering all requested entries (sorted by
    /// physical offset), then processes each entry sequentially from that single
    /// stream. This reduces HTTP round trips from O(2N) to O(1).
    ///
    /// `sink_factory` is called once per entry (in archive physical order) from
    /// a blocking context. Return `Some(writer)` to extract, `None` to skip.
    /// Directories are always skipped.
    pub async fn extract_all<F>(
        &self,
        entry_indices: &[usize],
        sink_factory: F,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError>
    where
        F: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        // Build work list sorted by physical LFH offset.
        let mut work: Vec<(ArchiveEntry, ZipEntryLocation)> = entry_indices
            .iter()
            .filter(|&&idx| !self.entries[idx].is_directory)
            .map(|&idx| (self.entries[idx].clone(), self.locations[idx].clone()))
            .collect();
        work.sort_by_key(|(_, loc)| loc.offset);

        if work.is_empty() {
            return Ok(0);
        }

        // Compute stream range: first LFH to end of file.
        let stream_start = work[0].1.offset;
        let file_size = self.reader.size().await.map_err(CoreError::Io)?;
        let stream_len = file_size.saturating_sub(stream_start);

        info!(
            entries = work.len(),
            stream_start, stream_len, "ZIP extract_all: opening single stream"
        );

        // One stream for the entire body.
        let stream = self
            .reader
            .read_stream(stream_start, stream_len)
            .await
            .map_err(CoreError::Io)?;

        let handle = tokio::runtime::Handle::current();
        let password_bytes: Option<Vec<u8>> = password.map(|p| p.to_vec());
        let limits_clone = limits.clone();

        // Everything runs in spawn_blocking — sync I/O for both read and write.
        tokio::task::spawn_blocking(move || -> Result<u64, CoreError> {
            let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);
            let mut sink_factory = sink_factory;
            let mut stream_pos = stream_start;
            let mut total_bytes: u64 = 0;
            let limits = &limits_clone;

            for (entry, location) in &work {
                // ── Skip gap bytes to reach this entry's LFH ─────────────
                let gap = location.offset.saturating_sub(stream_pos);
                if gap > 0 {
                    skip_exact(&mut sync_reader, gap)?;
                    stream_pos += gap;
                }

                // ── Parse Local File Header (30 bytes) ───────────────────
                let mut lfh = [0u8; 30];
                sync_reader
                    .read_exact(&mut lfh)
                    .map_err(|e| stream_io_err(e, stream_pos))?;
                stream_pos += 30;

                let sig = u32::from_le_bytes([lfh[0], lfh[1], lfh[2], lfh[3]]);
                if sig != 0x04034b50 {
                    return Err(CoreError::InvalidFormat {
                        message: format!(
                            "invalid LFH signature at offset {:#x}: {sig:#010x}",
                            location.offset
                        ),
                    });
                }
                let name_len = u16::from_le_bytes([lfh[26], lfh[27]]) as u64;
                let extra_len = u16::from_le_bytes([lfh[28], lfh[29]]) as u64;
                let var_len = name_len + extra_len;
                if var_len > 0 {
                    skip_exact(&mut sync_reader, var_len)?;
                    stream_pos += var_len;
                }

                // ── Create sink (or skip this entry) ─────────────────────
                let compressed_size = entry.compressed_size;

                if let Some(mut sink) = sink_factory(entry) {
                    {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_start(&entry.name, entry.uncompressed_size);
                    }

                    let result: Result<u64, CoreError> = (|| {
                        // Read compressed data into memory, then decompress to a temp file.
                        // This eliminates the O(uncompressed_size) RAM requirement.
                        let mut compressed = vec![0u8; compressed_size as usize];
                        sync_reader
                            .read_exact(&mut compressed)
                            .map_err(|e| stream_io_err(e, stream_pos))?;
                        stream_pos += compressed_size;

                        let temp_file = tempfile::NamedTempFile::new().map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("failed to create temp file: {e}"),
                            }
                        })?;

                        let bytes_written = {
                            let mut temp_writer = std::io::BufWriter::new(temp_file.as_file());
                            let written = decompress_and_verify(
                                std::io::Cursor::new(compressed),
                                &mut temp_writer,
                                DecompressParams {
                                    entry_name: &entry.name,
                                    compressed_size,
                                    method_id: location.method_id,
                                    mod_time: location.mod_time,
                                    aes_info: location.aes_info.clone(),
                                    expected_crc: location.crc32,
                                    is_encrypted: entry.is_encrypted,
                                    password: password_bytes.as_deref(),
                                    progress: &progress,
                                    limits,
                                },
                            )?;
                            temp_writer.flush().map_err(|e| CoreError::BitstreamError {
                                detail: format!("temp file flush failed: {e}"),
                            })?;
                            written
                        };

                        // Copy verified output from temp to sink.
                        let mut temp_reader =
                            std::fs::File::open(temp_file.path()).map_err(|e| {
                                CoreError::BitstreamError {
                                    detail: format!("failed to reopen temp file: {e}"),
                                }
                            })?;
                        std::io::copy(&mut temp_reader, &mut sink).map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("copy to sink failed for '{}': {e}", entry.name),
                            }
                        })?;

                        Ok(bytes_written)
                    })();

                    {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_complete(&entry.name, result.is_ok());
                    }

                    total_bytes += result?;
                } else {
                    // Skip compressed data.
                    skip_exact(&mut sync_reader, compressed_size)?;
                    stream_pos += compressed_size;
                }
            }

            Ok(total_bytes)
        })
        .await
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("extract_all task panicked: {e}"),
        })?
    }

    /// Extract multiple entries using stream coalescing for efficiency.
    ///
    /// Sorts entries by physical offset, clusters nearby entries within
    /// [`crate::io::COALESCE_GAP`] (with [`crate::io::MAX_WASTE_RATIO`] guard),
    /// then opens **one `read_stream` per cluster** instead of one per entry.
    /// This reduces HTTP round trips from O(2N) to O(M) where M = number of clusters.
    ///
    /// `sink_factory` is called once per entry (in archive physical order)
    /// from async context. Return `Some(writer)` to extract, `None` to skip.
    /// Directories are always skipped.
    ///
    /// Per-entry failures (CRC mismatch, decompression, wrong password) are
    /// collected in `BatchResult.failed` — the loop continues. Returns `Err`
    /// only for unrecoverable stream-level errors.
    pub async fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        mut sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<BatchResult, CoreError>
    where
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn AsyncWrite + Send + Unpin>> + Send,
    {
        let total_size: u64 = entries.iter().map(|e| e.uncompressed_size).sum();
        if total_size > limits.max_total_unpacked {
            return Err(CoreError::ResourceLimitExceeded {
                message: format!(
                    "batch extraction total size {} exceeds max_total_unpacked {}",
                    total_size, limits.max_total_unpacked
                ),
            });
        }

        // 1. Filter directories, map to (entry, location) pairs.
        let work: Vec<(&ArchiveEntry, &ZipEntryLocation)> = entries
            .iter()
            .filter_map(|&e| {
                if e.is_directory {
                    None
                } else {
                    Some((e, &self.locations[e.index]))
                }
            })
            .collect();

        if work.is_empty() {
            return Ok(BatchResult {
                bytes_written: 0,
                succeeded: 0,
                failed: vec![],
            });
        }

        // 2. Map to FetchRegion and cluster.
        let regions: Vec<FetchRegion<(&ArchiveEntry, &ZipEntryLocation)>> = work
            .into_iter()
            .map(|(e, loc)| {
                let total_len = 30u64
                    .saturating_add(e.name.len() as u64)
                    .saturating_add(1024)
                    .saturating_add(e.compressed_size);
                FetchRegion {
                    offset: loc.offset,
                    total_len,
                    payload: (e, loc),
                }
            })
            .collect();

        let clusters = build_clusters(regions);

        let file_size = self.reader.size().await.map_err(CoreError::Io)?;
        let password_bytes: Option<Vec<u8>> = password.map(|p| p.to_vec());
        let mut batch = BatchResult {
            bytes_written: 0,
            succeeded: 0,
            failed: vec![],
        };

        for cluster in &clusters {
            // Cancel check between clusters.
            {
                let p = progress.lock().unwrap();
                if p.should_cancel() {
                    return Err(CoreError::Cancelled);
                }
            }

            let first_region = &cluster[0];
            let last_region = &cluster[cluster.len() - 1];

            let cluster_start = first_region.offset;
            let cluster_end = last_region
                .offset
                .saturating_add(last_region.total_len)
                .min(file_size);
            let cluster_len = cluster_end.saturating_sub(cluster_start);

            info!(
                cluster_entries = cluster.len(),
                cluster_start, cluster_len, "ZIP extract_selected: opening cluster stream"
            );

            let stream = self
                .reader
                .read_stream(cluster_start, cluster_len)
                .await
                .map_err(CoreError::Io)?;

            // Owned copies for spawn_blocking.
            let cluster_data: Vec<(ArchiveEntry, ZipEntryLocation)> = cluster
                .iter()
                .map(|r| ((*r.payload.0).clone(), (*r.payload.1).clone()))
                .collect();
            let pw = password_bytes.clone();
            let prog = progress.clone();
            let lims = limits.clone();
            let handle = tokio::runtime::Handle::current();
            let sync_reader = SyncIoBridge::new_with_handle(stream, handle);

            // Decompress all entries in this cluster from a single stream into temp files.
            let decompressed: Vec<Result<tempfile::NamedTempFile, CoreError>> =
                tokio::task::spawn_blocking(move || {
                    decompress_cluster(sync_reader, cluster_start, &cluster_data, pw, prog, lims)
                })
                .await
                .map_err(|e| CoreError::BitstreamError {
                    detail: format!("extract_selected cluster task panicked: {e}"),
                })??;

            // Write results asynchronously and accumulate BatchResult.
            for (result, region) in decompressed.into_iter().zip(cluster.iter()) {
                let (entry, _) = region.payload;
                match result {
                    Ok(temp_file) => {
                        if let Some(mut sink) = sink_factory(entry) {
                            let copy_result = async {
                                let mut temp_reader = tokio::fs::File::open(temp_file.path())
                                    .await
                                    .map_err(|e| format!("failed to reopen temp: {e}"))?;
                                let written = tokio::io::copy(&mut temp_reader, &mut sink)
                                    .await
                                    .map_err(|e| format!("write failed: {e}"))?;
                                Ok::<u64, String>(written)
                            }
                            .await;

                            match copy_result {
                                Ok(written) => {
                                    batch.bytes_written += written;
                                    batch.succeeded += 1;
                                }
                                Err(msg) => {
                                    batch.failed.push((entry.name.clone(), msg));
                                }
                            }
                        }
                        // temp_file dropped here → auto-deleted
                    }
                    Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                    Err(e) => {
                        batch.failed.push((entry.name.clone(), e.to_string()));
                    }
                }
            }
        }

        Ok(batch)
    }
}

impl<R: ReadAt + Send + Sync + 'static> ArchiveReader for ZipArchive<R> {
    fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        entry.compressed_size
    }

    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        ZipArchive::extract(self, entry, sink, progress, password, limits).await
    }

    async fn test(
        &self,
        entry: &ArchiveEntry,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<(), CoreError> {
        ZipArchive::test(self, entry, progress, limits).await
    }

    fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> impl std::future::Future<Output = Result<BatchResult, CoreError>> + Send
    where
        SinkFn:
            FnMut(&ArchiveEntry) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> + Send,
    {
        ZipArchive::extract_selected(self, entries, sink_factory, progress, password, limits)
    }
}

// ── Cluster decompressor (runs inside spawn_blocking) ────────────────────────

/// Decompress all entries in a single coalesced cluster stream.
///
/// Returns one `Result<Vec<u8>, CoreError>` per entry **in the same order as
/// `cluster_data`**. Per-entry failures are returned as `Err` items; the loop
/// continues. Only stream-level failures (I/O, bad LFH signature) cause the
/// entire `Result` to be `Err`.
fn decompress_cluster(
    mut stream: SyncIoBridge<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    cluster_start: u64,
    cluster_data: &[(ArchiveEntry, ZipEntryLocation)],
    password: Option<Vec<u8>>,
    progress: Arc<Mutex<dyn ProgressReporter>>,
    limits: crate::ResourceLimits,
) -> Result<Vec<Result<tempfile::NamedTempFile, CoreError>>, CoreError> {
    let mut stream_pos = cluster_start;
    let mut results = Vec::with_capacity(cluster_data.len());

    for (entry, location) in cluster_data {
        // Cancel check between entries.
        {
            let p = progress.lock().unwrap();
            if p.should_cancel() {
                return Err(CoreError::Cancelled);
            }
        }

        // Skip gap bytes to reach this entry's Local File Header.
        let gap = location.offset.saturating_sub(stream_pos);
        if gap > 0 {
            skip_exact(&mut stream, gap)?;
            stream_pos += gap;
        }

        // Parse the 30-byte fixed LFH header.
        let mut lfh = [0u8; 30];
        stream
            .read_exact(&mut lfh)
            .map_err(|e| stream_io_err(e, stream_pos))?;
        stream_pos += 30;

        let sig = u32::from_le_bytes([lfh[0], lfh[1], lfh[2], lfh[3]]);
        if sig != 0x04034b50 {
            return Err(CoreError::InvalidFormat {
                message: format!(
                    "invalid LFH signature at offset {:#x}: {sig:#010x}",
                    location.offset
                ),
            });
        }

        let name_len = u16::from_le_bytes([lfh[26], lfh[27]]) as u64;
        let extra_len = u16::from_le_bytes([lfh[28], lfh[29]]) as u64;
        let var_len = name_len + extra_len;
        if var_len > 0 {
            skip_exact(&mut stream, var_len)?;
            stream_pos += var_len;
        }

        // Read compressed data into memory (including encryption overhead).
        let compressed_size = entry.compressed_size;
        let mut compressed = vec![0u8; compressed_size as usize];
        stream
            .read_exact(&mut compressed)
            .map_err(|e| stream_io_err(e, stream_pos))?;
        stream_pos += compressed_size;

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry.name, entry.uncompressed_size);
        }

        // Decompress + CRC-verify into a temp file (per-entry failure → Err item, loop continues).
        let result: Result<tempfile::NamedTempFile, CoreError> = (|| {
            let temp_file =
                tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                    detail: format!("failed to create temp file: {e}"),
                })?;

            {
                let mut temp_writer = std::io::BufWriter::new(temp_file.as_file());
                decompress_and_verify(
                    std::io::Cursor::new(compressed),
                    &mut temp_writer,
                    DecompressParams {
                        entry_name: &entry.name,
                        compressed_size,
                        method_id: location.method_id,
                        mod_time: location.mod_time,
                        aes_info: location.aes_info.clone(),
                        expected_crc: location.crc32,
                        is_encrypted: entry.is_encrypted,
                        password: password.as_deref(),
                        progress: &progress,
                        limits: &limits,
                    },
                )?;
                temp_writer.flush().map_err(|e| CoreError::BitstreamError {
                    detail: format!("temp file flush failed: {e}"),
                })?;
            }

            Ok(temp_file)
        })();

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_complete(&entry.name, result.is_ok());
        }

        results.push(result);
    }

    Ok(results)
}

/// Read and strip the ZIP LZMA header, returning the raw LZMA properties.
///
/// ZIP method 14 (LZMA) prepends a 4-byte header before the properties and bitstream:
/// `[2 bytes: LZMA SDK version] [2 bytes: properties size] [props_size bytes: properties]`
///
/// After this call, `reader` is positioned at the start of the raw LZMA bitstream.
pub(crate) fn read_zip_lzma_header(reader: &mut impl Read) -> Result<Vec<u8>, CoreError> {
    let mut header = [0u8; 4];
    reader
        .read_exact(&mut header)
        .map_err(|e| CoreError::InvalidFormat {
            message: format!("truncated LZMA header: {e}"),
        })?;
    let props_size = u16::from_le_bytes([header[2], header[3]]) as usize;
    if !(5..=255).contains(&props_size) {
        return Err(CoreError::InvalidFormat {
            message: format!("invalid LZMA properties size: {props_size}"),
        });
    }
    let mut props = vec![0u8; props_size];
    reader
        .read_exact(&mut props)
        .map_err(|e| CoreError::InvalidFormat {
            message: format!("truncated LZMA properties: {e}"),
        })?;
    Ok(props)
}

// ── Shared helpers for extract() and extract_all() ───────────────────────────

/// Skip exactly `n` bytes from a reader by reading and discarding.
fn skip_exact(reader: &mut impl Read, n: u64) -> Result<(), CoreError> {
    let mut remaining = n;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        reader
            .read_exact(&mut buf[..to_read])
            .map_err(|e| stream_io_err(e, 0))?;
        remaining -= to_read as u64;
    }
    Ok(())
}

/// Convert a stream `io::Error` to `CoreError`, recovering embedded `ReadError`
/// from the `SyncIoBridge` when present.
fn stream_io_err(e: std::io::Error, offset: u64) -> CoreError {
    if let Some(inner) = e.into_inner()
        && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
    {
        return CoreError::Io(*read_error);
    }
    CoreError::CorruptHeader {
        offset,
        reason: "stream read failed".into(),
    }
}

/// Decompress and CRC-verify one ZIP entry, writing output to a `Write` sink.
///
/// Output flows to the sink chunk-by-chunk with O(1) memory. Returns bytes written.
fn decompress_and_verify<R: Read + Send, W: std::io::Write>(
    mut reader: R,
    sink: &mut W,
    params: DecompressParams<'_>,
) -> Result<u64, CoreError> {
    let is_aes = params.aes_info.is_some() && params.is_encrypted;
    let skip_crc_check = is_aes;

    // ── Decryption + decoder setup ───────────────────────────────────────
    let mut decoder: Box<dyn crate::codec::Decoder + '_> = if is_aes {
        let aes_extra = params.aes_info.as_ref().unwrap();
        let pwd = params.password.ok_or(CoreError::PasswordRequired)?;
        let decrypted =
            aes::decrypt_winzip_aes(&mut reader, pwd, aes_extra.strength, params.compressed_size)?;
        let method = parse::zip_method_to_codec(aes_extra.actual_method_id).ok_or_else(|| {
            CoreError::UnsupportedCompression {
                algorithm: format!("ZIP AES actual method id {}", aes_extra.actual_method_id),
            }
        })?;
        let mut aes_stream = std::io::Cursor::new(decrypted);
        let properties = if method == CompressionMethod::Lzma {
            read_zip_lzma_header(&mut aes_stream)?
        } else {
            vec![]
        };
        build_decoder(
            method,
            aes_stream,
            &properties,
            &crate::limits::CodecConfig::default(),
        )?
    } else if params.is_encrypted {
        let pwd = params.password.ok_or(CoreError::PasswordRequired)?;
        let mut enc_header = [0u8; 12];
        reader.read_exact(&mut enc_header).map_err(|io_err| {
            if let Some(inner) = io_err.into_inner()
                && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
            {
                return CoreError::Io(*read_error);
            }
            CoreError::InvalidFormat {
                message: format!("truncated encryption header for '{}'", params.entry_name),
            }
        })?;
        let mut decryptor = ZipCryptoDecryptor::new(pwd);
        decryptor.decrypt(&mut enc_header);

        // Verification byte check (the 12th byte of the header).
        // Traditionally, this is the high byte of the CRC32.
        // However, if the archive uses a Data Descriptor (Bit 3 set), the CRC isn't
        // known at write time, so the high byte of the DOS last-mod-time is used instead.
        // Some modern writers (like zip-rs v8) seem to prefer the time even when
        // the CRC is known.
        let crc_hi = (params.expected_crc >> 24) as u8;
        let time_hi = (params.mod_time >> 8) as u8;

        if enc_header[11] != crc_hi && enc_header[11] != time_hi {
            debug!(
                "ZipCrypto verification failed for '{}': header[11]={:02x}, crc_hi={:02x}, time_hi={:02x}",
                params.entry_name, enc_header[11], crc_hi, time_hi
            );
            return Err(CoreError::WrongPassword {
                method: crate::PasswordDetectionMethod::EarlyHeaderValidation,
            });
        }
        let method = parse::zip_method_to_codec(params.method_id).ok_or_else(|| {
            CoreError::UnsupportedCompression {
                algorithm: format!("ZIP method id {}", params.method_id),
            }
        })?;
        let mut crypto_reader = ZipCryptoReader::new(reader, decryptor);
        let properties = if method == CompressionMethod::Lzma {
            read_zip_lzma_header(&mut crypto_reader)?
        } else {
            vec![]
        };
        build_decoder(
            method,
            crypto_reader,
            &properties,
            &crate::limits::CodecConfig::default(),
        )?
    } else {
        let method = parse::zip_method_to_codec(params.method_id).ok_or_else(|| {
            CoreError::UnsupportedCompression {
                algorithm: format!("ZIP method id {}", params.method_id),
            }
        })?;
        let properties = if method == CompressionMethod::Lzma {
            read_zip_lzma_header(&mut reader)?
        } else {
            vec![]
        };
        build_decoder(
            method,
            reader,
            &properties,
            &crate::limits::CodecConfig::default(),
        )?
    };

    // ── Decompress + CRC verify ──────────────────────────────────────────
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut ratio_warned = false;

    let mut hasher = crc32fast::Hasher::new();
    let mut total_written: u64 = 0;
    let mut chunk_buf = vec![0u8; CHUNK_SIZE];

    loop {
        let n = decoder.read(&mut chunk_buf).map_err(|io_err| {
            if let Some(inner) = io_err.into_inner()
                && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
            {
                return CoreError::Io(*read_error);
            }
            CoreError::BitstreamError {
                detail: format!("decompression failed for '{}'", params.entry_name),
            }
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk_buf[..n]);
        sink.write_all(&chunk_buf[..n])
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("write failed for '{}': {e}", params.entry_name),
            })?;
        total_written += n as u64;

        if !ratio_warned {
            ratio_warned = params.limits.check_extraction_limits(
                params.entry_name,
                params.compressed_size,
                total_written,
                params.progress,
                true,
                false,
            )?;
        } else {
            // Already warned about ratio, but still check max_entry_unpacked.
            // Pass skip_warn=true to suppress additional warnings.
            params.limits.check_extraction_limits(
                params.entry_name,
                params.compressed_size,
                total_written,
                params.progress,
                true,
                true,
            )?;
        }

        {
            let mut p = params.progress.lock().unwrap();
            p.on_bytes_progress(total_written);
            if p.should_cancel() {
                return Err(CoreError::Cancelled);
            }
        }
    }

    if !skip_crc_check {
        let actual_crc = hasher.finalize();
        trace!(
            entry = %params.entry_name,
            expected = format_args!("{:#010x}", params.expected_crc),
            actual = format_args!("{actual_crc:#010x}"),
            "CRC32 verification"
        );
        if actual_crc != params.expected_crc {
            warn!(
                entry = %params.entry_name,
                expected = format_args!("{:#010x}", params.expected_crc),
                actual = format_args!("{actual_crc:#010x}"),
                "CRC32 mismatch"
            );
            return Err(CoreError::CrcMismatch {
                entry: params.entry_name.to_owned(),
                expected: params.expected_crc,
                actual: actual_crc,
            });
        }
    }
    Ok(total_written)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use zip::CompressionMethod as ZipCrateMethod;
    use zip::unstable::write::FileOptionsExt;
    use zip::write::SimpleFileOptions;

    use crate::PasswordDetectionMethod;
    use crate::codec::CompressionMethod;
    use crate::io::LocalReadAt;

    use super::*;

    /// Helper: create a real ZIP file in a temp file, return a `LocalReadAt`.
    fn create_test_zip(
        files: &[(&str, &[u8], ZipCrateMethod)],
    ) -> (tempfile::NamedTempFile, LocalReadAt) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data, method) in files {
                let options = SimpleFileOptions::default().compression_method(*method);
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            let buf = writer.finish().unwrap().into_inner();
            std::fs::write(tmp.path(), &buf).unwrap();
        }
        let reader = LocalReadAt::new(tmp.path());
        (tmp, reader)
    }

    /// Helper: extract entry to a Vec<u8> (convenience for tests, no password).
    async fn extract_to_vec(zip: &ZipArchive<LocalReadAt>, entry: &ArchiveEntry) -> Vec<u8> {
        let mut buf = Vec::new();
        zip.extract(
            entry,
            &mut buf,
            Arc::new(Mutex::new(())),
            None,
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();
        buf
    }

    #[tokio::test]
    async fn list_files_in_zip() {
        let (_tmp, reader) = create_test_zip(&[
            ("hello.txt", b"Hello, world!", ZipCrateMethod::Deflated),
            (
                "data/numbers.bin",
                b"\x01\x02\x03\x04",
                ZipCrateMethod::Stored,
            ),
        ]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let entries = zip.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].uncompressed_size, 13);
        assert!(!entries[0].is_directory);

        assert_eq!(entries[1].name, "data/numbers.bin");
        assert_eq!(entries[1].uncompressed_size, 4);
    }

    #[tokio::test]
    async fn extract_deflated_file() {
        let content = b"pluq is awesome! Compressing this text with Deflate.";
        let (_tmp, reader) =
            create_test_zip(&[("compressed.txt", content, ZipCrateMethod::Deflated)]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(data.as_slice(), content);
    }

    #[tokio::test]
    async fn extract_stored_file() {
        let content = b"\x00\x01\x02\x03\x04\x05\x06\x07";
        let (_tmp, reader) = create_test_zip(&[("raw.bin", content, ZipCrateMethod::Stored)]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(data.as_slice(), content);
    }

    #[tokio::test]
    async fn handles_empty_zip() {
        let (_tmp, reader) = create_test_zip(&[]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert!(zip.entries().is_empty());
    }

    #[tokio::test]
    async fn compression_method_field_is_set() {
        let (_tmp, reader) = create_test_zip(&[
            ("deflated.txt", b"hello", ZipCrateMethod::Deflated),
            ("stored.bin", b"hello", ZipCrateMethod::Stored),
        ]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let entries = zip.entries();
        assert_eq!(
            entries[0].compression_method,
            Some(CompressionMethod::Deflate)
        );
        assert_eq!(
            entries[1].compression_method,
            Some(CompressionMethod::Store)
        );
    }

    #[tokio::test]
    async fn modified_at_is_parsed() {
        let (_tmp, reader) = create_test_zip(&[("file.txt", b"data", ZipCrateMethod::Stored)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        // The zip crate writes a real timestamp, so modified_at should be Some.
        assert!(zip.entries()[0].modified_at.is_some());
    }

    #[tokio::test]
    async fn test_passes_on_valid_entry() {
        let content = b"integrity check content";
        let (_tmp, reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        zip.test(
            &zip.entries()[0],
            Arc::new(Mutex::new(())),
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn extract_returns_bytes_written() {
        let content = b"counting bytes";
        let (_tmp, reader) = create_test_zip(&[("count.txt", content, ZipCrateMethod::Stored)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let mut sink = Vec::new();
        let bytes_written = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &crate::ResourceLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(bytes_written, content.len() as u64);
        assert_eq!(sink, content);
    }

    #[tokio::test]
    async fn crc_mismatch_detected() {
        // Build a valid ZIP and corrupt one byte in the compressed data stream.
        let content = b"This is some content that will be Deflated and then corrupted.";
        let (_tmp, _reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Deflated)]);

        // Read the raw ZIP bytes, flip a byte deep in the compressed data section.
        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        // The compressed data starts after the Local File Header (at least 30 + name bytes).
        // Flip a byte somewhere in the middle of the file, well past the headers.
        let mid = raw.len() / 2;
        raw[mid] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &crate::ResourceLimits::default(),
            )
            .await;

        // Should be either CrcMismatch or an InvalidFormat decompression error
        assert!(
            matches!(result, Err(CoreError::CrcMismatch { .. }))
                || matches!(result, Err(CoreError::BitstreamError { .. }))
                || matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected CrcMismatch, BitstreamError or InvalidFormat, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn encrypted_entry_returns_password_required() {
        // Strategy: write a normal stored ZIP, then flip GP bit 0 in the Central
        // Directory to simulate an encrypted entry — no AES dep needed.
        let content = b"very secret data";
        let (_tmp, _reader) = create_test_zip(&[("secret.txt", content, ZipCrateMethod::Stored)]);

        // Find the CD entry signature [50 4b 01 02] and set GP bit 0 (encrypted).
        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        let cd_sig = [0x50u8, 0x4b, 0x01, 0x02];
        if let Some(pos) = raw.windows(4).position(|w| w == cd_sig) {
            raw[pos + 8] |= 0x01; // GP flags offset = +8 from CD signature
        } else {
            panic!("could not locate Central Directory signature in test ZIP");
        }
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2, &crate::ResourceLimits::default())
            .await
            .unwrap();

        assert!(
            zip.entries()[0].is_encrypted,
            "parser must flag entry as encrypted when GP bit 0 is set"
        );

        // Must fail immediately with PasswordRequired — before any decompression attempt.
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &crate::ResourceLimits::default(),
            )
            .await;
        assert!(
            matches!(result, Err(CoreError::PasswordRequired)),
            "expected PasswordRequired, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn entries_clean_filters_os_junk() {
        let (_tmp, reader) = create_test_zip(&[
            ("real_file.txt", b"content", ZipCrateMethod::Stored),
            (".DS_Store", b"mac junk", ZipCrateMethod::Stored),
            (
                "__MACOSX/._real_file.txt",
                b"mac sidecar",
                ZipCrateMethod::Stored,
            ),
        ]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert_eq!(zip.entries().len(), 3, "entries() must return all 3");

        let clean = zip.entries_clean();
        assert_eq!(
            clean.len(),
            1,
            "entries_clean() must filter out 2 junk files"
        );
        assert_eq!(clean[0].name, "real_file.txt");
    }

    #[tokio::test]
    async fn unsupported_method_parses_but_errors_on_extract() {
        // Write a valid stored ZIP, then patch the method ID in both the
        // Central Directory (offset +10) and the Local File Header (offset +8)
        // to 98 (PPMd), which is recognised in metadata but not compiled in.
        let content = b"some data";
        let (_tmp, _reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Stored)]);

        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();

        // Patch Local File Header method (sig [50 4b 03 04], method at +8).
        let lfh_sig = [0x50u8, 0x4b, 0x03, 0x04];
        if let Some(pos) = raw.windows(4).position(|w| w == lfh_sig) {
            raw[pos + 8] = 98; // low byte of method ID (PPMd)
            raw[pos + 9] = 0;
        }

        // Patch Central Directory method (sig [50 4b 01 02], method at +10).
        let cd_sig = [0x50u8, 0x4b, 0x01, 0x02];
        if let Some(pos) = raw.windows(4).position(|w| w == cd_sig) {
            raw[pos + 10] = 98;
            raw[pos + 11] = 0;
        }

        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2, &crate::ResourceLimits::default())
            .await
            .unwrap();

        // open() must succeed — unknown methods are deferred (not an error at parse time).
        assert_eq!(zip.entries()[0].name, "file.txt");
        assert_eq!(
            zip.entries()[0].compression_method,
            Some(CompressionMethod::Ppmd),
            "method should be recognised in the entry metadata"
        );

        // extract() must fail with UnsupportedCompression — not a panic or InvalidFormat.
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &crate::ResourceLimits::default(),
            )
            .await;
        assert!(
            matches!(result, Err(CoreError::UnsupportedCompression { .. })),
            "expected UnsupportedCompression, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn chunked_extraction_produces_identical_output() {
        // ~200 KB of repeating data — compresses well with Deflate, and the
        // decompressed output spans at least three 64 KB chunks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();

        let (_tmp, reader) = create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(
            data, content,
            "chunked extraction must be byte-for-byte identical"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_extraction() {
        // Shared state accessed both by the reporter (inside spawn_blocking) and the test.
        #[derive(Default)]
        struct State {
            chunks_seen: u32,
            entry_start_called: bool,
            entry_complete_called: bool,
            complete_success: Option<bool>,
        }

        struct CancelAfterFirstChunk {
            state: Arc<Mutex<State>>,
        }

        impl ProgressReporter for CancelAfterFirstChunk {
            fn on_entry_start(&mut self, _name: &str, _size: u64) {
                self.state.lock().unwrap().entry_start_called = true;
            }
            fn on_bytes_progress(&mut self, _bytes_so_far: u64) {
                self.state.lock().unwrap().chunks_seen += 1;
            }
            fn on_entry_complete(&mut self, _name: &str, success: bool) {
                let mut s = self.state.lock().unwrap();
                s.entry_complete_called = true;
                s.complete_success = Some(success);
            }
            fn should_cancel(&self) -> bool {
                self.state.lock().unwrap().chunks_seen >= 1
            }
        }

        // ~200 KB — enough to span multiple 64 KB chunks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();

        let (_tmp, reader) = create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let state = Arc::new(Mutex::new(State::default()));
        let progress: Arc<Mutex<dyn ProgressReporter>> =
            Arc::new(Mutex::new(CancelAfterFirstChunk {
                state: Arc::clone(&state),
            }));

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await;

        assert!(
            matches!(result, Err(CoreError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );

        let s = state.lock().unwrap();
        assert!(s.entry_start_called, "on_entry_start must be called");
        assert!(
            s.entry_complete_called,
            "on_entry_complete must be called on cancellation"
        );
        assert_eq!(
            s.complete_success,
            Some(false),
            "on_entry_complete must report success=false on cancellation"
        );
    }

    #[tokio::test]
    async fn progress_reports_bytes() {
        #[derive(Default)]
        struct State {
            reported: Vec<u64>,
        }

        struct ByteRecorder {
            state: Arc<Mutex<State>>,
        }

        impl ProgressReporter for ByteRecorder {
            fn on_bytes_progress(&mut self, bytes_so_far: u64) {
                self.state.lock().unwrap().reported.push(bytes_so_far);
            }
        }

        // ~200 KB — produces multiple progress callbacks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();
        let uncompressed_size = content.len() as u64;

        let (_tmp, reader) = create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let state = Arc::new(Mutex::new(State::default()));
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(ByteRecorder {
            state: Arc::clone(&state),
        }));

        let mut sink = tokio::io::sink();
        zip.extract(
            &zip.entries()[0],
            &mut sink,
            progress,
            None,
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        let s = state.lock().unwrap();
        assert!(
            s.reported.len() > 1,
            "expected multiple progress callbacks, got {}",
            s.reported.len()
        );
        assert_eq!(
            *s.reported.last().unwrap(),
            uncompressed_size,
            "final progress value must equal uncompressed_size"
        );
        // Values must be strictly increasing.
        for w in s.reported.windows(2) {
            assert!(w[1] > w[0], "progress values must be strictly increasing");
        }
    }

    #[tokio::test]
    async fn truncated_zip_returns_invalid_format() {
        // Write a real ZIP then truncate it to just 10 bytes — far too short
        // to contain any valid EOCD record. open() must return an error, not panic.
        let (_tmp, _reader) = create_test_zip(&[("file.txt", b"hello", ZipCrateMethod::Stored)]);

        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        raw.truncate(10);
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let result = ZipArchive::open(reader2, &crate::ResourceLimits::default()).await;

        assert!(
            matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected InvalidFormat for truncated ZIP, got: {:?}",
            result.err()
        );
    }

    // ── WinZip AES encryption ──────────────────────────────────────────────

    /// Helper: create a ZIP with WinZip AES-256 encryption.
    fn create_aes_encrypted_test_zip(
        password: &str,
        files: &[(&str, &[u8], ZipCrateMethod)],
    ) -> (tempfile::NamedTempFile, LocalReadAt) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data, method) in files {
                let options = SimpleFileOptions::default()
                    .compression_method(*method)
                    .with_aes_encryption(zip::AesMode::Aes256, password);
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            let buf = writer.finish().unwrap().into_inner();
            std::fs::write(tmp.path(), &buf).unwrap();
        }
        let reader = LocalReadAt::new(tmp.path());
        (tmp, reader)
    }

    #[tokio::test]
    async fn aes_entry_detected_as_encrypted() {
        let (_tmp, reader) = create_aes_encrypted_test_zip(
            "secret",
            &[("file.txt", b"hello", ZipCrateMethod::Stored)],
        );
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let entry = &zip.entries()[0];
        assert!(entry.is_encrypted, "AES entry must be flagged as encrypted");
        // compression_method must reflect the real method (Store), not AES marker 99
        assert_eq!(
            entry.compression_method,
            Some(CompressionMethod::Store),
            "compression_method must show real method, not AES marker"
        );
    }

    #[tokio::test]
    async fn extract_aes256_encrypted_entry() {
        let password = "correct-horse-aes";
        let content = b"pluq WinZip AES test content!";

        let (_tmp, reader) = create_aes_encrypted_test_zip(
            password,
            &[("secret.txt", content, ZipCrateMethod::Stored)],
        );
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert!(zip.entries()[0].is_encrypted);

        let mut buf = Vec::new();
        zip.extract(
            &zip.entries()[0],
            &mut buf,
            Arc::new(Mutex::new(())),
            Some(password.as_bytes()),
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(buf.as_slice(), content);
    }

    #[tokio::test]
    async fn extract_aes256_deflated_encrypted_entry() {
        let password = "deflate-aes-pw";
        let content: Vec<u8> = b"pluq AES + Deflate pipeline test. "
            .iter()
            .cycle()
            .take(2048)
            .copied()
            .collect();

        let (_tmp, reader) = create_aes_encrypted_test_zip(
            password,
            &[("large.txt", &content, ZipCrateMethod::Deflated)],
        );
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert_eq!(
            zip.entries()[0].compression_method,
            Some(CompressionMethod::Deflate),
            "real method must be Deflate"
        );

        let mut buf = Vec::new();
        zip.extract(
            &zip.entries()[0],
            &mut buf,
            Arc::new(Mutex::new(())),
            Some(password.as_bytes()),
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(buf, content);
    }

    #[tokio::test]
    async fn aes_wrong_password_detected() {
        let password = "correct-horse-aes";
        let content = b"some sensitive data";

        let (_tmp, reader) = create_aes_encrypted_test_zip(
            password,
            &[("secret.txt", content, ZipCrateMethod::Stored)],
        );
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                Some(b"wrong-password"),
                &crate::ResourceLimits::default(),
            )
            .await;

        assert!(
            matches!(
                result,
                Err(CoreError::WrongPassword {
                    method: PasswordDetectionMethod::EarlyHeaderValidation
                }) | Err(CoreError::WrongPassword {
                    method: PasswordDetectionMethod::CrcMismatch
                })
            ),
            "expected WrongPassword (early or HMAC), got: {result:?}"
        );
    }

    // ── ZipCrypto (traditional encryption) ────────────────────────────────

    /// Helper: create a ZIP with traditional PKWARE encryption.
    fn create_encrypted_test_zip(
        password: &[u8],
        files: &[(&str, &[u8], ZipCrateMethod)],
    ) -> (tempfile::NamedTempFile, LocalReadAt) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data, method) in files {
                let options = SimpleFileOptions::default()
                    .compression_method(*method)
                    .with_deprecated_encryption(password)
                    .unwrap();
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            let buf = writer.finish().unwrap().into_inner();
            std::fs::write(tmp.path(), &buf).unwrap();
        }
        let reader = LocalReadAt::new(tmp.path());
        (tmp, reader)
    }

    #[tokio::test]
    async fn extract_traditional_encrypted_entry() {
        let password = b"correct-horse";
        let content = b"Secret message for pluq!";

        let (_tmp, reader) =
            create_encrypted_test_zip(password, &[("secret.txt", content, ZipCrateMethod::Stored)]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert!(
            zip.entries()[0].is_encrypted,
            "entry must be marked as encrypted"
        );

        let mut buf = Vec::new();
        zip.extract(
            &zip.entries()[0],
            &mut buf,
            Arc::new(Mutex::new(())),
            Some(password),
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(buf.as_slice(), content);
    }

    #[tokio::test]
    async fn wrong_password_detected_early() {
        let password = b"correct-horse";
        let content = b"Secret message for pluq!";

        let (_tmp, reader) =
            create_encrypted_test_zip(password, &[("secret.txt", content, ZipCrateMethod::Stored)]);

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                Some(b"wrong-password"),
                &crate::ResourceLimits::default(),
            )
            .await;

        // The early header check catches most wrong passwords (1/256 false-negative rate).
        // If it slips through, the CRC or decompressor will still catch it.
        assert!(
            matches!(
                result,
                Err(CoreError::WrongPassword {
                    method: PasswordDetectionMethod::EarlyHeaderValidation
                }) | Err(CoreError::CrcMismatch { .. })
                    | Err(CoreError::BitstreamError { .. })
            ),
            "expected WrongPassword, CrcMismatch, or BitstreamError, got: {result:?}"
        );
    }

    // ── DoS protection ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn zip_bomb_ratio_rejected() {
        // 11 MB of zeros compresses to ~2 KB with Deflate (ratio ~5500:1), which
        // vastly exceeds the 1000:1 limit and also blows past the 10 MB output floor.
        // No patching needed — the real compressed_size in the archive is tiny.
        let content = vec![0u8; 11 * 1024 * 1024];
        let (_tmp, reader) = create_test_zip(&[("bomb.bin", &content, ZipCrateMethod::Deflated)]);

        // Confirm the archive actually compresses well (ratio > 1000:1).
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        let entry = &zip.entries()[0];
        assert!(
            entry.compressed_size < entry.uncompressed_size / 1000,
            "test data must compress better than 1000:1 (got {}/{})",
            entry.compressed_size,
            entry.uncompressed_size
        );

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                entry,
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &crate::ResourceLimits::default(),
            )
            .await;

        assert!(
            matches!(result, Err(CoreError::ResourceLimitExceeded { .. })),
            "expected ResourceLimitExceeded for zip bomb, got: {result:?}"
        );
    }

    struct WarnReporter {
        warnings: Vec<String>,
    }
    impl crate::progress::ProgressReporter for WarnReporter {
        fn on_warning(&mut self, msg: &str) {
            self.warnings.push(msg.to_string());
        }
    }

    #[tokio::test]
    async fn zip_bomb_ratio_warned() {
        let content = vec![0u8; 11 * 1024 * 1024];
        let (_tmp, reader) = create_test_zip(&[("bomb.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let limits = crate::ResourceLimits {
            ratio_limit: Some(crate::limits::RatioLimit {
                max_ratio: 1000,
                mode: crate::limits::LimitMode::Warn,
            }),
            ..Default::default()
        };

        let mut sink = tokio::io::sink();
        let reporter = Arc::new(Mutex::new(WarnReporter { warnings: vec![] }));
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                reporter.clone(),
                None,
                &limits,
            )
            .await;

        assert!(
            result.is_ok(),
            "warn mode must not block extraction, got err: {:?}",
            result.err()
        );
        assert!(
            reporter
                .lock()
                .unwrap()
                .warnings
                .iter()
                .any(|w| w.contains("suspicious compression ratio")),
            "warning was not recorded by reporter"
        );
    }

    #[tokio::test]
    async fn max_entry_unpacked_rejected() {
        let content: Vec<u8> = (0u8..=255).cycle().take(100 * 1024).collect(); // 100KB
        let (_tmp, reader) = create_test_zip(&[("normal.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let limits = crate::ResourceLimits {
            max_entry_unpacked: 50 * 1024, // 50KB limit
            ..Default::default()
        };

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(
                &zip.entries()[0],
                &mut sink,
                Arc::new(Mutex::new(())),
                None,
                &limits,
            )
            .await;

        assert!(
            matches!(result, Err(CoreError::ResourceLimitExceeded { .. })),
            "expected ResourceLimitExceeded for max_entry_unpacked, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn legitimate_high_ratio_allowed() {
        // Cycling bytes 0–255 in a 100 KB file compresses moderately (ratio well under
        // 1000:1), so the ratio check must not reject it.
        let content: Vec<u8> = (0u8..=255).cycle().take(100 * 1024).collect();
        let (_tmp, reader) = create_test_zip(&[("normal.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let entry = &zip.entries()[0];
        assert!(
            entry.compressed_size > entry.uncompressed_size / 1000,
            "test data must not compress more than 1000:1 (got {}/{})",
            entry.compressed_size,
            entry.uncompressed_size
        );

        let data = extract_to_vec(&zip, entry).await;
        assert_eq!(
            data, content,
            "legitimate high-ratio file must extract successfully"
        );
    }

    #[tokio::test]
    async fn excessive_entry_count_rejected() {
        // Construct a fake ZIP64 archive that claims 20 million entries.
        // max_entries default is 1M, so ZipArchive::open must reject it immediately.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let absurd_count: u64 = 20_000_000;
            let mut buf = Vec::new();

            // ZIP64 EOCD Record (56 bytes total, starts at offset 0).
            buf.extend_from_slice(&0x06064b50u32.to_le_bytes()); // signature
            buf.extend_from_slice(&44u64.to_le_bytes()); // size of remaining record
            buf.extend_from_slice(&45u16.to_le_bytes()); // version made by
            buf.extend_from_slice(&45u16.to_le_bytes()); // version needed
            buf.extend_from_slice(&0u32.to_le_bytes()); // disk number
            buf.extend_from_slice(&0u32.to_le_bytes()); // disk with start of CD
            buf.extend_from_slice(&absurd_count.to_le_bytes()); // entries on this disk
            buf.extend_from_slice(&absurd_count.to_le_bytes()); // total entries
            buf.extend_from_slice(&0u64.to_le_bytes()); // CD size
            buf.extend_from_slice(&0u64.to_le_bytes()); // CD offset

            // ZIP64 EOCD Locator (20 bytes): points back to offset 0.
            buf.extend_from_slice(&0x07064b50u32.to_le_bytes()); // signature
            buf.extend_from_slice(&0u32.to_le_bytes()); // disk with ZIP64 EOCD
            buf.extend_from_slice(&0u64.to_le_bytes()); // offset of ZIP64 EOCD
            buf.extend_from_slice(&1u32.to_le_bytes()); // total disks

            // Standard EOCD (22 bytes): sentinel values trigger ZIP64 lookup.
            buf.extend_from_slice(&0x06054b50u32.to_le_bytes()); // signature
            buf.extend_from_slice(&0u16.to_le_bytes()); // disk number
            buf.extend_from_slice(&0u16.to_le_bytes()); // CD start disk
            buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entries this disk (sentinel)
            buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // total entries (sentinel)
            buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // CD size (sentinel)
            buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // CD offset (sentinel)
            buf.extend_from_slice(&0u16.to_le_bytes()); // comment length

            std::fs::write(tmp.path(), &buf).unwrap();
        }

        let reader = LocalReadAt::new(tmp.path());
        let result = ZipArchive::open(reader, &crate::ResourceLimits::default()).await;

        assert!(
            matches!(result, Err(CoreError::ResourceLimitExceeded { .. })),
            "expected ResourceLimitExceeded for 20M entry count, got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn extract_encrypted_deflated_entry() {
        let password = b"deflate-test-pw";
        // Repeating data that compresses well with Deflate.
        let content: Vec<u8> = b"pluq ZipCrypto test with Deflate compression. "
            .iter()
            .cycle()
            .take(1024)
            .copied()
            .collect();

        let (_tmp, reader) = create_encrypted_test_zip(
            password,
            &[("deflated_secret.txt", &content, ZipCrateMethod::Deflated)],
        );

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        assert!(
            zip.entries()[0].is_encrypted,
            "entry must be marked as encrypted"
        );

        let mut buf = Vec::new();
        zip.extract(
            &zip.entries()[0],
            &mut buf,
            Arc::new(Mutex::new(())),
            Some(password),
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(buf, content);
    }

    /// Regression test: when the compression-ratio guard fires, `on_entry_complete`
    /// must still be called exactly once with `success == false`.
    ///
    /// Before B3, the ratio-guard early exit (`check_extraction_limits(...)?`)
    /// returned without ever calling `on_entry_complete`, causing the progress
    /// counter to under-count by one for every subsequent entry.
    #[tokio::test]
    async fn ratio_guard_triggers_exactly_one_entry_complete() {
        #[derive(Default)]
        struct SpyState {
            entry_complete_count: u32,
            complete_success: Option<bool>,
        }

        struct SpyReporter {
            state: Arc<Mutex<SpyState>>,
        }

        impl ProgressReporter for SpyReporter {
            fn on_entry_complete(&mut self, _name: &str, success: bool) {
                let mut s = self.state.lock().unwrap();
                s.entry_complete_count += 1;
                s.complete_success = Some(success);
            }
        }

        // 2000 bytes of a single repeated byte compresses to ~12 bytes with Deflate.
        // With max_ratio=5 the ratio check fires well before the loop finishes.
        let content = vec![b'a'; 2000];
        let (_tmp, reader) = create_test_zip(&[(
            "highly_compressible.txt",
            &content,
            ZipCrateMethod::Deflated,
        )]);

        let limits = crate::ResourceLimits {
            ratio_limit: Some(crate::limits::RatioLimit {
                max_ratio: 5,
                mode: crate::limits::LimitMode::HardError,
            }),
            ..Default::default()
        };

        let zip = ZipArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        let state = Arc::new(Mutex::new(SpyState::default()));
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(SpyReporter {
            state: Arc::clone(&state),
        }));

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(&zip.entries()[0], &mut sink, progress, None, &limits)
            .await;

        assert!(
            matches!(result, Err(CoreError::ResourceLimitExceeded { .. })),
            "expected ResourceLimitExceeded, got: {result:?}"
        );

        let s = state.lock().unwrap();
        assert_eq!(
            s.entry_complete_count, 1,
            "on_entry_complete must fire exactly once even on ratio-guard error"
        );
        assert_eq!(
            s.complete_success,
            Some(false),
            "on_entry_complete must report success=false on ratio-guard error"
        );
    }
}
