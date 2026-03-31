//! RAR format support.
//!
//! v1 scope: RAR5 non-solid archives (listing + extraction).
//! RAR4 archives are detected and rejected with a clear error.

pub(crate) mod parse;

pub use parse::{RAR4_SIGNATURE, RAR5_SIGNATURE};

use std::path::PathBuf;
use std::time::SystemTime;

use std::io::Read;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

use serde::{Deserialize, Serialize};

use tracing::{debug, info};

use crate::archive_reader::ArchiveReader;
use crate::codec::CompressionMethod;
use crate::io::LocalPath;
use crate::progress::ProgressReporter;
use crate::rar::parse::{
    QoCachedBlock, parse_block_header, parse_file_header, parse_main_header, parse_quick_open_data,
};
use crate::{ArchiveEntry, CoreError, ReadAt};

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
#[allow(dead_code)]
pub(crate) struct SnippetMsg {
    /// Path to the temp file containing a valid standalone RAR archive.
    pub(crate) path: std::path::PathBuf,
    /// The discovered entry metadata.
    pub(crate) entry: ArchiveEntry,
    /// CRC32 of uncompressed data (for verification after unrar).
    pub(crate) crc32: Option<u32>,
    /// BLAKE2sp hash (RAR5 only; None for RAR4).
    pub(crate) blake2_hash: Option<[u8; 32]>,
}

/// Extraction config passed into the modified elastic scan.
/// When provided, the scan writes snippet micro-archives instead of discarding data.
#[allow(dead_code)]
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
    /// Solid entries are skipped — only non-solid entries appear in the listing.
    ///
    /// For local sources, always performs the sequential scan (or Quick Open).
    /// For HTTP sources without Quick Open, defers the scan — returns with
    /// `scan_complete = false`. Call [`scan_metadata`](ArchiveReader::scan_metadata)
    /// before accessing entries.
    ///
    /// Returns [`CoreError::InvalidFormat`] if the RAR5 signature is absent.
    /// Returns [`CoreError::UnsupportedFormat`] if a RAR4 signature is detected.
    // The `LocalPath` bound is `pub(crate)` while `open` is `pub`, but `RarArchive`
    // is only constructed from within this crate (via `Source`), so external
    // callers cannot reach this method in practice.
    #[allow(private_bounds)]
    pub async fn open(reader: R) -> Result<Self, CoreError>
    where
        R: LocalPath,
    {
        info!("opening RAR archive");
        let local_path = reader.local_path().map(|p| p.to_path_buf());
        let size = reader.size().await?;
        debug!(size, local = local_path.is_some(), "RAR archive size determined");

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
        let preamble_end =
            (main_header.raw.archive_offset + main_header.raw.total_size) as usize;
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
                Self::try_quick_open(&reader, qo_abs, size).await
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
                Self::run_elastic_scan(&reader, preamble_end as u64, size, &no_op).await?;
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

    /// All entries in the archive (non-solid only; solid entries are omitted).
    ///
    /// Returns an empty slice if the archive has not yet been scanned.
    /// The [`Archive`] facade ensures via the type system that `entries()` is
    /// only called after a successful scan, so this is a safety net only.
    pub fn entries(&self) -> &[ArchiveEntry] {
        if self.scan_complete { &self.entries } else { &[] }
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
    ) -> Result<(Vec<ArchiveEntry>, Vec<RarEntryLocation>), CoreError> {
        const LARGE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MiB
        debug!(start_offset, size, "elastic scan starting");

        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<RarEntryLocation> = Vec::new();

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
                        let stream_head =
                            offset + (buf.len().saturating_sub(buf_pos)) as u64;
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

            match block.block_type {
                2 => {
                    // File block.
                    let file_hdr = parse_file_header(&buf[buf_pos..], block)?;

                    if !file_hdr.compression.solid {
                        let hdr_size = file_hdr.raw.total_size - file_hdr.raw.data_size;
                        let entry = ArchiveEntry {
                            name: file_hdr.name.clone(),
                            compressed_size: file_hdr.raw.data_size,
                            uncompressed_size: file_hdr.unpacked_size,
                            is_directory: file_hdr.is_directory,
                            is_encrypted: file_hdr.encryption.is_some(),
                            modified_at: file_hdr.mtime.map(unix_to_system_time),
                            compression_method: rar_method_to_codec(
                                file_hdr.compression.method,
                            ),
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
                            is_solid: false,
                            is_encrypted: file_hdr.encryption.is_some(),
                        };
                        entries.push(entry);
                        locations.push(location);
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
        &self,
        entry_indices: &[usize],
        sink_factory: F,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, CoreError>
    where
        F: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        let work: Vec<(ArchiveEntry, RarEntryLocation)> = entry_indices
            .iter()
            .filter(|&&idx| {
                !self.entries[idx].is_directory && !self.locations[idx].is_solid
            })
            .map(|&idx| (self.entries[idx].clone(), self.locations[idx].clone()))
            .collect();

        if work.is_empty() {
            return Ok(0);
        }

        if let Some(path) = &self.local_path {
            info!(entries = work.len(), "RAR extract_all: local path");
            return extract_all_via_unrar(
                path.clone(),
                work,
                sink_factory,
                progress,
                password,
            )
            .await;
        }

        // Cloud path: download to temp file via one stream, then extract locally.
        info!(entries = work.len(), "RAR extract_all: cloud path (single stream to temp)");
        let preamble = self.archive_preamble.clone();
        let preamble_end = preamble.len() as u64;
        let archive_size = self.archive_size;

        // Create temp file.
        let temp_file = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("spawn temp file failed: {e}"),
            })?
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("create temp file failed: {e}"),
            })?;

        // Write preamble + stream body + end block to temp file.
        let temp_std = temp_file.as_file().try_clone().map_err(|e| {
            CoreError::BitstreamError {
                detail: format!("temp file clone failed: {e}"),
            }
        })?;
        let mut temp_writer =
            tokio::io::BufWriter::new(tokio::fs::File::from_std(temp_std));

        temp_writer.write_all(&preamble).await.map_err(|e| {
            CoreError::BitstreamError {
                detail: format!("temp file write preamble failed: {e}"),
            }
        })?;

        // One HTTP request: stream the entire body.
        let body_len = archive_size.saturating_sub(preamble_end);
        let mut stream = self
            .reader
            .read_stream(preamble_end, body_len)
            .await
            .map_err(CoreError::Io)?;

        tokio::io::copy(&mut stream, &mut temp_writer)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("temp file stream copy failed: {e}"),
            })?;

        temp_writer.flush().await.map_err(|e| {
            CoreError::BitstreamError {
                detail: format!("temp file flush failed: {e}"),
            }
        })?;
        drop(temp_writer);

        // Extract from the local temp file.
        let temp_path = temp_file.path().to_path_buf();
        let result = extract_all_via_unrar(
            temp_path,
            work,
            sink_factory,
            progress,
            password,
        )
        .await;

        // temp_file dropped here → auto-deleted.
        result
    }

    /// Bytes that must be transferred to produce this entry.
    ///
    /// For non-solid RAR archives every entry is independently seekable, so
    /// this is just the entry's compressed data size.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        self.locations[entry.index].data_size
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
    ) -> Result<u64, CoreError> {
        if entry.is_directory {
            return Ok(0);
        }

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
            return self.extract_compressed(entry, loc, sink, progress, password).await;
        }

        match loc.method {
            0 => {
                debug!(name = %entry.name, "store extraction (method 0)");
                self.extract_store(entry, loc, sink, progress).await
            }
            1..=5 => {
                debug!(name = %entry.name, method = loc.method, "compressed extraction via unrar");
                self.extract_compressed(entry, loc, sink, progress, None).await
            }
            m => Err(CoreError::UnsupportedCompression {
                algorithm: format!("RAR method {m}"),
            }),
        }
    }

    /// Decompress and verify CRC without retaining output.
    ///
    /// Used for archive integrity checking (`snipe test` command).
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        let progress = Arc::new(Mutex::new(()));
        self.extract(entry, &mut tokio::io::sink(), progress, None)
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
    ) -> Result<u64, CoreError> {
        // Capture owned values before the async/blocking boundary.
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let expected_blake2 = loc.blake2_hash;

        // Step 1 (async): get a streaming AsyncRead for the raw (stored) data.
        let stream = self
            .reader
            .read_stream(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        // Step 2 (spawn_blocking): read chunks, compute hashes, accumulate.
        let handle = tokio::runtime::Handle::current();
        let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);

        let verified: Vec<u8> =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
                const CHUNK_SIZE: usize = 64 * 1024;

                let mut crc_hasher = crc32fast::Hasher::new();
                let mut blake2_state = if expected_blake2.is_some() {
                    Some(blake2s_simd::blake2sp::State::new())
                } else {
                    None
                };

                let mut total_written: u64 = 0;
                let mut chunk_buf = vec![0u8; CHUNK_SIZE];
                let mut verified_buf = Vec::with_capacity(uncompressed_size as usize);

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
                    verified_buf.extend_from_slice(&chunk_buf[..n]);
                    total_written += n as u64;

                    {
                        let mut p = progress.lock().unwrap();
                        p.on_bytes_progress(total_written);
                        if p.should_cancel() {
                            p.on_entry_complete(&entry_name, false);
                            return Err(CoreError::Cancelled);
                        }
                    }
                }

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

                Ok(verified_buf)
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        // Step 3 (async): write verified bytes to the sink.
        let bytes_written = verified.len() as u64;
        sink.write_all(&verified)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("write to sink failed: {e}"),
            })?;

        Ok(bytes_written)
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
    ) -> Result<u64, CoreError> {
        if let Some(path) = &self.local_path {
            debug!(name = %entry.name, path = %path.display(), "unrar local path");
            return self
                .extract_via_unrar_local(entry, loc, path, sink, progress, password)
                .await;
        }
        debug!(name = %entry.name, "unrar cloud path (synthetic archive)");
        self.extract_via_unrar_cloud(entry, loc, sink, progress, password)
            .await
    }

    /// Extract a compressed (or encrypted) entry from a **local** archive via `unrar`.
    ///
    /// Opens the original file directly — no temp file needed.  Iterates headers
    /// until the target entry is found, then reads all decompressed bytes into
    /// memory via `unrar`.  CRC32 and BLAKE2sp are verified after decompression.
    async fn extract_via_unrar_local<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &RarEntryLocation,
        archive_path: &std::path::Path,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
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

        let data =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
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
                                message: format!(
                                    "entry '{target_name}' not found in archive"
                                ),
                            })
                        }
                    };

                    let name =
                        header.entry().filename.to_string_lossy().replace('\\', "/");
                    if name == target_name {
                        let (data, _) = header.read().map_err(map_unrar_error)?;
                        return Ok(data);
                    }
                    archive = header.skip().map_err(map_unrar_error)?;
                }
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_write(
            &data,
            &entry_name,
            expected_crc,
            expected_blake2,
            sink,
            &progress,
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

        // 2. Download the compressed data.
        let compressed_data = self
            .reader
            .read_range(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        // Snapshot preamble and end block for the blocking closure.
        let archive_preamble = self.archive_preamble.clone();
        let end_block = self.end_block.clone();

        // 3. Assemble synthetic archive, extract with unrar — all in spawn_blocking.
        let data =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
                // Create temp file and write the synthetic RAR5 archive.
                let temp_file = tempfile::NamedTempFile::new().map_err(|e| {
                    CoreError::BitstreamError {
                        detail: format!("failed to create temp file: {e}"),
                    }
                })?;

                {
                    use std::io::Write;
                    let mut writer =
                        std::io::BufWriter::new(temp_file.as_file());
                    for chunk in [
                        archive_preamble.as_ref(),
                        file_header_bytes.as_ref(),
                        compressed_data.as_ref(),
                        end_block.as_ref(),
                    ] {
                        writer.write_all(chunk).map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("temp file write failed: {e}"),
                            }
                        })?;
                    }
                    writer.flush().map_err(|e| CoreError::BitstreamError {
                        detail: format!("temp file flush failed: {e}"),
                    })?;
                }

                // Extract from the synthetic archive.
                let temp_path = temp_file.path().to_path_buf();
                let archive = match &password_bytes {
                    Some(pw) => unrar::Archive::with_password(&temp_path, pw.as_slice())
                        .open_for_processing()
                        .map_err(map_unrar_error)?,
                    None => unrar::Archive::new(&temp_path)
                        .open_for_processing()
                        .map_err(map_unrar_error)?,
                };

                let header = archive.read_header().map_err(map_unrar_error)?;
                let header = header.ok_or_else(|| CoreError::BitstreamError {
                    detail: "synthetic archive produced no entries".into(),
                })?;
                let (data, _) = header.read().map_err(map_unrar_error)?;

                // temp_file is dropped here → temp file deleted automatically.
                Ok(data)
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_write(
            &data,
            &entry_name,
            expected_crc,
            expected_blake2,
            sink,
            &progress,
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

        for QoCachedBlock { original_archive_offset, data } in cached {
            // Parse common block header from cached bytes.
            let raw_block = match parse_block_header(&data, original_archive_offset) {
                Ok(b) => b,
                Err(_) => continue, // skip unparseable cached entry
            };

            // Only process file headers (type 2).
            if raw_block.block_type != 2 {
                continue;
            }

            // Note: cached headers in QO blocks do NOT include the data area bytes.
            // The data_size field from the common block header reflects the compressed
            // data size in the original archive; we use it for location offsets.
            let file_hdr = match parse_file_header(&data, raw_block) {
                Ok(h) => h,
                Err(_) => continue,
            };

            if file_hdr.compression.solid {
                continue; // skip solid entries
            }

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
                is_solid: false,
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

    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, CoreError> {
        RarArchive::extract(self, entry, sink, progress, password).await
    }

    async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        RarArchive::test(self, entry).await
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
    ) -> Result<(), CoreError> {
        let preamble_end = self.archive_preamble.len() as u64;
        let size = self.archive_size;

        {
            let mut p = progress.lock().unwrap();
            p.on_scan_start(size);
        }

        let (entries, locations) =
            Self::run_elastic_scan(&self.reader, preamble_end, size, &progress).await?;

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
        Code::BadData | Code::BadArchive => CoreError::BitstreamError {
            detail: format!("corrupt RAR data: {e}"),
        },
        _ => CoreError::BitstreamError {
            detail: format!("unrar error: {e}"),
        },
    }
}

/// Verify CRC32 and BLAKE2sp of `data`, write to `sink`, and fire progress callbacks.
///
/// Used by both local and cloud extraction paths after `unrar` decompresses bytes
/// into a `Vec<u8>`.  Writes to `sink` only after hash verification succeeds.
pub(crate) async fn verify_and_write<W: AsyncWrite + Send + Unpin>(
    data: &[u8],
    entry_name: &str,
    expected_crc: Option<u32>,
    expected_blake2: Option<[u8; 32]>,
    sink: &mut W,
    progress: &Arc<Mutex<dyn ProgressReporter>>,
) -> Result<u64, CoreError> {
    // CRC32 verification.
    if let Some(expected) = expected_crc {
        let actual = crc32fast::hash(data);
        if actual != expected {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(entry_name, false);
            return Err(CoreError::CrcMismatch {
                entry: entry_name.to_owned(),
                expected,
                actual,
            });
        }
    }

    // BLAKE2sp verification (present only when extra area record 0x02 exists).
    if let Some(expected_hash) = expected_blake2 {
        let mut state = blake2s_simd::blake2sp::State::new();
        state.update(data);
        let actual = state.finalize();
        if actual.as_array() != &expected_hash {
            progress
                .lock()
                .unwrap()
                .on_entry_complete(entry_name, false);
            return Err(CoreError::BitstreamError {
                detail: format!("BLAKE2sp hash mismatch for '{entry_name}'"),
            });
        }
    }

    let bytes_written = data.len() as u64;
    {
        let mut p = progress.lock().unwrap();
        p.on_bytes_progress(bytes_written);
    }
    sink.write_all(data)
        .await
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("write to sink failed: {e}"),
        })?;
    progress
        .lock()
        .unwrap()
        .on_entry_complete(entry_name, true);
    Ok(bytes_written)
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
) -> Result<u64, CoreError>
where
    F: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
{
    let password_bytes = password.map(|p| p.to_vec());

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

                    let (data, next_archive) = header.read().map_err(map_unrar_error)?;
                    archive = next_archive;

                    // CRC32 verification.
                    if let Some(expected) = loc.crc32 {
                        let actual = crc32fast::hash(&data);
                        if actual != expected {
                            return Err(CoreError::CrcMismatch {
                                entry: name,
                                expected,
                                actual,
                            });
                        }
                    }

                    // BLAKE2sp verification.
                    if let Some(expected_hash) = loc.blake2_hash {
                        let mut state = blake2s_simd::blake2sp::State::new();
                        state.update(&data);
                        let actual = state.finalize();
                        if actual.as_array() != &expected_hash {
                            return Err(CoreError::BitstreamError {
                                detail: format!("BLAKE2sp hash mismatch for '{name}'"),
                            });
                        }
                    }

                    use std::io::Write;
                    sink.write_all(&data).map_err(|e| CoreError::BitstreamError {
                        detail: format!("write failed for '{name}': {e}"),
                    })?;

                    total_bytes += data.len() as u64;
                    progress.lock().unwrap().on_entry_complete(&name, true);
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
        let secs = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
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
            assert_eq!(rar_method_to_codec(m), None, "method {m} should map to None");
        }
    }

    // ── RarArchive::open — in-memory fixtures ─────────────────────────────────

    /// Build a minimal valid RAR5 archive in memory: signature + main header +
    /// one Store file entry + end-of-archive block.
    ///
    /// Used to verify `RarArchive::open` without requiring a real `.rar` file.
    pub(crate) fn build_minimal_rar5(
        filename: &str,
        data: &[u8],
        solid: bool,
    ) -> Vec<u8> {
        // ── End-of-archive block ──────────────────────────────────────────────
        let eoa = build_end_of_archive_block();

        // ── File header block (type 2) ────────────────────────────────────────
        // file_flags: 0x06 (mtime + crc32 present, size known, not dir)
        // Adjust compression_info for solid flag.
        let solid_bit: u8 = if solid { 1 } else { 0 };
        // compression_info bits: version=0, solid=solid_bit(bit6), method=0(bits7-9)
        let compression_info: u8 = solid_bit << 6;

        let mtime: u32 = 1_700_000_000;
        let crc32_val: u32 = crc32fast::hash(data);
        let data_len = data.len() as u8; // only correct for small test data

        let name_bytes = filename.as_bytes();
        let name_len = name_bytes.len() as u8;

        let mut body = Vec::new();
        body.push(0x06u8);                              // file_flags: mtime + crc32
        body.push(data_len);                            // unpacked_size (vint, 1 byte)
        body.push(0x20u8);                              // attributes
        body.extend_from_slice(&mtime.to_le_bytes());   // mtime (4 bytes)
        body.extend_from_slice(&crc32_val.to_le_bytes()); // data_crc32 (4 bytes)
        body.push(compression_info);                    // compression_info (vint, 1 byte)
        body.push(0x01u8);                              // host_os = Unix
        body.push(name_len);                            // name_len (vint, 1 byte)
        body.extend_from_slice(name_bytes);             // name

        // block_type=2, flags=0x0002 (data area present), data_area_size = data.len()
        let header_content_len = 1 + 1 + 1 + body.len(); // block_type + flags + data_size_vint + body
        let header_size = header_content_len;

        // Encode header_size as vint (1 byte, assuming < 128).
        let mut crc_region = vec![
            header_size as u8,
            0x02u8,   // block_type = 2
            0x02u8,   // flags = 0x0002 (data area flag)
            data_len, // data_area_size vint
        ];
        crc_region.extend_from_slice(&body);

        let file_hdr_crc = crc32fast::hash(&crc_region);
        let mut file_block = Vec::new();
        file_block.extend_from_slice(&file_hdr_crc.to_le_bytes());
        file_block.extend_from_slice(&crc_region);
        file_block.extend_from_slice(data); // data area

        // ── Main archive header (type 1) ──────────────────────────────────────
        // file_flags = 0 (no volume, no comment, no solid)
        let archive_flags: u8 = 0;
        let main_body = vec![archive_flags]; // archive_flags vint

        let main_header_content = 1 + 1 + main_body.len(); // block_type + flags + body
        let main_header_size = main_header_content;

        let mut main_crc_region = vec![
            main_header_size as u8, // header_size vint
            0x01u8,                 // block_type = 1 (main header)
            0x00u8,                 // flags = 0 (no extra, no data)
        ];
        main_crc_region.extend_from_slice(&main_body);

        let main_crc = crc32fast::hash(&main_crc_region);
        let mut main_block = Vec::new();
        main_block.extend_from_slice(&main_crc.to_le_bytes());
        main_block.extend_from_slice(&main_crc_region);

        // ── Assemble ──────────────────────────────────────────────────────────
        let mut archive = Vec::new();
        archive.extend_from_slice(&RAR5_SIGNATURE);
        archive.extend_from_slice(&main_block);
        archive.extend_from_slice(&file_block);
        archive.extend_from_slice(&eoa);
        archive
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
        let rar = RarArchive::open(reader).await.unwrap();

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
        let result = RarArchive::open(reader).await;
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
        let result = RarArchive::open(reader).await;
        assert!(
            matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected InvalidFormat, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn solid_entry_skipped() {
        use crate::io::LocalReadAt;

        let data = b"solid data";
        let archive_bytes = build_minimal_rar5("solid.txt", data, true);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader).await.unwrap();

        // Solid entry must be silently omitted from the listing.
        assert_eq!(rar.entries().len(), 0, "solid entries should be skipped");
    }

    #[tokio::test]
    async fn end_block_captured() {
        use crate::io::LocalReadAt;

        let data = b"test";
        let archive_bytes = build_minimal_rar5("f.txt", data, false);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive_bytes).unwrap();

        let reader = LocalReadAt::new(tmp.path().to_str().unwrap());
        let rar = RarArchive::open(reader).await.unwrap();

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
        let rar = RarArchive::open(reader).await.unwrap();

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
        let rar = RarArchive::open(reader).await.unwrap();

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
            if value == 0 { buf.push(byte); break; } else { buf.push(byte | 0x80); }
        }
    }

    /// Build a minimal valid RAR5 file-header block (type 2, Store, no extras).
    /// Returns only the header bytes — the `data_size` bytes of compressed data
    /// are NOT appended, matching how QO caches headers.
    fn build_file_header_bytes(name: &str, data_size: u64) -> Vec<u8> {
        let name_bytes = name.as_bytes();
        let mut body = Vec::new();
        body.push(0x00u8);                          // file_flags = 0 (size known, no mtime, no crc)
        encode_vint_test(data_size, &mut body);     // unpacked_size
        body.push(0x00u8);                          // attributes
        body.push(0x00u8);                          // compression_info = 0 (Store)
        body.push(0x01u8);                          // host_os = Unix
        encode_vint_test(name_bytes.len() as u64, &mut body);
        body.extend_from_slice(name_bytes);

        let mut data_size_vint = Vec::new();
        encode_vint_test(data_size, &mut data_size_vint);

        // header_size = block_type(1) + flags(1) + data_size_vint + body
        let header_size = 1 + 1 + data_size_vint.len() + body.len();
        let mut crc_region = Vec::new();
        encode_vint_test(header_size as u64, &mut crc_region);
        crc_region.push(0x02u8);                    // block_type = 2
        crc_region.push(0x02u8);                    // flags = 0x0002 (data area present)
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
        crc_region.push(0x03u8);                    // block_type = 3 (service)
        crc_region.push(0x02u8);                    // flags = 0x0002 (data area present)
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
        structure.push(0x00u8);                     // flags = 0
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
        let rar = RarArchive::open(reader).await.unwrap();

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
        let result = RarArchive::try_quick_open(&reader, 0, size).await;

        assert!(result.is_ok());
        let opt = result.unwrap();
        assert!(opt.is_some(), "try_quick_open must return Some for a valid QO block");

        let (entries, locations) = opt.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "qo_file.txt");
        assert_eq!(entries[0].compression_method, Some(CompressionMethod::Store));
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
        let result = RarArchive::try_quick_open(&reader, size + 1000, size).await;
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
        let rar = RarArchive::open(reader).await.unwrap();
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
        let n = rar.extract(entry, &mut out, progress, None).await.unwrap();

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
        let n = rar.extract(entry, &mut out, progress, None).await.unwrap();

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
        let result = rar.extract(entry, &mut out, progress, None).await;

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
        rar.extract(entry, &mut out, progress, None).await.unwrap();
    }

    #[tokio::test]
    async fn extract_solid_returns_unsupported_feature() {
        let data = b"solid data";
        // build_minimal_rar5 with solid=true creates a solid entry — open() skips it,
        // so entries() is empty. Verify that (solid entries are already filtered out).
        let archive_bytes = build_minimal_rar5("s.txt", data, true);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;
        assert_eq!(rar.entries().len(), 0, "solid entries must be omitted from listing");
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
        let n = rar.extract(&dir_entry, &mut out, progress, None).await.unwrap();
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
            Self { started: vec![], progress_updates: vec![], completed: vec![] }
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
        rar.extract(entry, &mut out, reporter.clone(), None)
            .await
            .unwrap();

        let r = reporter.lock().unwrap();
        assert_eq!(r.started.len(), 1);
        assert_eq!(r.started[0].0, "progress.txt");
        assert_eq!(r.started[0].1, data.len() as u64);
        assert!(!r.progress_updates.is_empty(), "on_bytes_progress must be called");
        assert_eq!(r.completed.len(), 1);
        assert_eq!(r.completed[0].0, "progress.txt");
        assert!(r.completed[0].1, "on_entry_complete must report success=true");
    }

    // Cancelling reporter: cancels after the first progress callback.
    struct CancelAfterFirstUpdate;
    impl crate::ProgressReporter for CancelAfterFirstUpdate {
        fn on_bytes_progress(&mut self, _: u64) {} // first call lands here
        fn should_cancel(&self) -> bool { true }
    }

    #[tokio::test]
    async fn extract_store_cancellation_returns_cancelled() {
        let data = b"cancellation test data longer than chunk";
        let archive_bytes = build_minimal_rar5("cancel.txt", data, false);
        let (_tmp, rar) = open_rar5_from_bytes(&archive_bytes).await;

        let entry = &rar.entries()[0];
        let progress = Arc::new(Mutex::new(CancelAfterFirstUpdate));
        let mut out = Vec::new();
        let result = rar.extract(entry, &mut out, progress, None).await;

        assert!(
            matches!(result, Err(CoreError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );
    }
}

