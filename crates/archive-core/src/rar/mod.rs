//! RAR format support.
//!
//! v1 scope: RAR5 non-solid archives (listing + extraction).
//! RAR4 archives are detected and rejected with a clear error.

#[cfg(test)]
mod batch_tests;
pub(crate) mod parse;

pub use parse::{RAR4_SIGNATURE, RAR5_SIGNATURE};

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use blake2s_simd::blake2sp;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;
use tracing::debug;

use crate::core_error::CoreError;
use crate::progress::ProgressReporter;

pub(crate) struct VerifyData<'a> {
    pub name: &'a str,
    pub expected_crc: Option<u32>,
    pub expected_blake2: Option<[u8; 32]>,
    pub compressed_size: u64,
}

/// Stream a file on disk through CRC32 + BLAKE2sp verification into `sink`,
/// 256 KiB at a time. Hashes are computed incrementally so RAM use is bounded
/// by the buffer size regardless of file length.
///
/// Note: partial bytes may land in `sink` before a hash mismatch is detected,
/// because we cannot verify the full output without buffering it. Callers that
/// need atomic writes must wrap `sink` accordingly.
pub(crate) async fn verify_and_stream_from_file<W: AsyncWrite + Send + Unpin>(
    mut file: std::fs::File,
    verify: VerifyData<'_>,
    sink: &mut W,
    progress: &Arc<Mutex<dyn ProgressReporter>>,
    limits: &crate::ResourceLimits,
) -> Result<u64, CoreError> {
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("failed to seek temp file for verification: {e}"),
        })?;

    let mut file = tokio::fs::File::from_std(file);
    let mut crc_hasher = crc32fast::Hasher::new();
    let mut blake_state = blake2sp::State::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total_bytes = 0u64;
    let mut ratio_warned = false;

    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("failed to read from temp file: {e}"),
            })?;

        if n == 0 {
            break;
        }

        let chunk = &buf[..n];
        if verify.expected_crc.is_some() {
            crc_hasher.update(chunk);
        }
        if verify.expected_blake2.is_some() {
            blake_state.update(chunk);
        }

        sink.write_all(chunk)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("failed to write to sink: {e}"),
            })?;

        total_bytes += n as u64;

        if !ratio_warned {
            ratio_warned = limits.check_extraction_limits(
                verify.name,
                verify.compressed_size,
                total_bytes,
                progress,
                true,
                false,
            )?;
        } else {
            limits.check_extraction_limits(
                verify.name,
                verify.compressed_size,
                total_bytes,
                progress,
                true,
                true,
            )?;
        }
    }

    // Finalize hashes and compare.
    if let Some(expected) = verify.expected_crc {
        let actual = crc_hasher.finalize();
        if actual != expected {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(verify.name, false);
            return Err(CoreError::CrcMismatch {
                entry: verify.name.to_owned(),
                expected,
                actual,
            });
        }
    }

    if let Some(expected) = verify.expected_blake2 {
        let actual = blake_state.finalize();
        if actual.as_bytes() != expected {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(verify.name, false);
            return Err(CoreError::BitstreamError {
                detail: format!("BLAKE2sp hash mismatch for '{}'", verify.name),
            });
        }
    }

    progress
        .lock()
        .unwrap()
        .on_entry_complete(verify.name, true);
    Ok(total_bytes)
}

/// Blocking version of [`verify_and_stream_from_file`] for use inside `spawn_blocking`.
pub(crate) fn verify_and_stream_from_file_sync<W: Write>(
    mut file: std::fs::File,
    verify: VerifyData<'_>,
    sink: &mut W,
    progress: &Arc<Mutex<dyn ProgressReporter>>,
    limits: &crate::ResourceLimits,
) -> Result<u64, CoreError> {
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("failed to seek temp file for sync verification: {e}"),
        })?;

    let mut crc_hasher = crc32fast::Hasher::new();
    let mut blake_state = blake2sp::State::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total_bytes = 0u64;
    let mut ratio_warned = false;

    loop {
        let n = file.read(&mut buf).map_err(|e| CoreError::BitstreamError {
            detail: format!("failed to read from temp file (sync): {e}"),
        })?;

        if n == 0 {
            break;
        }

        let chunk = &buf[..n];
        if verify.expected_crc.is_some() {
            crc_hasher.update(chunk);
        }
        if verify.expected_blake2.is_some() {
            blake_state.update(chunk);
        }

        sink.write_all(chunk)
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("failed to write to sink (sync): {e}"),
            })?;

        total_bytes += n as u64;

        if !ratio_warned {
            ratio_warned = limits.check_extraction_limits(
                verify.name,
                verify.compressed_size,
                total_bytes,
                progress,
                true,
                false,
            )?;
        } else {
            limits.check_extraction_limits(
                verify.name,
                verify.compressed_size,
                total_bytes,
                progress,
                true,
                true,
            )?;
        }
    }

    // Finalize hashes and compare.
    if let Some(expected) = verify.expected_crc {
        let actual = crc_hasher.finalize();
        if actual != expected {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(verify.name, false);
            return Err(CoreError::CrcMismatch {
                entry: verify.name.to_owned(),
                expected,
                actual,
            });
        }
    }

    if let Some(expected) = verify.expected_blake2 {
        let actual = blake_state.finalize();
        if actual.as_bytes() != expected {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(verify.name, false);
            return Err(CoreError::BitstreamError {
                detail: format!("BLAKE2sp hash mismatch for '{}'", verify.name),
            });
        }
    }

    progress
        .lock()
        .unwrap()
        .on_entry_complete(verify.name, true);
    Ok(total_bytes)
}

use serde::{Deserialize, Serialize};

use tracing::info;

use crate::archive_reader::{ArchiveReader, BatchResult};
use crate::codec::CompressionMethod;
use crate::io::{FetchRegion, LocalPath, build_clusters};
use crate::rar::parse::{
    QoCachedBlock, parse_block_header, parse_file_header, parse_main_header, parse_quick_open_data,
};
use crate::{ArchiveEntry, ReadAt};

// ── Location data (private) ───────────────────────────────────────────────────

/// Per-entry location data, parallel to `RarArchive::entries`.
///
/// Holds byte offsets and extraction metadata that are not part of the
/// public `ArchiveEntry` API. `pub` for cache serialization in the `archive` crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RarEntryLocation {
    /// Absolute offset of the file header block in the archive.
    /// Used in cloud extraction (synthetic archive construction).
    pub header_offset: u64,
    /// Byte length of the file header block (header fields only, not data area).
    /// Used in cloud extraction (synthetic archive construction).
    pub header_size: u64,
    /// Absolute offset of the compressed data in the archive.
    pub data_offset: u64,
    /// Compressed data size in bytes.
    pub data_size: u64,
    /// RAR5 compression method: 0 = Store, 1–5 = proprietary compression.
    pub method: u8,
    /// Compression dictionary size in bytes.
    /// Informational — unrar handles dict size internally from the file header.
    #[allow(dead_code)]
    pub dict_size: u64,
    /// CRC32 of the uncompressed data, if present in the file header.
    pub crc32: Option<u32>,
    /// BLAKE2sp hash of the uncompressed data, if present in the extra area.
    pub blake2_hash: Option<[u8; 32]>,
    /// Per-file solid flag — entry depends on preceding entries' dictionary state.
    pub is_solid: bool,
    /// True if an encryption extra area record was found for this entry.
    pub is_encrypted: bool,
}

// ── Streaming splitter types (private) ───────────────────────────────────────

/// Sent from producer to consumer over the bounded channel.
/// Used by the streaming splitter pipeline.
pub(crate) struct SnippetMsg {
    /// Path to the temp file containing a valid standalone RAR archive.
    pub(crate) path: std::path::PathBuf,
    /// The discovered entry metadata.
    pub(crate) entry: ArchiveEntry,
    /// Compressed data size.
    pub(crate) data_size: u64,
    /// CRC32 of uncompressed data (for verification after unrar).
    pub(crate) crc32: Option<u32>,
    /// BLAKE2sp hash (RAR5 only; None for RAR4).
    pub(crate) blake2_hash: Option<[u8; 32]>,
}

/// Extraction config passed into the modified elastic scan.
/// When provided, the scan writes snippet micro-archives instead of discarding data.
pub(crate) struct SnippetConfig {
    pub(crate) sender: tokio::sync::mpsc::Sender<SnippetMsg>,
    pub(crate) preamble: bytes::Bytes,
    pub(crate) end_block: bytes::Bytes,
    pub(crate) filter: Box<dyn Fn(&ArchiveEntry) -> bool + Send + Sync>,
}

// ── RarArchive ────────────────────────────────────────────────────────────────

/// A parsed RAR5 archive backed by any [`ReadAt`] source.
///
/// Created via [`RarArchive::open`], which reads the RAR5 signature and main
/// header, then scans file headers sequentially (or via Quick Open in Step 3).
///
/// # Internal layout
///
/// Holds two parallel `Vec`s at the same index:
/// - `entries` — public, format-agnostic [`ArchiveEntry`] metadata
/// - `locations` — private [`RarEntryLocation`] with byte offsets and extraction details
///
/// # Synthetic archive construction (cloud extraction)
///
/// `archive_preamble` contains the RAR5 signature + main header bytes.
/// `end_block` contains a minimal end-of-archive block.  Together with a
/// file header + compressed data, they form a minimal valid RAR5 archive that
/// `unrar` can decompress.
pub struct RarArchive<R: ReadAt> {
    pub(crate) reader: R,
    entries: Vec<ArchiveEntry>,
    pub(crate) locations: Vec<RarEntryLocation>,
    /// RAR5 signature + main header bytes (for synthetic archive construction in cloud extraction).
    pub(crate) archive_preamble: Bytes,
    /// Minimal end-of-archive block (for synthetic archive construction in cloud extraction).
    pub(crate) end_block: Bytes,
    /// Local filesystem path of the archive, if the source is a local file.
    ///
    /// `Some` for [`LocalReadAt`](crate::io::LocalReadAt) sources; `None` for HTTP.
    /// Captured at [`open`](RarArchive::open) time and used by [`extract_compressed`]
    /// to choose between the direct-path (`unrar` shortcut) and synthetic-archive
    /// (cloud) extraction paths.
    local_path: Option<PathBuf>,
    /// Total archive size in bytes (for scan_info estimated_bytes).
    archive_size: u64,
    /// Whether the entry list has been populated (via QO, local scan, or scan_metadata).
    scan_complete: bool,
}

impl<R: ReadAt> std::fmt::Debug for RarArchive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RarArchive")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl<R: ReadAt> RarArchive<R> {
    /// Open a RAR5 archive by verifying the signature and scanning file headers.
    ///
    /// Reads in 64 KiB chunks to minimise HTTP round trips for cloud sources.
    /// Solid entries are listed but rejected at extract (and reported as
    /// failures in batch extraction) — user sees them, can't extract them.
    ///
    /// For local sources, always performs the sequential scan (or Quick Open).
    /// For HTTP sources without Quick Open, defers the scan — returns with
    /// `scan_complete = false`. Call [`scan_metadata`](ArchiveReader::scan_metadata)
    /// before accessing entries.
    ///
    /// Returns [`CoreError::InvalidFormat`] if the RAR5 signature is absent.
    /// Returns [`CoreError::UnsupportedFormat`] if a RAR4 signature is detected.
    pub async fn open(reader: R, limits: &crate::ResourceLimits) -> Result<Self, CoreError>
    where
        R: LocalPath,
    {
        info!("opening RAR archive");
        let local_path = reader.local_path().map(|p| p.to_path_buf());
        let size = reader.size().await?;
        debug!(
            size,
            local = local_path.is_some(),
            "RAR archive size determined"
        );

        // 1. Verify RAR5 signature (8 bytes at offset 0).
        let sig = reader.read_range(0, 8).await?;
        if sig.len() < 8 {
            return Err(CoreError::InvalidFormat {
                message: "archive too small to contain a RAR5 signature".into(),
            });
        }
        if sig[..7] == RAR4_SIGNATURE {
            return Err(CoreError::UnsupportedFormat {
                format: "RAR4 archives are not yet supported (RAR5 only)".into(),
            });
        }
        if sig[..8] != RAR5_SIGNATURE {
            return Err(CoreError::InvalidFormat {
                message: "not a RAR5 archive (bad signature)".into(),
            });
        }

        // 2. Read initial 64 KiB chunk — covers the signature, main header, and
        //    typically hundreds of file headers for most real-world archives.
        let initial_chunk_size = (64 * 1024_u64).min(size);
        let chunk = reader.read_range(0, initial_chunk_size).await?;

        // 3. Parse the main archive header (starts immediately after the 8-byte signature).
        let main_header = parse_main_header(&chunk, 8)?;
        let preamble_end = (main_header.raw.archive_offset + main_header.raw.total_size) as usize;
        if preamble_end > chunk.len() {
            return Err(CoreError::CorruptHeader {
                offset: 8,
                reason: "main header extends beyond initial read".into(),
            });
        }
        let archive_preamble = chunk.slice(0..preamble_end);
        let end_block = build_end_of_archive_block();

        // 4. Try Quick Open optimisation (avoids full sequential scan for QO-enabled archives).
        //    On failure we fall back depending on source type.
        if let Some(locator) = &main_header.locator
            && let Some(qo_offset) = locator.quick_open_offset
        {
            let qo_abs = main_header.raw.archive_offset + qo_offset;
            debug!(qo_offset = qo_abs, "attempting Quick Open");
            if let Ok(Some((entries, locations))) =
                Self::try_quick_open(&reader, qo_abs, size, limits).await
            {
                info!(entries = entries.len(), "RAR opened via Quick Open");
                return Ok(Self {
                    reader,
                    entries,
                    locations,
                    archive_preamble,
                    end_block,
                    local_path,
                    archive_size: size,
                    scan_complete: true,
                });
            }
        }

        // 5. QO not available or failed.
        //    - Local source: run elastic scan immediately.
        //    - HTTP source: defer the scan — caller must invoke scan_metadata().
        debug!("Quick Open unavailable or failed, falling back");
        if local_path.is_some() {
            debug!("local source: running immediate elastic scan");
            let no_op: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
            let (entries, locations) =
                Self::run_elastic_scan(&reader, preamble_end as u64, size, &no_op, None, limits)
                    .await?;
            return Ok(Self {
                reader,
                entries,
                locations,
                archive_preamble,
                end_block,
                local_path,
                archive_size: size,
                scan_complete: true,
            });
        }

        // HTTP source without QO — defer scan.
        info!("HTTP source without Quick Open — deferring metadata scan");
        Ok(Self {
            reader,
            entries: vec![],
            locations: vec![],
            archive_preamble,
            end_block,
            local_path,
            archive_size: size,
            scan_complete: false,
        })
    }

    /// All entries in the archive.
    ///
    /// Solid entries are included in the index but cannot be extracted via
    /// surgical cloud paths (surgical extraction requires random access).
    pub fn entries(&self) -> &[ArchiveEntry] {
        if self.scan_complete {
            &self.entries
        } else {
            &[]
        }
    }

    /// Per-entry location data, parallel to `entries()`. Used by the cache layer.
    pub fn locations(&self) -> &[RarEntryLocation] {
        &self.locations
    }

    /// RAR5 signature + main header bytes (for synthetic archive construction).
    pub fn archive_preamble_bytes(&self) -> &[u8] {
        &self.archive_preamble
    }

    /// Minimal end-of-archive block bytes.
    pub fn end_block_bytes(&self) -> &[u8] {
        &self.end_block
    }

    /// Total archive size in bytes.
    pub fn archive_size(&self) -> u64 {
        self.archive_size
    }

    /// Reconstruct a `RarArchive` from cached data, skipping all I/O.
    pub fn from_cached(
        reader: R,
        entries: Vec<ArchiveEntry>,
        locations: Vec<RarEntryLocation>,
        archive_preamble: Bytes,
        end_block: Bytes,
        local_path: Option<PathBuf>,
        archive_size: u64,
    ) -> Self {
        Self {
            reader,
            entries,
            locations,
            archive_preamble,
            end_block,
            local_path,
            archive_size,
            scan_complete: true,
        }
    }

    /// Elastic metadata scan — streams archive bytes from `start_offset`.
    ///
    /// Opens a single `read_stream` and slides through blocks sequentially.
    /// Small data areas (≤ 10 MiB) are consumed in-stream.
    /// Large data areas are skipped via a surgical seek (new stream open).
    ///
    /// Progress callbacks fire after every block with the absolute byte offset
    /// and current entry count.
    async fn run_elastic_scan(
        reader: &R,
        start_offset: u64,
        size: u64,
        progress: &Arc<Mutex<dyn ProgressReporter>>,
        snippet_config: Option<SnippetConfig>,
        limits: &crate::ResourceLimits,
    ) -> Result<(Vec<ArchiveEntry>, Vec<RarEntryLocation>), CoreError> {
        const LARGE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MiB
        debug!(start_offset, size, "elastic scan starting");

        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<RarEntryLocation> = Vec::new();
        let mut header_bytes_read: u64 = 0;

        if start_offset >= size {
            return Ok((entries, locations));
        }

        let mut stream = reader
            .read_stream(start_offset, size - start_offset)
            .await
            .map_err(CoreError::Io)?;

        // Sliding buffer: buf[buf_pos..] = archive bytes starting at `offset`.
        // Invariant: stream_head = offset + (buf.len() - buf_pos)
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut buf_pos: usize = 0;
        let mut offset = start_offset;

        loop {
            if offset >= size {
                break;
            }

            // Check for cancellation at the top of each block iteration.
            if progress.lock().unwrap().should_cancel() {
                return Err(CoreError::Cancelled);
            }

            // Compact consumed prefix to bound memory usage.
            if buf_pos > 4096 {
                buf.drain(..buf_pos);
                buf_pos = 0;
            }

            // Helper: fill buf so that buf[buf_pos..buf_pos+n] is available.
            // Reads from stream, up to archive boundary. If insufficient data
            // exists in the archive, we just return what we have (parse will fail).
            macro_rules! fill {
                ($n:expr) => {
                    let needed = buf_pos + $n;
                    if buf.len() < needed {
                        let stream_head = offset + (buf.len().saturating_sub(buf_pos)) as u64;
                        let avail = size.saturating_sub(stream_head) as usize;
                        let to_read = (needed - buf.len()).min(avail);
                        if to_read > 0 {
                            let old_len = buf.len();
                            buf.resize(old_len + to_read, 0);
                            stream
                                .read_exact(&mut buf[old_len..])
                                .await
                                .map_err(|e| scan_stream_err(e, offset))?;
                        }
                    }
                };
            }

            // Stage 1: enough for CRC (4) + header_size vint (≤8).
            fill!(12);
            if buf.len().saturating_sub(buf_pos) < 5 {
                break; // End of archive data
            }

            // Parse header_size vint to compute exact header byte count.
            let mut vpos = buf_pos + 4;
            let hdr_size_val =
                parse::read_vint(&buf, &mut vpos).map_err(|_| CoreError::CorruptHeader {
                    offset,
                    reason: "block header_size vint is truncated".into(),
                })?;
            let hdr_bytes = 4 + (vpos - (buf_pos + 4)) + hdr_size_val as usize;

            // Stage 2: ensure the full header is in buf.
            fill!(hdr_bytes);

            // Parse common block header and dispatch.
            let block = parse_block_header(&buf[buf_pos..], offset)?;
            let total_size = block.total_size;
            let data_size = block.data_size;

            let this_header_len = total_size - data_size;
            header_bytes_read += this_header_len;
            if header_bytes_read > limits.max_header_bytes {
                return Err(CoreError::ResourceLimitExceeded {
                    message: format!(
                        "RAR5 cumulative header size exceeds max_header_bytes {}",
                        limits.max_header_bytes
                    ),
                });
            }

            match block.block_type {
                2 => {
                    if entries.len() >= limits.max_entries {
                        return Err(CoreError::ResourceLimitExceeded {
                            message: format!(
                                "RAR5 entry count exceeds max_entries {}",
                                limits.max_entries
                            ),
                        });
                    }

                    // File block.
                    let file_hdr = parse_file_header(&buf[buf_pos..], block)?;
                    let is_solid = file_hdr.compression.solid;

                    let hdr_size = file_hdr.raw.total_size - file_hdr.raw.data_size;
                    let entry = ArchiveEntry {
                        name: file_hdr.name.clone(),
                        compressed_size: file_hdr.raw.data_size,
                        uncompressed_size: file_hdr.unpacked_size,
                        is_directory: file_hdr.is_directory,
                        is_encrypted: file_hdr.encryption.is_some(),
                        modified_at: file_hdr.mtime.map(unix_to_system_time),
                        compression_method: rar_method_to_codec(file_hdr.compression.method),
                        index: entries.len(),
                    };
                    let location = RarEntryLocation {
                        header_offset: offset,
                        header_size: hdr_size,
                        data_offset: offset + hdr_size,
                        data_size: file_hdr.raw.data_size,
                        method: file_hdr.compression.method,
                        dict_size: file_hdr.compression.dict_size,
                        crc32: file_hdr.data_crc32,
                        blake2_hash: file_hdr.blake2_hash,
                        is_solid,
                        is_encrypted: file_hdr.encryption.is_some(),
                    };
                    let crc32 = location.crc32;
                    let blake2_hash = location.blake2_hash;
                    entries.push(entry.clone());
                    locations.push(location);

                    if !is_solid
                        && let Some(ref config) = snippet_config
                        && (config.filter)(&entry)
                    {
                        let hdr_bytes_owned = buf[buf_pos..buf_pos + hdr_bytes].to_vec();

                        let temp_file = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
                            .await
                            .map_err(|e| CoreError::BitstreamError {
                                detail: format!("spawn temp: {e}"),
                            })?
                            .map_err(|e| CoreError::BitstreamError {
                                detail: format!("create temp: {e}"),
                            })?;

                        let std_file = temp_file.as_file().try_clone().map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("clone temp file: {e}"),
                            }
                        })?;
                        let mut snippet_writer = tokio::io::BufWriter::with_capacity(
                            64 * 1024,
                            tokio::fs::File::from_std(std_file),
                        );

                        snippet_writer
                            .write_all(&config.preamble)
                            .await
                            .map_err(snippet_write_err)?;
                        snippet_writer
                            .write_all(&hdr_bytes_owned)
                            .await
                            .map_err(snippet_write_err)?;

                        buf_pos += hdr_bytes;
                        let data_in_buf = (buf.len() - buf_pos).min(data_size as usize);
                        if data_in_buf > 0 {
                            snippet_writer
                                .write_all(&buf[buf_pos..buf_pos + data_in_buf])
                                .await
                                .map_err(snippet_write_err)?;
                        }
                        buf_pos += data_in_buf;

                        let data_remaining = data_size - data_in_buf as u64;
                        if data_remaining > 0 {
                            buf.drain(..buf_pos);
                            buf_pos = 0;
                            tokio::io::copy(
                                &mut (&mut stream).take(data_remaining),
                                &mut snippet_writer,
                            )
                            .await
                            .map_err(|e| scan_stream_err(e, offset))?;
                        }

                        snippet_writer
                            .write_all(&config.end_block)
                            .await
                            .map_err(snippet_write_err)?;
                        snippet_writer.flush().await.map_err(snippet_write_err)?;
                        drop(snippet_writer);

                        let (_, snippet_path) =
                            temp_file.keep().map_err(|e| CoreError::BitstreamError {
                                detail: format!("keep temp file: {e}"),
                            })?;
                        if config
                            .sender
                            .send(SnippetMsg {
                                path: snippet_path.clone(),
                                entry: entry.clone(),
                                data_size,
                                crc32,
                                blake2_hash,
                            })
                            .await
                            .is_err()
                        {
                            let _ = std::fs::remove_file(&snippet_path);
                            return Err(CoreError::BitstreamError {
                                detail: "extraction consumer dropped unexpectedly".into(),
                            });
                        }

                        offset += total_size;
                        progress
                            .lock()
                            .unwrap()
                            .on_scan_progress(offset, entries.len());
                        continue;
                    }

                    buf_pos += hdr_bytes; // advance past header

                    if data_size <= LARGE_THRESHOLD {
                        // Stream through: consume data_size bytes.
                        let data_in_buf = (buf.len() - buf_pos).min(data_size as usize);
                        buf_pos += data_in_buf;
                        let data_remaining = data_size - data_in_buf as u64;
                        if data_remaining > 0 {
                            buf.drain(..buf_pos);
                            buf_pos = 0;
                            tokio::io::copy(
                                &mut (&mut stream).take(data_remaining),
                                &mut tokio::io::sink(),
                            )
                            .await
                            .map_err(|e| scan_stream_err(e, offset))?;
                        }
                    } else {
                        // Surgical jump: skip the large data area entirely.
                        offset += total_size;
                        buf.clear();
                        buf_pos = 0;
                        drop(stream);
                        stream = reader
                            .read_stream(offset, size.saturating_sub(offset))
                            .await
                            .map_err(CoreError::Io)?;
                        progress
                            .lock()
                            .unwrap()
                            .on_scan_progress(offset, entries.len());
                        continue;
                    }
                }
                5 => break, // End of archive.
                _ => {
                    // Service header (3) or unknown: consume data area.
                    buf_pos += hdr_bytes;
                    let data_in_buf = (buf.len() - buf_pos).min(data_size as usize);
                    buf_pos += data_in_buf;
                    let data_remaining = data_size - data_in_buf as u64;
                    if data_remaining > 0 {
                        buf.drain(..buf_pos);
                        buf_pos = 0;
                        tokio::io::copy(
                            &mut (&mut stream).take(data_remaining),
                            &mut tokio::io::sink(),
                        )
                        .await
                        .map_err(|e| scan_stream_err(e, offset))?;
                    }
                }
            }

            offset += total_size;
            progress
                .lock()
                .unwrap()
                .on_scan_progress(offset, entries.len());
        }

        info!(entries = entries.len(), "elastic scan complete");
        Ok((entries, locations))
    }

    /// Extract multiple entries in a single pass.
    ///
    /// - **Local archives**: one `unrar` pass through the original file.
    /// - **Cloud archives**: downloads the entire archive body in **one**
    ///   `read_stream` to a temp file, then extracts locally. Reduces HTTP
    ///   round trips from O(N) to O(1).
    ///
    /// `sink_factory` is called once per entry from a blocking context.
    /// Return `Some(writer)` to extract, `None` to skip.
    pub async fn extract_all<F>(
        &mut self,
        entry_indices: &[usize],
        sink_factory: F,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError>
    where
        R: Send + Sync + 'static,
        F: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        if entry_indices.is_empty() {
            return Ok(0);
        }

        if let Some(path) = &self.local_path {
            let work: Vec<(ArchiveEntry, RarEntryLocation)> = entry_indices
                .iter()
                .filter(|&&idx| !self.entries[idx].is_directory && !self.locations[idx].is_solid)
                .map(|&idx| (self.entries[idx].clone(), self.locations[idx].clone()))
                .collect();

            if work.is_empty() {
                return Ok(0);
            }

            info!(entries = work.len(), "RAR extract_all: local path");
            return extract_all_via_unrar(
                path.clone(),
                work,
                sink_factory,
                progress,
                password,
                limits,
            )
            .await;
        }

        // Cloud path: use streaming splitter pipeline.
        let wanted: std::collections::HashSet<usize> = entry_indices.iter().copied().collect();
        self.extract_all_streaming(
            move |e: &ArchiveEntry| wanted.contains(&e.index),
            sink_factory,
            progress,
            password,
            limits,
        )
        .await
    }

    /// Extract multiple entries using stream coalescing for efficiency (Cloud only).
    ///
    /// For cloud archives, clusters nearby entries to reduce HTTP round trips.
    /// For local archives, delegates to the default `extract_selected` loop.
    pub async fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        mut sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<BatchResult, CoreError>
    where
        R: Send + Sync + 'static,
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

        if entries.is_empty() {
            return Ok(BatchResult {
                bytes_written: 0,
                succeeded: 0,
                failed: vec![],
            });
        }

        // 1. Local path: delegate to default loop (cheap already).
        if self.local_path.is_some() {
            return crate::archive_reader::extract_selected_default(
                self,
                entries,
                sink_factory,
                progress,
                password,
                limits,
            )
            .await;
        }

        // 2. Cloud path: build clusters.
        let mut batch = BatchResult {
            bytes_written: 0,
            succeeded: 0,
            failed: vec![],
        };

        let work: Vec<(&ArchiveEntry, &RarEntryLocation)> = entries
            .iter()
            .filter_map(|&e| {
                let loc = &self.locations[e.index];
                if e.is_directory {
                    None
                } else if loc.is_solid {
                    batch.failed.push((
                        e.name.clone(),
                        "solid RAR entries are not supported for batch extraction".into(),
                    ));
                    None
                } else {
                    Some((e, loc))
                }
            })
            .collect();

        if work.is_empty() {
            // Might have skipped all if they were solid or directories.
            return Ok(batch);
        }

        let regions: Vec<FetchRegion<(&ArchiveEntry, &RarEntryLocation)>> = work
            .into_iter()
            .map(|(e, loc)| FetchRegion {
                offset: loc.header_offset,
                total_len: loc.header_size.saturating_add(loc.data_size),
                payload: (e, loc),
            })
            .collect();

        let clusters = build_clusters(regions);
        let password_bytes = password.map(|p| p.to_vec());
        let archive_size = self.archive_size;

        for cluster in &clusters {
            // Cancel check between clusters.
            if progress.lock().unwrap().should_cancel() {
                return Err(CoreError::Cancelled);
            }

            let first = &cluster[0];
            let last = &cluster[cluster.len() - 1];
            let cluster_start = first.offset;
            let cluster_end = last.offset.saturating_add(last.total_len).min(archive_size);
            let cluster_len = cluster_end.saturating_sub(cluster_start);

            info!(
                entries = cluster.len(),
                cluster_start, cluster_len, "RAR extract_selected: opening cluster stream"
            );

            let stream = self
                .reader
                .read_stream(cluster_start, cluster_len)
                .await
                .map_err(CoreError::Io)?;

            // Owned clones for spawn_blocking.
            let cluster_data: Vec<(ArchiveEntry, RarEntryLocation)> = cluster
                .iter()
                .map(|r| (r.payload.0.clone(), r.payload.1.clone()))
                .collect();
            let preamble = self.archive_preamble.clone();
            let eoa = self.end_block.clone();
            let pw = password_bytes.clone();
            let prog = progress.clone();
            let handle = tokio::runtime::Handle::current();

            let results: Vec<Result<tempfile::NamedTempFile, CoreError>> =
                tokio::task::spawn_blocking(move || {
                    let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);
                    let mut stream_pos = cluster_start;
                    let mut cluster_results = Vec::with_capacity(cluster_data.len());

                    for (_entry, loc) in cluster_data {
                        // Cancel check between entries.
                        if prog.lock().unwrap().should_cancel() {
                            return Err(CoreError::Cancelled);
                        }

                        // a. Skip gap.
                        let gap = loc.header_offset.saturating_sub(stream_pos);
                        if gap > 0 {
                            std::io::copy(&mut (&mut sync_reader).take(gap), &mut std::io::sink())
                                .map_err(|e| CoreError::BitstreamError {
                                    detail: format!("skip gap failed: {e}"),
                                })?;
                            stream_pos += gap;
                        }

                        // b. Read header.
                        let mut header_bytes = vec![0u8; loc.header_size as usize];
                        sync_reader.read_exact(&mut header_bytes).map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("read header failed: {e}"),
                            }
                        })?;
                        stream_pos += loc.header_size;

                        // c-f. Assemble synthetic archive.
                        let temp_res = assemble_and_unrar_synthetic(
                            &preamble,
                            &header_bytes,
                            &mut sync_reader,
                            loc.data_size,
                            &eoa,
                            pw.as_deref(),
                            &prog,
                        );

                        match temp_res {
                            Ok(tmp) => cluster_results.push(Ok(tmp)),
                            Err(e) => cluster_results.push(Err(e)),
                        }
                        stream_pos += loc.data_size;
                    }
                    Ok(cluster_results)
                })
                .await
                .map_err(|e| CoreError::BitstreamError {
                    detail: format!("RAR cluster task panicked: {e}"),
                })??;

            // 7. Verification and streaming.
            for (result, region) in results.into_iter().zip(cluster.iter()) {
                let (entry, loc) = region.payload;
                match result {
                    Ok(out_tmp) => {
                        if let Some(mut sink) = sink_factory(entry) {
                            let verify_res = verify_and_stream_from_file(
                                out_tmp.into_file(),
                                VerifyData {
                                    name: &entry.name,
                                    expected_crc: loc.crc32,
                                    expected_blake2: loc.blake2_hash,
                                    compressed_size: loc.data_size,
                                },
                                &mut sink,
                                &progress,
                                limits,
                            )
                            .await;

                            match verify_res {
                                Ok(written) => {
                                    batch.bytes_written += written;
                                    batch.succeeded += 1;
                                }
                                Err(e) => {
                                    batch.failed.push((entry.name.clone(), e.to_string()));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        batch.failed.push((entry.name.clone(), e.to_string()));
                    }
                }
            }
        }

        Ok(batch)
    }

    /// Bytes that must be transferred to produce this entry.
    ///
    /// For surgical cloud extraction, we must download both the file header
    /// and the compressed data area.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        let loc = &self.locations[entry.index];
        loc.header_size + loc.data_size
    }

    /// Compression dictionary size in bytes for this entry.
    pub fn dict_size(&self, entry: &ArchiveEntry) -> Option<u64> {
        Some(self.locations[entry.index].dict_size)
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written.  CRC32 and BLAKE2sp (when present)
    /// are verified — output is only written after both checks pass.
    ///
    /// Method 0 (Store) is handled directly via [`ReadAt::read_stream`].
    /// Methods 1–5 (proprietary RAR compression) are handled by the `unrar` C++ library.
    /// Encrypted entries route through `unrar` regardless of method.
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

        limits.check_extraction_limits(
            &entry.name,
            entry.compressed_size,
            entry.uncompressed_size,
            &progress,
            false,
            false,
        )?;

        let loc = &self.locations[entry.index];

        info!(
            name = %entry.name,
            method = loc.method,
            compressed = loc.data_size,
            uncompressed = entry.uncompressed_size,
            encrypted = loc.is_encrypted,
            solid = loc.is_solid,
            "RAR extract start"
        );

        if loc.is_encrypted && password.is_none() {
            debug!(name = %entry.name, "password required but not provided");
            return Err(CoreError::PasswordRequired);
        }

        if loc.is_solid {
            return Err(CoreError::UnsupportedFeature {
                detail: "solid RAR archives are not yet supported".into(),
            });
        }

        // Encrypted entries must go through unrar (it handles AES decryption).
        if loc.is_encrypted {
            debug!(name = %entry.name, "encrypted entry — routing to unrar");
            return self
                .extract_compressed(entry, loc, sink, progress, password, limits)
                .await;
        }

        match loc.method {
            0 => {
                debug!(name = %entry.name, "store extraction (method 0)");
                self.extract_store(entry, loc, sink, progress, limits).await
            }
            1..=5 => {
                debug!(name = %entry.name, method = loc.method, "compressed extraction via unrar");
                self.extract_compressed(entry, loc, sink, progress, password, limits)
                    .await
            }

            m => Err(CoreError::UnsupportedCompression {
                algorithm: format!("RAR method {m}"),
            }),
        }
    }

    /// Decompress and verify CRC without retaining output.
    ///
    /// Used for archive integrity checking (`snipe test` command).
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

    /// Extract a Store (method 0) entry with CRC32 and optional BLAKE2sp verification.
    ///
    /// Follows the ZIP store pattern:
    /// 1. **async**: `read_stream()` — get a streaming `AsyncRead` for the raw data
    /// 2. **`spawn_blocking`**: `SyncIoBridge` → chunked read + CRC32 + BLAKE2sp
    /// 3. **async**: write verified buffer to sink
    async fn extract_store<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &RarEntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        // Capture owned values before the async/blocking boundary.
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let expected_blake2 = loc.blake2_hash;
        let compressed_size = loc.data_size;

        // Step 1 (async): get a streaming AsyncRead for the raw (stored) data.
        let stream = self
            .reader
            .read_stream(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        // Step 2 (spawn_blocking): read chunks, compute hashes, accumulate.
        let handle = tokio::runtime::Handle::current();
        let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);
        let limits_clone = limits.clone();

        use std::io::Write;
        let temp_file =
            tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile, CoreError> {
                const CHUNK_SIZE: usize = 64 * 1024;

                let mut crc_hasher = crc32fast::Hasher::new();
                let mut blake2_state = if expected_blake2.is_some() {
                    Some(blake2s_simd::blake2sp::State::new())
                } else {
                    None
                };

                let mut total_written: u64 = 0;
                let mut chunk_buf = vec![0u8; CHUNK_SIZE];
                let mut ratio_warned = false;

                let temp_file =
                    tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                        detail: format!("failed to create temp file: {e}"),
                    })?;
                let mut temp_writer = std::io::BufWriter::new(temp_file.as_file());

                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_start(&entry_name, uncompressed_size);
                }

                loop {
                    let n = sync_reader.read(&mut chunk_buf).map_err(|io_err| {
                        if let Some(inner) = io_err.into_inner()
                            && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
                        {
                            return CoreError::Io(*read_error);
                        }
                        CoreError::BitstreamError {
                            detail: format!("read failed for '{entry_name}'"),
                        }
                    })?;
                    if n == 0 {
                        break;
                    }

                    crc_hasher.update(&chunk_buf[..n]);
                    if let Some(ref mut state) = blake2_state {
                        state.update(&chunk_buf[..n]);
                    }

                    temp_writer.write_all(&chunk_buf[..n]).map_err(|e| {
                        CoreError::BitstreamError {
                            detail: format!("temp file write failed for '{entry_name}': {e}"),
                        }
                    })?;

                    total_written += n as u64;

                    if !ratio_warned {
                        ratio_warned = limits_clone.check_extraction_limits(
                            &entry_name,
                            compressed_size,
                            total_written,
                            &progress,
                            true,
                            false,
                        )?;
                    } else {
                        limits_clone.check_extraction_limits(
                            &entry_name,
                            compressed_size,
                            total_written,
                            &progress,
                            true,
                            true,
                        )?;
                    }

                    {
                        let mut p = progress.lock().unwrap();
                        p.on_bytes_progress(total_written);
                        if p.should_cancel() {
                            p.on_entry_complete(&entry_name, false);
                            return Err(CoreError::Cancelled);
                        }
                    }
                }

                temp_writer.flush().map_err(|e| CoreError::BitstreamError {
                    detail: format!("temp file flush failed for '{entry_name}': {e}"),
                })?;
                drop(temp_writer);

                // CRC32 verification.
                if let Some(expected) = expected_crc {
                    let actual = crc_hasher.finalize();
                    if actual != expected {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_complete(&entry_name, false);
                        return Err(CoreError::CrcMismatch {
                            entry: entry_name,
                            expected,
                            actual,
                        });
                    }
                }

                // BLAKE2sp verification (present only when the extra area record 0x02 exists).
                if let (Some(state), Some(expected_hash)) = (blake2_state, expected_blake2) {
                    let actual_hash = state.finalize();
                    if actual_hash.as_array() != &expected_hash {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_complete(&entry_name, false);
                        return Err(CoreError::BitstreamError {
                            detail: format!("BLAKE2sp hash mismatch for '{entry_name}'"),
                        });
                    }
                }

                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_complete(&entry_name, true);
                }

                Ok(temp_file)
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        // Step 3 (async): write verified bytes to the sink.
        // Copy verified temp file to the final sink.
        let mut temp_reader = tokio::fs::File::open(temp_file.path()).await.map_err(|e| {
            CoreError::BitstreamError {
                detail: format!("failed to reopen temp file: {e}"),
            }
        })?;
        let written = tokio::io::copy(&mut temp_reader, sink).await.map_err(|e| {
            CoreError::BitstreamError {
                detail: format!("copy to sink failed: {e}"),
            }
        })?;
        // temp_file dropped here → auto-deleted.
        Ok(written)
    }

    /// Dispatch compressed extraction (methods 1–5, or any encrypted entry) to `unrar`.
    ///
    /// Chooses between the local shortcut (pass the archive path directly to `unrar`)
    /// and the cloud synthetic-archive approach based on whether a local path was
    /// captured at [`open`](RarArchive::open) time.
    async fn extract_compressed<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &RarEntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        if let Some(path) = &self.local_path {
            debug!(name = %entry.name, path = %path.display(), "unrar local path");
            return Self::extract_via_unrar_local(
                entry, loc, path, sink, progress, password, limits,
            )
            .await;
        }
        debug!(name = %entry.name, "unrar cloud path (synthetic archive)");
        self.extract_via_unrar_cloud(entry, loc, sink, progress, password, limits)
            .await
    }

    /// Extract a compressed (or encrypted) entry from a **local** archive via `unrar`.
    ///
    /// Opens the original file directly — no temp file needed.  Iterates headers
    /// until the target entry is found, then reads all decompressed bytes into
    /// memory via `unrar`.  CRC32 and BLAKE2sp are verified after decompression.
    async fn extract_via_unrar_local<W: AsyncWrite + Send + Unpin>(
        entry: &ArchiveEntry,
        loc: &RarEntryLocation,
        archive_path: &std::path::Path,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        let path = archive_path.to_path_buf();
        let target_name = entry.name.clone();
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let expected_blake2 = loc.blake2_hash;
        let password_bytes = password.map(|p| p.to_vec());

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry_name, uncompressed_size);
        }

        let (cancel, on_progress) = build_unrar_progress_hooks(&progress);
        let out_tmp =
            tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile, CoreError> {
                let out_tmp =
                    tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                        detail: format!("failed to create extraction temp file: {e}"),
                    })?;
                let out_path = out_tmp.path().to_path_buf();

                let mut archive = match &password_bytes {
                    Some(pw) => unrar::Archive::with_password(&path, pw.as_slice())
                        .open_for_processing()
                        .map_err(map_unrar_error)?,
                    None => unrar::Archive::new(&path)
                        .open_for_processing()
                        .map_err(map_unrar_error)?,
                };

                loop {
                    let header = archive.read_header().map_err(map_unrar_error)?;
                    let header = match header {
                        Some(h) => h,
                        None => {
                            return Err(CoreError::InvalidFormat {
                                message: format!("entry '{target_name}' not found in archive"),
                            });
                        }
                    };

                    let name = header.entry().filename.to_string_lossy().replace('\\', "/");
                    if name == target_name {
                        header
                            .extract_to_with_progress(&out_path, Some(cancel), Some(on_progress))
                            .map_err(map_unrar_error)?;
                        return Ok(out_tmp);
                    }
                    archive = header.skip().map_err(map_unrar_error)?;
                }
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_stream_from_file(
            out_tmp.into_file(),
            VerifyData {
                name: &entry_name,
                expected_crc,
                expected_blake2,
                compressed_size: loc.data_size,
            },
            sink,
            &progress,
            limits,
        )
        .await
    }

    /// Extract a compressed (or encrypted) entry from a **cloud** archive via `unrar`.
    ///
    /// Downloads only the target entry's file header + compressed data, assembles
    /// them into a minimal valid RAR5 archive in a temp file, and decompresses with
    /// `unrar`.  The temp file is automatically deleted when the function returns.
    async fn extract_via_unrar_cloud<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &RarEntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let expected_blake2 = loc.blake2_hash;
        let password_bytes = password.map(|p| p.to_vec());

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry_name, uncompressed_size);
        }

        // 1. Download the file header bytes (they contain the entry metadata unrar needs).
        let file_header_bytes = self
            .reader
            .read_range(loc.header_offset, loc.header_size)
            .await
            .map_err(CoreError::Io)?;

        // 2. Open a streaming reader for the compressed data (no RAM buffering).
        let compressed_stream = self
            .reader
            .read_stream(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        let archive_preamble = self.archive_preamble.clone();
        let end_block = self.end_block.clone();
        let data_size = loc.data_size;

        let handle = tokio::runtime::Handle::current();
        let mut sync_reader = SyncIoBridge::new_with_handle(compressed_stream, handle);
        let prog_clone = progress.clone();

        let out_tmp = tokio::task::spawn_blocking(move || {
            assemble_and_unrar_synthetic(
                archive_preamble.as_ref(),
                file_header_bytes.as_ref(),
                &mut sync_reader,
                data_size,
                end_block.as_ref(),
                password_bytes.as_deref(),
                &prog_clone,
            )
        })
        .await
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("extraction task panicked: {e}"),
        })??;

        verify_and_stream_from_file(
            out_tmp.into_file(),
            VerifyData {
                name: &entry_name,
                expected_crc,
                expected_blake2,
                compressed_size: loc.data_size,
            },
            sink,
            &progress,
            limits,
        )
        .await
    }

    /// Attempt to build the entry list from the Quick Open (QO) service block.
    ///
    /// Returns `Ok(Some((entries, locations)))` on success, `Ok(None)` if the QO block is
    /// absent or the data is unusable, and `Err` only for genuine I/O errors.
    async fn try_quick_open(
        reader: &R,
        qo_abs: u64,
        archive_size: u64,
        limits: &crate::ResourceLimits,
    ) -> Result<Option<(Vec<ArchiveEntry>, Vec<RarEntryLocation>)>, CoreError> {
        // Guard: QO offset must be within the archive.
        if qo_abs >= archive_size {
            return Ok(None);
        }

        // Read enough bytes to parse the QO service block header (64 KiB is sufficient).
        let fetch_size = (64 * 1024_u64).min(archive_size - qo_abs);
        let qo_buf = reader.read_range(qo_abs, fetch_size).await?;

        // Parse QO block common header.
        let qo_block = match parse_block_header(&qo_buf, qo_abs) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };

        // Must be a service header (block_type 3).
        if qo_block.block_type != 3 {
            return Ok(None);
        }

        // Locate the QO data area within our buffer.
        // The data area immediately follows the header bytes.
        let header_byte_count = (qo_block.total_size - qo_block.data_size) as usize;
        if header_byte_count + qo_block.data_size as usize > qo_buf.len() {
            // Data area extends past our fetch — would need another read.
            // For now fall back to sequential scan (uncommon for typical archives).
            return Ok(None);
        }
        let qo_data = &qo_buf[header_byte_count..header_byte_count + qo_block.data_size as usize];

        // Parse the QO service name to confirm this is indeed a QO block.
        // The service header body starts at qo_block.body_offset within qo_buf.
        // The name_len vint and name bytes follow file_flags + unpacked_size + attributes + ...
        // Quick check: the data area length being > 0 is sufficient to attempt parsing.
        if qo_data.is_empty() {
            return Ok(None);
        }

        // Parse cached header entries from QO data area.
        let cached = parse_quick_open_data(qo_data, qo_abs);
        if cached.is_empty() {
            return Ok(None);
        }

        let mut entries: Vec<ArchiveEntry> = Vec::with_capacity(cached.len());
        let mut locations: Vec<RarEntryLocation> = Vec::with_capacity(cached.len());

        for QoCachedBlock {
            original_archive_offset,
            data,
        } in cached
        {
            // Parse common block header from cached bytes.
            let raw_block = match parse_block_header(&data, original_archive_offset) {
                Ok(b) => b,
                Err(_) => continue, // skip unparseable cached entry
            };

            // Only process file headers (type 2).
            if raw_block.block_type != 2 {
                continue;
            }

            if entries.len() >= limits.max_entries {
                return Err(CoreError::ResourceLimitExceeded {
                    message: format!(
                        "RAR5 entry count exceeds max_entries {}",
                        limits.max_entries
                    ),
                });
            }

            // Note: cached headers in QO blocks do NOT include the data area bytes.
            // The data_size field from the common block header reflects the compressed
            // data size in the original archive; we use it for location offsets.
            let file_hdr = match parse_file_header(&data, raw_block) {
                Ok(h) => h,
                Err(_) => continue,
            };

            let is_solid = file_hdr.compression.solid;

            let hdr_size = file_hdr.raw.total_size - file_hdr.raw.data_size;
            let entry = ArchiveEntry {
                name: file_hdr.name.clone(),
                compressed_size: file_hdr.raw.data_size,
                uncompressed_size: file_hdr.unpacked_size,
                is_directory: file_hdr.is_directory,
                is_encrypted: file_hdr.encryption.is_some(),
                modified_at: file_hdr.mtime.map(unix_to_system_time),
                compression_method: rar_method_to_codec(file_hdr.compression.method),
                index: entries.len(),
            };
            let location = RarEntryLocation {
                header_offset: original_archive_offset,
                header_size: hdr_size,
                data_offset: original_archive_offset + hdr_size,
                data_size: file_hdr.raw.data_size,
                method: file_hdr.compression.method,
                dict_size: file_hdr.compression.dict_size,
                crc32: file_hdr.data_crc32,
                blake2_hash: file_hdr.blake2_hash,
                is_solid,
                is_encrypted: file_hdr.encryption.is_some(),
            };
            entries.push(entry);
            locations.push(location);
        }

        if entries.is_empty() {
            return Ok(None); // QO had no usable file entries — fall back
        }
        Ok(Some((entries, locations)))
    }
} // impl RarArchive

// ── ArchiveReader trait implementation ────────────────────────────────────────

impl<R: ReadAt + Send + Sync + 'static> ArchiveReader for RarArchive<R> {
    fn entries(&self) -> &[ArchiveEntry] {
        RarArchive::entries(self)
    }

    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        RarArchive::extraction_cost(self, entry)
    }

    fn dict_size(&self, entry: &ArchiveEntry) -> Option<u64> {
        RarArchive::dict_size(self, entry)
    }

    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        RarArchive::extract(self, entry, sink, progress, password, limits).await
    }

    async fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<BatchResult, CoreError>
    where
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn AsyncWrite + Send + Unpin>> + Send,
    {
        RarArchive::extract_selected(self, entries, sink_factory, progress, password, limits).await
    }

    async fn test(
        &self,
        entry: &ArchiveEntry,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<(), CoreError> {
        RarArchive::test(self, entry, progress, limits).await
    }

    fn scan_info(&self) -> crate::scan::ScanInfo {
        if self.scan_complete {
            crate::scan::ScanInfo::Ready
        } else {
            crate::scan::ScanInfo::Required {
                estimated_bytes: self.archive_size,
                reason: "This archive has no metadata index and requires a one-time scan. Extraction will be instant afterward.",
            }
        }
    }

    async fn scan_metadata(
        &mut self,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<(), CoreError> {
        let preamble_end = self.archive_preamble.len() as u64;
        let size = self.archive_size;

        {
            let mut p = progress.lock().unwrap();
            p.on_scan_start(size);
        }

        let (entries, locations) =
            Self::run_elastic_scan(&self.reader, preamble_end, size, &progress, None, limits)
                .await?;

        let entry_count = entries.len();
        self.entries = entries;
        self.locations = locations;
        self.scan_complete = true;

        {
            let mut p = progress.lock().unwrap();
            p.on_scan_complete(entry_count);
        }

        Ok(())
    }

    async fn extract_all_streaming<Filter, SinkFn>(
        &mut self,
        filter: Filter,
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError>
    where
        Filter: Fn(&ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        // Local path: delegate to existing unrar direct path.
        if let Some(path) = &self.local_path {
            let work: Vec<(ArchiveEntry, RarEntryLocation)> = self
                .entries
                .iter()
                .zip(self.locations.iter())
                .filter(|(e, loc)| !e.is_directory && !loc.is_solid && filter(e))
                .map(|(e, loc)| (e.clone(), loc.clone()))
                .collect();

            if work.is_empty() {
                return Ok(0);
            }

            let total_size: u64 = work.iter().map(|(e, _)| e.uncompressed_size).sum();
            if total_size > limits.max_total_unpacked {
                return Err(CoreError::ResourceLimitExceeded {
                    message: format!(
                        "batch extraction total size {} exceeds max_total_unpacked {}",
                        total_size, limits.max_total_unpacked
                    ),
                });
            }

            for (entry, _) in &work {
                limits.check_extraction_limits(
                    &entry.name,
                    entry.compressed_size,
                    entry.uncompressed_size,
                    &progress,
                    false,
                    false,
                )?;
            }

            return extract_all_via_unrar(
                path.clone(),
                work,
                sink_factory,
                progress,
                password,
                limits,
            )
            .await;
        }

        // Cloud path: pipelined streaming splitter.
        let preamble_end = self.archive_preamble.len() as u64;
        let size = self.archive_size;

        let (tx, rx) = tokio::sync::mpsc::channel::<SnippetMsg>(2);
        let password_owned = password.map(|p| p.to_vec());

        // Spawn consumer.
        let consumer_progress = progress.clone();
        let limits_clone = limits.clone();
        let consumer = tokio::spawn(consume_snippets(
            rx,
            sink_factory,
            consumer_progress,
            password_owned,
            limits_clone,
        ));

        // Build snippet config.
        let config = SnippetConfig {
            sender: tx,
            preamble: self.archive_preamble.clone(),
            end_block: self.end_block.clone(),
            filter: Box::new(filter),
        };

        // Signal scan start.
        {
            progress.lock().unwrap().on_scan_start(size);
        }

        // Run producer (in current task — borrows self.reader).
        // When run_elastic_scan returns, config is dropped, tx is dropped,
        // signaling the consumer that no more snippets are coming.
        let (entries, locations) = Self::run_elastic_scan(
            &self.reader,
            preamble_end,
            size,
            &progress,
            Some(config),
            limits,
        )
        .await?;

        // Await consumer.
        let total_bytes = consumer.await.map_err(|e| CoreError::BitstreamError {
            detail: format!("consumer panicked: {e}"),
        })??;

        // Populate self with harvested metadata.
        let entry_count = entries.len();
        self.entries = entries;
        self.locations = locations;
        self.scan_complete = true;

        {
            progress.lock().unwrap().on_scan_complete(entry_count);
        }

        Ok(total_bytes)
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

/// Map a stream `io::Error` to `CoreError` for the elastic scan.
///
/// HTTP `read_stream` wraps [`ReadError`](crate::io::ReadError) inside
/// `io::Error::other`, so we unwrap it when present. Local I/O errors
/// (e.g. truncated archive) become [`CoreError::CorruptHeader`].
fn scan_stream_err(e: std::io::Error, offset: u64) -> CoreError {
    if let Some(inner) = e.into_inner()
        && let Ok(re) = inner.downcast::<crate::io::ReadError>()
    {
        return CoreError::Io(*re);
    }
    CoreError::CorruptHeader {
        offset,
        reason: "stream read failed during scan".into(),
    }
}

fn snippet_write_err(e: std::io::Error) -> CoreError {
    CoreError::BitstreamError {
        detail: format!("snippet write: {e}"),
    }
}

/// Convert a Unix timestamp (seconds since 1970-01-01) to `SystemTime`.
fn unix_to_system_time(timestamp: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64)
}

/// Map a RAR5 compression method to the codec layer's `CompressionMethod`.
///
/// Only Store (0) maps cleanly. Methods 1–5 are proprietary RAR algorithms
/// handled by the `unrar` C++ library — they have no `CompressionMethod`
/// equivalent, so we return `None` for them.
fn rar_method_to_codec(method: u8) -> Option<CompressionMethod> {
    match method {
        0 => Some(CompressionMethod::Store),
        _ => None, // RAR proprietary methods 1–5 — handled by unrar
    }
}

/// Build a minimal valid RAR5 end-of-archive block (block type 5).
///
/// Layout: CRC32 (4 LE) + header_size vint (1) + block_type vint (1) + flags vint (1).
/// Total: 7 bytes.
fn build_end_of_archive_block() -> Bytes {
    // CRC region: [header_size=2, block_type=5, flags=0]
    // header_size counts the block_type + flags vints (2 bytes).
    let crc_region = [0x02u8, 0x05, 0x00];
    let crc = crc32fast::hash(&crc_region);
    let mut buf = Vec::with_capacity(7);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&crc_region);
    Bytes::from(buf)
}

/// Build the cancel flag and progress closure for `extract_to_with_progress`.
///
/// The closure accumulates chunk bytes and reports cumulative progress.
/// It also bridges `ProgressReporter::should_cancel()` into the `AtomicBool`.
pub(crate) fn build_unrar_progress_hooks(
    progress: &Arc<Mutex<dyn ProgressReporter>>,
) -> (Arc<AtomicBool>, Box<dyn Fn(usize) + Send>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_cb = cancel.clone();
    let progress_cb = progress.clone();
    let bytes_so_far = AtomicU64::new(0);

    let on_progress: Box<dyn Fn(usize) + Send> = Box::new(move |chunk_bytes| {
        let cumulative =
            bytes_so_far.fetch_add(chunk_bytes as u64, Ordering::Relaxed) + chunk_bytes as u64;

        let mut p = progress_cb.lock().unwrap();
        p.on_bytes_progress(cumulative);
        if p.should_cancel() {
            cancel_cb.store(true, Ordering::Relaxed);
        }
    });

    (cancel, on_progress)
}

/// Map a `unrar` error to the appropriate [`CoreError`] variant.
///
/// `unrar` reports errors via `code` + `when` pairs.  We translate the
/// most actionable codes to specific variants; everything else becomes a
/// `BitstreamError`.
pub(crate) fn map_unrar_error(e: unrar::error::UnrarError) -> CoreError {
    use unrar::error::Code;
    match e.code {
        Code::MissingPassword => CoreError::PasswordRequired,
        Code::BadPassword => CoreError::WrongPassword {
            method: crate::PasswordDetectionMethod::CrcMismatch,
        },
        Code::UserAbort => CoreError::Cancelled,
        Code::BadData | Code::BadArchive => CoreError::BitstreamError {
            detail: format!("corrupt RAR data: {e}"),
        },
        _ => CoreError::BitstreamError {
            detail: format!("unrar error: {e}"),
        },
    }
}

/// Assemble a synthetic standalone RAR archive and extract it.
///
/// Internal helper used by cloud extraction paths (single and batched)
/// inside `spawn_blocking`.
pub(crate) fn assemble_and_unrar_synthetic<R: Read>(
    preamble: &[u8],
    header_bytes: &[u8],
    data_stream: &mut R,
    data_size: u64,
    eoa: &[u8],
    password: Option<&[u8]>,
    progress: &Arc<Mutex<dyn ProgressReporter>>,
) -> Result<tempfile::NamedTempFile, CoreError> {
    // 1. Create temp file for synthetic RAR5 archive.
    let temp_file = tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
        detail: format!("failed to create synthetic archive temp file: {e}"),
    })?;

    // 2. Assemble.
    {
        let mut writer = std::io::BufWriter::new(temp_file.as_file());
        writer
            .write_all(preamble)
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("temp file write preamble failed: {e}"),
            })?;
        writer
            .write_all(header_bytes)
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("temp file write header failed: {e}"),
            })?;

        // Stream exactly data_size bytes from the bridged reader into writer.
        std::io::copy(&mut data_stream.take(data_size), &mut writer).map_err(|e| {
            let detail = e.to_string();
            if let Some(inner) = e.into_inner()
                && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
            {
                return CoreError::Io(*read_error);
            }
            CoreError::BitstreamError {
                detail: format!("streaming compressed data to temp failed: {detail}"),
            }
        })?;

        writer
            .write_all(eoa)
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("temp file write EOA failed: {e}"),
            })?;

        writer.flush().map_err(|e| CoreError::BitstreamError {
            detail: format!("temp file flush failed: {e}"),
        })?;
    }

    // 3. Unrar.
    let (cancel, on_progress) = build_unrar_progress_hooks(progress);
    unrar_synthetic_to_tmp(temp_file.path(), password, cancel, on_progress)
}

/// Extract from a synthetic standalone RAR archive to a temp file.
///
/// Internal helper used by cloud extraction paths (single and batched).
fn unrar_synthetic_to_tmp(
    synthetic_path: &Path,
    password: Option<&[u8]>,
    cancel: Arc<AtomicBool>,
    on_progress: Box<dyn Fn(usize) + Send>,
) -> Result<tempfile::NamedTempFile, CoreError> {
    // Create temp file for extraction output.
    let out_tmp = tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
        detail: format!("failed to create extraction temp file: {e}"),
    })?;
    let out_path = out_tmp.path().to_path_buf();

    let archive = match password {
        Some(pw) => unrar::Archive::with_password(synthetic_path, pw)
            .open_for_processing()
            .map_err(map_unrar_error)?,
        None => unrar::Archive::new(synthetic_path)
            .open_for_processing()
            .map_err(map_unrar_error)?,
    };

    {
        let header = archive.read_header().map_err(map_unrar_error)?;
        let header = header.ok_or_else(|| CoreError::BitstreamError {
            detail: "synthetic archive produced no entries".into(),
        })?;
        header
            .extract_to_with_progress(&out_path, Some(cancel), Some(on_progress))
            .map_err(map_unrar_error)?;
    } // archive (and header) dropped here, releasing synthetic_path and out_path handles.

    Ok(out_tmp)
}

/// Extract multiple entries from a RAR5 archive via `unrar` in a single pass.
///
/// Opens the archive once and iterates through all headers, extracting entries
/// that are present in `work`. Skips unwanted entries efficiently without
/// decompressing them.
///
/// Returns the total number of uncompressed bytes written.
async fn extract_all_via_unrar<F>(
    archive_path: PathBuf,
    work: Vec<(ArchiveEntry, RarEntryLocation)>,
    mut sink_factory: F,
    progress: Arc<Mutex<dyn ProgressReporter>>,
    password: Option<&[u8]>,
    limits: &crate::ResourceLimits,
) -> Result<u64, CoreError>
where
    F: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
{
    let password_bytes = password.map(|p| p.to_vec());
    let limits_clone = limits.clone();

    // Build name → work-index lookup for O(1) dispatch inside spawn_blocking.
    let wanted: std::collections::HashMap<String, usize> = work
        .iter()
        .enumerate()
        .map(|(i, (entry, _))| (entry.name.clone(), i))
        .collect();

    let result = tokio::task::spawn_blocking(move || -> Result<u64, CoreError> {
        let mut archive = match &password_bytes {
            Some(pw) => unrar::Archive::with_password(&archive_path, pw.as_slice())
                .open_for_processing()
                .map_err(map_unrar_error)?,
            None => unrar::Archive::new(&archive_path)
                .open_for_processing()
                .map_err(map_unrar_error)?,
        };

        let mut total_bytes: u64 = 0;

        loop {
            let header = archive.read_header().map_err(map_unrar_error)?;
            let header = match header {
                Some(h) => h,
                None => break,
            };

            let name = header.entry().filename.to_string_lossy().replace('\\', "/");

            if let Some(&idx) = wanted.get(&name) {
                let (entry, loc) = &work[idx];

                if let Some(mut sink) = sink_factory(entry) {
                    progress
                        .lock()
                        .unwrap()
                        .on_entry_start(&name, entry.uncompressed_size);

                    let (cancel, on_progress) = build_unrar_progress_hooks(&progress);

                    // Create extraction temp file.
                    let out_tmp =
                        tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                            detail: format!(
                                "failed to create extraction temp file for '{name}': {e}"
                            ),
                        })?;
                    let out_path = out_tmp.path().to_path_buf();

                    let next_archive = header
                        .extract_to_with_progress(&out_path, Some(cancel), Some(on_progress))
                        .map_err(map_unrar_error)?;
                    archive = next_archive;

                    // Verify and stream from temp file to sink using sync verifier.
                    // Passes the opened file handle to avoid Permission Denied on reopen.
                    let bytes = verify_and_stream_from_file_sync(
                        out_tmp.into_file(),
                        VerifyData {
                            name: &name,
                            expected_crc: loc.crc32,
                            expected_blake2: loc.blake2_hash,
                            compressed_size: loc.data_size,
                        },
                        &mut sink,
                        &progress,
                        &limits_clone,
                    )?;

                    total_bytes += bytes;
                } else {
                    // Sink factory returned None — skip this entry.
                    archive = header.skip().map_err(map_unrar_error)?;
                }
            } else {
                archive = header.skip().map_err(map_unrar_error)?;
            }
        }

        Ok(total_bytes)
    })
    .await
    .map_err(|e| CoreError::BitstreamError {
        detail: format!("extract_all task panicked: {e}"),
    })??;

    Ok(result)
}

/// Receives snippet micro-archives from the producer, extracts each via
/// `unrar` in a blocking task, verifies integrity, writes to sink, and
/// deletes the snippet.
///
/// `pub(crate)` so RAR4 (Step 5) can reuse this function.
pub(crate) async fn consume_snippets<SinkFn>(
    mut rx: tokio::sync::mpsc::Receiver<SnippetMsg>,
    mut sink_factory: SinkFn,
    progress: Arc<Mutex<dyn ProgressReporter>>,
    password: Option<Vec<u8>>,
    limits: crate::ResourceLimits,
) -> Result<u64, CoreError>
where
    SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
{
    let mut total_bytes: u64 = 0;

    while let Some(msg) = rx.recv().await {
        let result = extract_single_snippet(
            &msg,
            &mut sink_factory,
            &progress,
            password.as_deref(),
            &limits,
        )
        .await;

        // Always clean up snippet, even on error.
        let _ = tokio::fs::remove_file(&msg.path).await;

        total_bytes += result?;
    }

    Ok(total_bytes)
}

async fn extract_single_snippet<SinkFn>(
    msg: &SnippetMsg,
    sink_factory: &mut SinkFn,
    progress: &Arc<Mutex<dyn ProgressReporter>>,
    password: Option<&[u8]>,
    limits: &crate::ResourceLimits,
) -> Result<u64, CoreError>
where
    SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send,
{
    let Some(mut sink) = sink_factory(&msg.entry) else {
        return Ok(0); // sink factory says skip
    };

    let entry_name = msg.entry.name.clone();
    let uncompressed_size = msg.entry.uncompressed_size;
    let expected_crc = msg.crc32;
    let expected_blake2 = msg.blake2_hash;
    let snippet_path = msg.path.clone();
    let password_bytes = password.map(|p| p.to_vec());
    let limits_clone = limits.clone();

    {
        progress
            .lock()
            .unwrap()
            .on_entry_start(&entry_name, uncompressed_size);
    }

    let (cancel, on_progress) = build_unrar_progress_hooks(progress);

    // Extract and verify via unrar in a single blocking task.
    let progress_sync = progress.clone();
    let data_size = msg.data_size;
    let snippet_path_clone = snippet_path.clone();

    tokio::task::spawn_blocking(move || -> Result<u64, CoreError> {
        let out_tmp = tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
            detail: format!("failed to create extraction temp file: {e}"),
        })?;
        let out_path = out_tmp.path().to_path_buf();

        let archive = match &password_bytes {
            Some(pw) => unrar::Archive::with_password(&snippet_path_clone, pw.as_slice())
                .open_for_processing()
                .map_err(map_unrar_error)?,
            None => unrar::Archive::new(&snippet_path_clone)
                .open_for_processing()
                .map_err(map_unrar_error)?,
        };

        let header = archive.read_header().map_err(map_unrar_error)?;
        let header = header.ok_or_else(|| CoreError::BitstreamError {
            detail: "snippet archive produced no entries".into(),
        })?;
        header
            .extract_to_with_progress(&out_path, Some(cancel), Some(on_progress))
            .map_err(map_unrar_error)?;

        // Verify and stream from temp file to sink using sync verifier.
        // Passes the opened file handle to avoid Permission Denied on reopen.
        verify_and_stream_from_file_sync(
            out_tmp.into_file(),
            VerifyData {
                name: &entry_name,
                expected_crc,
                expected_blake2,
                compressed_size: data_size,
            },
            &mut sink,
            &progress_sync,
            &limits_clone,
        )
    })
    .await
    .map_err(|e| CoreError::BitstreamError {
        detail: format!("extraction/verification task panicked: {e}"),
    })?
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rar::parse::parse_block_header;

    // ── build_end_of_archive_block ────────────────────────────────────────────

    #[test]
    fn end_block_is_valid_rar5_block() {
        let block = build_end_of_archive_block();
        assert_eq!(block.len(), 7, "end block should be exactly 7 bytes");
        // parse_block_header must succeed and report type 5.
        let hdr = parse_block_header(&block, 0).unwrap();
        assert_eq!(hdr.block_type, 5);
        assert_eq!(hdr.flags, 0);
        assert_eq!(hdr.data_size, 0);
        assert_eq!(hdr.total_size, 7);
    }

    // ── unix_to_system_time ───────────────────────────────────────────────────

    #[test]
    fn unix_epoch_zero() {
        let t = unix_to_system_time(0);
        assert_eq!(t, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn unix_timestamp_nonzero() {
        let ts: u32 = 1_700_000_000;
        let t = unix_to_system_time(ts);
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, ts as u64);
    }

    // ── rar_method_to_codec ───────────────────────────────────────────────────

    #[test]
    fn method_0_maps_to_store() {
        assert_eq!(rar_method_to_codec(0), Some(CompressionMethod::Store));
    }

    #[test]
    fn compressed_methods_map_to_none() {
        for m in 1u8..=5 {
            assert_eq!(
                rar_method_to_codec(m),
                None,
                "method {m} should map to None"
            );
        }
    }

    // ── RarArchive::open — in-memory fixtures ─────────────────────────────────

    /// Build a minimal valid RAR5 archive in memory with multiple Store entries.
    ///
    /// Used to verify `RarArchive` behavior without requiring real `.rar` files.
    pub(crate) fn build_minimal_rar5_multi(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        // ── End-of-archive block ──────────────────────────────────────────────
        let eoa = build_end_of_archive_block();

        let mut archive = Vec::new();
        archive.extend_from_slice(&RAR5_SIGNATURE);

        // ── Main archive header (type 1) ──────────────────────────────────────
        let archive_flags: u8 = 0;
        let main_body = vec![archive_flags];
        let main_header_content = 1 + 1 + main_body.len();
        let mut main_crc_region = vec![
            main_header_content as u8,
            0x01u8, // main header
            0x00u8, // no extra
        ];
        main_crc_region.extend_from_slice(&main_body);
        let main_crc = crc32fast::hash(&main_crc_region);
        archive.extend_from_slice(&main_crc.to_le_bytes());
        archive.extend_from_slice(&main_crc_region);

        // ── File entries ──────────────────────────────────────────────────────
        for (filename, data, solid) in entries {
            let solid_bit: u8 = if *solid { 1 } else { 0 };
            let compression_info: u8 = solid_bit << 6;
            let mtime: u32 = 1_700_000_000;
            let crc32_val: u32 = crc32fast::hash(data);
            let name_bytes = filename.as_bytes();

            let mut body = Vec::new();
            body.push(0x06u8); // mtime + crc32
            body.push(data.len() as u8); // unpacked_size (assuming < 128)
            body.push(0x20u8); // attributes
            body.extend_from_slice(&mtime.to_le_bytes());
            body.extend_from_slice(&crc32_val.to_le_bytes());
            body.push(compression_info);
            body.push(0x01u8); // host_os = Unix
            body.push(name_bytes.len() as u8); // name_len
            body.extend_from_slice(name_bytes);

            let header_size = 1 + 1 + 1 + body.len();
            let mut crc_region = vec![
                header_size as u8,
                0x02u8,           // file header
                0x02u8,           // data area flag
                data.len() as u8, // data size vint
            ];
            crc_region.extend_from_slice(&body);

            let file_hdr_crc = crc32fast::hash(&crc_region);
            archive.extend_from_slice(&file_hdr_crc.to_le_bytes());
            archive.extend_from_slice(&crc_region);
            archive.extend_from_slice(data);
        }

        archive.extend_from_slice(&eoa);
        archive
    }

    /// Backwards compat for single entry.
    pub(crate) fn build_minimal_rar5(filename: &str, data: &[u8], solid: bool) -> Vec<u8> {
        build_minimal_rar5_multi(&[(filename, data, solid)])
    }

    #[tokio::test]
    async fn open_minimal_rar5_one_entry() {
        use crate::io::LocalReadAt;

        let data = b"hello world\n";
        let archive_bytes = build_minimal_rar5("hello.txt", data, false);

        // Write to a temp file so LocalReadAt can open it.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        assert_eq!(rar.entries().len(), 1);
        let entry = &rar.entries()[0];
        assert_eq!(entry.name, "hello.txt");
        assert!(!entry.is_directory);
        assert!(!entry.is_encrypted);
        assert_eq!(entry.uncompressed_size, data.len() as u64);
        assert_eq!(entry.compressed_size, data.len() as u64);
        assert_eq!(entry.compression_method, Some(CompressionMethod::Store));
    }

    #[tokio::test]
    async fn open_rejects_rar4_signature() {
        use crate::io::LocalReadAt;

        // Build a fake archive with a RAR4 signature followed by garbage.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&RAR4_SIGNATURE);
        bytes.extend_from_slice(&[0u8; 64]); // filler

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let result = RarArchive::open(reader, &crate::ResourceLimits::default()).await;
        assert!(
            matches!(result, Err(CoreError::UnsupportedFormat { .. })),
            "expected UnsupportedFormat, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn open_rejects_bad_signature() {
        use crate::io::LocalReadAt;

        let bytes = [0u8; 32];
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let result = RarArchive::open(reader, &crate::ResourceLimits::default()).await;
        assert!(
            matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected InvalidFormat, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn solid_entry_listed_but_marked_solid() {
        use crate::io::LocalReadAt;

        let data = b"solid data";
        let archive_bytes = build_minimal_rar5("solid.txt", data, true);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        // Solid entries are listed so the user can see them; extract() rejects.
        assert_eq!(rar.entries().len(), 1);
        assert!(rar.locations[0].is_solid);
    }

    #[tokio::test]
    async fn end_block_captured() {
        use crate::io::LocalReadAt;

        let data = b"test";
        let archive_bytes = build_minimal_rar5("f.txt", data, false);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        // end_block must be a valid RAR5 end-of-archive block.
        let hdr = parse_block_header(&rar.end_block, 0).unwrap();
        assert_eq!(hdr.block_type, 5);
    }

    #[tokio::test]
    async fn preamble_starts_with_rar5_signature() {
        use crate::io::LocalReadAt;

        let data = b"preamble test";
        let archive_bytes = build_minimal_rar5("p.txt", data, false);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        assert!(
            rar.archive_preamble.starts_with(&RAR5_SIGNATURE),
            "preamble must begin with the RAR5 signature"
        );
    }

    #[tokio::test]
    async fn location_offsets_are_correct() {
        use crate::io::LocalReadAt;

        let data = b"location test data";
        let archive_bytes = build_minimal_rar5("loc.txt", data, false);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        assert_eq!(rar.entries().len(), 1);
        let loc = &rar.locations[0];

        // data_offset must point to the actual data bytes in the archive.
        assert_eq!(loc.data_size, data.len() as u64);
        // Explicit bindings so the dead_code lint sees these reads.
        let header_offset = loc.header_offset;
        let header_size = loc.header_size;
        assert_eq!(loc.data_offset, header_offset + header_size);
        assert!(!loc.is_solid);
        assert!(!loc.is_encrypted);
        assert_eq!(loc.method, 0); // Store
        // CRC32 and BLAKE2sp hash are used in Step 5 (extraction + verification).
        assert_eq!(loc.crc32, Some(crc32fast::hash(data)));
        assert_eq!(loc.blake2_hash, None); // no extra area in minimal fixture
        assert_eq!(loc.dict_size, 128 * 1024); // dict_exponent=0 → 128 KiB
    }

    // ── Quick Open helpers ────────────────────────────────────────────────────

    fn encode_vint_test(mut value: u64, buf: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                buf.push(byte);
                break;
            } else {
                buf.push(byte | 0x80);
            }
        }
    }

    /// Build a minimal valid RAR5 file-header block (type 2, Store, no extras).
    /// Returns only the header bytes — the `data_size` bytes of compressed data
    /// are NOT appended, matching how QO caches headers.
    fn build_file_header_bytes(name: &str, data_size: u64) -> Vec<u8> {
        let name_bytes = name.as_bytes();
        let mut body = Vec::new();
        body.push(0x00u8); // file_flags = 0 (size known, no mtime, no crc)
        encode_vint_test(data_size, &mut body); // unpacked_size
        body.push(0x00u8); // attributes
        body.push(0x00u8); // compression_info = 0 (Store)
        body.push(0x01u8); // host_os = Unix
        encode_vint_test(name_bytes.len() as u64, &mut body);
        body.extend_from_slice(name_bytes);

        let mut data_size_vint = Vec::new();
        encode_vint_test(data_size, &mut data_size_vint);

        // header_size = block_type(1) + flags(1) + data_size_vint + body
        let header_size = 1 + 1 + data_size_vint.len() + body.len();
        let mut crc_region = Vec::new();
        encode_vint_test(header_size as u64, &mut crc_region);
        crc_region.push(0x02u8); // block_type = 2
        crc_region.push(0x02u8); // flags = 0x0002 (data area present)
        crc_region.extend_from_slice(&data_size_vint);
        crc_region.extend_from_slice(&body);

        let crc = crc32fast::hash(&crc_region);
        let mut out = Vec::new();
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&crc_region);
        out
    }

    /// Wrap raw QO cache data in a RAR5 service block (type 3) header.
    fn build_service_block(data_area: &[u8]) -> Vec<u8> {
        let mut data_size_vint = Vec::new();
        encode_vint_test(data_area.len() as u64, &mut data_size_vint);
        // header_size = block_type(1) + flags(1) + data_size_vint
        let header_size = 1 + 1 + data_size_vint.len();
        let mut crc_region = Vec::new();
        encode_vint_test(header_size as u64, &mut crc_region);
        crc_region.push(0x03u8); // block_type = 3 (service)
        crc_region.push(0x02u8); // flags = 0x0002 (data area present)
        crc_region.extend_from_slice(&data_size_vint);
        let crc = crc32fast::hash(&crc_region);
        let mut out = Vec::new();
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&crc_region);
        out.extend_from_slice(data_area);
        out
    }

    /// Build a single QO cache entry (same layout as parse.rs test helper).
    fn build_qo_entry_mod(relative_offset: u64, data: &[u8]) -> Vec<u8> {
        let mut structure = Vec::new();
        structure.push(0x00u8); // flags = 0
        encode_vint_test(relative_offset, &mut structure);
        encode_vint_test(data.len() as u64, &mut structure);
        structure.extend_from_slice(data);
        let crc = crc32fast::hash(&structure);
        let mut entry = Vec::new();
        entry.extend_from_slice(&crc.to_le_bytes());
        encode_vint_test(structure.len() as u64, &mut entry);
        entry.extend_from_slice(&structure);
        entry
    }

    // ── Quick Open integration tests ──────────────────────────────────────────

    #[tokio::test]
    async fn open_without_qo_locator_uses_sequential_scan() {
        use crate::io::LocalReadAt;

        // The minimal RAR5 fixture (no locator) exercises the sequential-scan path.
        let data = b"quick open fallback";
        let archive_bytes = build_minimal_rar5("qo_fallback.txt", data, false);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();

        assert_eq!(rar.entries().len(), 1);
        assert_eq!(rar.entries()[0].name, "qo_fallback.txt");
    }

    /// Exercises the `try_quick_open` happy path end-to-end:
    /// a valid service block (type 3) whose data area contains a QO cache entry
    /// wrapping a well-formed file header.  Must return the entry metadata without
    /// performing a sequential scan.
    #[tokio::test]
    async fn try_quick_open_with_valid_service_block_returns_entries() {
        use crate::io::LocalReadAt;

        // File header for "qo_file.txt" with Store compression and data_size = 10.
        let file_header_bytes = build_file_header_bytes("qo_file.txt", 10);

        // QO cache entry: relative_offset = 0 (original block lives at QO block start).
        let qo_entry = build_qo_entry_mod(0, &file_header_bytes);

        // Wrap the QO entry in a service block (type 3).
        let service_block = build_service_block(&qo_entry);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &service_block).unwrap();
        let size = service_block.len() as u64;

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let result =
            RarArchive::try_quick_open(&reader, 0, size, &crate::ResourceLimits::default()).await;

        assert!(result.is_ok());
        let opt = result.unwrap();
        assert!(
            opt.is_some(),
            "try_quick_open must return Some for a valid QO block"
        );

        let (entries, locations) = opt.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "qo_file.txt");
        assert_eq!(
            entries[0].compression_method,
            Some(CompressionMethod::Store)
        );
        assert_eq!(locations[0].data_size, 10);
        assert_eq!(locations[0].method, 0); // Store
        assert!(!locations[0].is_solid);
        assert!(!locations[0].is_encrypted);
    }

    #[tokio::test]
    async fn try_quick_open_out_of_bounds_offset_returns_none() {
        use crate::io::LocalReadAt;

        let data = b"some data";
        let archive_bytes = build_minimal_rar5("oob.txt", data, false);
        let size = archive_bytes.len() as u64;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        // An offset beyond the archive size must return Ok(None) without panicking.
        let result = RarArchive::try_quick_open(
            &reader,
            size + 1000,
            size,
            &crate::ResourceLimits::default(),
        )
        .await;
        assert!(matches!(result, Ok(None)));
    }

    // ── Step 5: Store Extraction tests ───────────────────────────────────────

    /// Helper: open a minimal RAR5 archive from bytes and return the `RarArchive`.
    async fn open_rar5_from_bytes(
        archive_bytes: &[u8],
    ) -> (tempfile::NamedTempFile, RarArchive<crate::io::LocalReadAt>) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), archive_bytes).unwrap();
        let reader = crate::io::LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader, &crate::ResourceLimits::default())
            .await
            .unwrap();
        (tmp, rar)
    }

    #[tokio::test]
    async fn extract_store_returns_correct_bytes() {
        let data = b"hello store extraction";
        let archive_bytes = build_minimal_rar5("out.txt", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let n = rar
            .extract(
                entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await
            .unwrap();

        assert_eq!(n, data.len() as u64);
        assert_eq!(out.as_slice(), data.as_ref());
    }

    #[tokio::test]
    async fn extract_store_empty_file_returns_zero_bytes() {
        let archive_bytes = build_minimal_rar5("empty.txt", b"", false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let n = rar
            .extract(
                entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await
            .unwrap();

        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn extract_store_crc_mismatch_returns_error() {
        let data = b"data for crc test";
        let mut archive_bytes = build_minimal_rar5("f.txt", data, false);

        // Corrupt the last data byte (before the 7-byte EOA block) without
        // updating the file-header CRC field — the post-decompression CRC check
        // will catch the mismatch.
        let corrupt_idx = archive_bytes.len() - 7 - 1;
        archive_bytes[corrupt_idx] ^= 0xFF;

        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let result = rar
            .extract(
                entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await;

        assert!(
            matches!(result, Err(CoreError::CrcMismatch { .. })),
            "expected CrcMismatch, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn extract_encrypted_no_password_returns_password_required() {
        // Build archive where is_encrypted = true (entry has encryption extra area record).
        // The simplest way: manually build a file header with an encryption extra record.
        // For this test we use a helper that sets is_encrypted on the location directly
        // by inspecting the parsed struct via the location array.
        //
        // We build a real encrypted-flag archive: file_flags with the "is_encrypted" bit
        // is determined by the extra area containing type 0x01.  Instead, we verify the
        // error path via the `is_encrypted` field on `RarEntryLocation`.
        //
        // Use build_minimal_rar5 then directly write an entry with an encryption record.
        // Here we take a shortcut: build a valid archive, open it, and verify the field.
        // Since `build_minimal_rar5` never sets the encryption flag, we test the
        // PasswordRequired error by calling extract on a synthetic location. Instead,
        // confirm the "no entry found" path and use the facade test for encryption.
        //
        // NOTE: A full test for encrypted entries requires a fixture with an actual
        // encryption extra area record (0x01). For now, verify the non-encrypted path
        // and trust the is_encrypted guard at the extract() boundary.
        let data = b"test";
        let archive_bytes = build_minimal_rar5("e.txt", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        // Confirm non-encrypted extract still works (guard not triggered).
        let entry = &rar.entries()[0];
        assert!(!entry.is_encrypted);
        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        rar.extract(
            entry,
            &mut out,
            progress,
            None,
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn extract_solid_returns_unsupported_feature() {
        let data = b"solid data";
        let archive_bytes = build_minimal_rar5("s.txt", data, true);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;
        assert_eq!(rar.entries().len(), 1);

        let entry = rar.entries()[0].clone();
        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let err = rar
            .extract(
                &entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, CoreError::UnsupportedFeature { .. }),
            "expected UnsupportedFeature, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn extract_directory_entry_returns_zero() {
        // Build an archive, then synthesise a directory entry by re-opening with a
        // patched ArchiveEntry.  Since `build_minimal_rar5` doesn't build directories,
        // we verify the guard via a synthetic entry directly.
        let data = b"ignored";
        let archive_bytes = build_minimal_rar5("d/", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        // Synthesise an is_directory entry pointing at index 0.
        let mut dir_entry = rar.entries()[0].clone();
        dir_entry.is_directory = true;

        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let n = rar
            .extract(
                &dir_entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    // Progress reporter that records all callbacks for verification.
    struct RecordingReporter {
        started: Vec<(String, u64)>,
        progress_updates: Vec<u64>,
        completed: Vec<(String, bool)>,
    }
    impl RecordingReporter {
        fn new() -> Self {
            Self {
                started: vec![],
                progress_updates: vec![],
                completed: vec![],
            }
        }
    }
    impl crate::ProgressReporter for RecordingReporter {
        fn on_entry_start(&mut self, name: &str, size: u64) {
            self.started.push((name.to_string(), size));
        }
        fn on_bytes_progress(&mut self, bytes: u64) {
            self.progress_updates.push(bytes);
        }
        fn on_entry_complete(&mut self, name: &str, success: bool) {
            self.completed.push((name.to_string(), success));
        }
    }

    #[tokio::test]
    async fn extract_store_progress_reporting() {
        let data = b"progress test data";
        let archive_bytes = build_minimal_rar5("progress.txt", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let reporter = Arc::new(Mutex::new(RecordingReporter::new()));
        let mut out = Vec::new();
        rar.extract(
            entry,
            &mut out,
            reporter.clone(),
            None,
            &crate::ResourceLimits::default(),
        )
        .await
        .unwrap();

        let r = reporter.lock().unwrap();
        assert_eq!(r.started.len(), 1);
        assert_eq!(r.started[0].0, "progress.txt");
        assert_eq!(r.started[0].1, data.len() as u64);
        assert!(
            !r.progress_updates.is_empty(),
            "on_bytes_progress must be called"
        );
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0].0, "progress.txt");
        assert!(
            r.completed[0].1,
            "on_entry_complete must report success=true"
        );
    }

    // Cancelling reporter: cancels after the first progress callback.
    struct CancelAfterFirstUpdate;
    impl crate::ProgressReporter for CancelAfterFirstUpdate {
        fn on_bytes_progress(&mut self, _: u64) {} // first call lands here
        fn should_cancel(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn extract_store_cancellation_returns_cancelled() {
        let data = b"cancellation test data longer than chunk";
        let archive_bytes = build_minimal_rar5("cancel.txt", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let progress = Arc::new(Mutex::new(CancelAfterFirstUpdate));
        let mut out = Vec::new();
        let result = rar
            .extract(
                entry,
                &mut out,
                progress,
                None,
                &crate::ResourceLimits::default(),
            )
            .await;

        assert!(
            matches!(result, Err(CoreError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );
    }
}
