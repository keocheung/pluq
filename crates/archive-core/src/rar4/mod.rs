//! RAR4 format support.
//!
//! Custom header parser for listing (enables cloud sniping via range requests).
//! Extraction delegates to `unrar` — local shortcut or synthetic archive for cloud.

#[cfg(test)]
mod batch_tests;
pub(crate) mod parse;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

use crate::archive_reader::{ArchiveReader, BatchResult, extract_selected_default};
use crate::codec::CompressionMethod;
use crate::io::{FetchRegion, LocalPath, build_clusters};
use crate::junk;
use crate::progress::ProgressReporter;
use crate::rar::{
    RAR4_SIGNATURE, SnippetConfig, SnippetMsg, assemble_and_unrar_synthetic,
    build_unrar_progress_hooks, consume_snippets, map_unrar_error, verify_and_stream_from_file,
};
use crate::rar4::parse::{
    BLOCK_END, BLOCK_FILE, build_rar4_end_block, parse_block_header, parse_file_header,
    parse_main_header,
};
use crate::time::dos_datetime_to_system_time;
use crate::{ArchiveEntry, CoreError, ReadAt};

// ── File-header flag constants ────────────────────────────────────────────────

/// File is encoded in solid mode — depends on the dictionary of preceding entries.
const FLAG_SOLID: u16 = 0x0010;
/// File is continued from a previous volume.
const FLAG_CONTINUED_PREV: u16 = 0x0001;
/// File continues into the next volume.
const FLAG_CONTINUED_NEXT: u16 = 0x0002;

// ── Location data (private) ───────────────────────────────────────────────────

/// Per-entry location data, parallel to `Rar4Archive::entries`.
///
/// `pub` for cache serialization in the `archive` crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rar4EntryLocation {
    /// Absolute offset of the file header block in the archive.
    pub header_offset: u64,
    /// Byte length of the file header (fields only, not data area).
    pub header_size: u64,
    /// Absolute offset of the compressed data in the archive.
    pub data_offset: u64,
    /// Compressed data size in bytes.
    pub data_size: u64,
    /// CRC32 of the uncompressed data.
    pub crc32: u32,
    /// RAR4 compression method byte: 0x30 = Store, 0x31-0x35 = RAR proprietary.
    pub method: u8,
    /// True if an encryption flag was found for this entry.
    pub is_encrypted: bool,
}

// ── Rar4Archive ───────────────────────────────────────────────────────────────

/// A parsed RAR4 archive backed by any [`ReadAt`] source.
///
/// Created via [`Rar4Archive::open`], which verifies the RAR4 signature and
/// performs a sequential scan of all file headers.
///
/// # Internal layout
///
/// Holds two parallel `Vec`s at the same index:
/// - `entries` — public, format-agnostic [`ArchiveEntry`] metadata
/// - `locations` — private [`Rar4EntryLocation`] with byte offsets
///
/// # Synthetic archive construction (cloud extraction)
///
/// `archive_preamble` contains the RAR4 signature + main header bytes.
/// `end_block` is a minimal RAR4 end-of-archive block.  Together with a
/// file header + compressed data, they form a minimal valid RAR4 archive
/// that `unrar` can decompress.
pub struct Rar4Archive<R: ReadAt> {
    reader: R,
    entries: Vec<ArchiveEntry>,
    locations: Vec<Rar4EntryLocation>,
    /// RAR4 signature + main header bytes (for synthetic archive construction).
    archive_preamble: Bytes,
    /// Minimal RAR4 end-of-archive block.
    end_block: Bytes,
    /// Local filesystem path, if the source is a local file.
    local_path: Option<PathBuf>,
    /// Total archive size in bytes.
    archive_size: u64,
    /// Whether the entry list has been populated.
    scan_complete: bool,
}

impl<R: ReadAt> std::fmt::Debug for Rar4Archive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rar4Archive")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl<R: ReadAt> Rar4Archive<R> {
    /// Open a RAR4 archive by verifying the signature and scanning file headers.
    ///
    /// Unlike RAR5, RAR4 has no Quick Open mechanism — the sequential scan always
    /// runs immediately during `open` for both local and HTTP sources.
    ///
    /// Returns [`CoreError::InvalidFormat`] if the RAR4 signature is absent.
    /// Returns [`CoreError::UnsupportedFeature`] for solid archives.
    pub async fn open(reader: R, limits: &crate::ResourceLimits) -> Result<Self, CoreError>
    where
        R: LocalPath,
    {
        let local_path = reader.local_path().map(|p| p.to_path_buf());
        let size = reader.size().await?;

        // 1. Verify RAR4 signature (7 bytes at offset 0).
        let sig = reader.read_range(0, 7).await?;
        if sig.len() < 7 || sig[..7] != RAR4_SIGNATURE {
            return Err(CoreError::InvalidFormat {
                message: "not a RAR4 archive (bad signature)".into(),
            });
        }

        // 2. Read initial 64 KiB chunk.
        let initial_chunk_size = (64 * 1024_u64).min(size);
        let chunk = reader.read_range(0, initial_chunk_size).await?;

        // 3. Parse the main archive header (immediately after the 7-byte marker block).
        let main_header = parse_main_header(&chunk[7..], 7)?;

        if main_header.is_solid {
            return Err(CoreError::UnsupportedFeature {
                detail: "solid RAR4 archives are not supported in v1".into(),
            });
        }

        let preamble_end = (main_header.raw.archive_offset + main_header.raw.total_size) as usize;
        if preamble_end > chunk.len() {
            return Err(CoreError::CorruptHeader {
                offset: 7,
                reason: "RAR4 main header extends beyond initial read".into(),
            });
        }

        let archive_preamble = chunk.slice(0..preamble_end);
        let end_block = build_rar4_end_block();

        // 4. Local source: scan immediately. HTTP source: defer scan.
        if local_path.is_some() {
            let no_op: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
            let (entries, locations) =
                Self::run_elastic_scan(&reader, preamble_end as u64, size, &no_op, None, limits)
                    .await?;
            Ok(Self {
                reader,
                entries,
                locations,
                archive_preamble,
                end_block,
                local_path,
                archive_size: size,
                scan_complete: true,
            })
        } else {
            // HTTP source: defer scan, return empty entries.
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
    }

    /// All entries in the archive (solid and split-volume entries are omitted).
    ///
    /// Returns an empty slice if the archive has not yet been scanned.
    pub fn entries(&self) -> &[ArchiveEntry] {
        if self.scan_complete {
            &self.entries
        } else {
            &[]
        }
    }

    /// Entries with OS-generated junk filtered out.
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries.iter().filter(|e| !junk::is_junk(e)).collect()
    }

    /// Per-entry location data, parallel to `entries()`. Used by the cache layer.
    pub fn locations(&self) -> &[Rar4EntryLocation] {
        &self.locations
    }

    /// RAR4 signature + main header bytes (for synthetic archive construction).
    pub fn archive_preamble_bytes(&self) -> &[u8] {
        &self.archive_preamble
    }

    /// Minimal RAR4 end-of-archive block bytes.
    pub fn end_block_bytes(&self) -> &[u8] {
        &self.end_block
    }

    /// Total archive size in bytes.
    pub fn archive_size(&self) -> u64 {
        self.archive_size
    }

    /// Reconstruct a `Rar4Archive` from cached data, skipping all I/O.
    pub fn from_cached(
        reader: R,
        entries: Vec<ArchiveEntry>,
        locations: Vec<Rar4EntryLocation>,
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

    /// Bytes that must be transferred to produce this entry.
    ///
    /// For surgical cloud extraction, we must download both the file header
    /// and the compressed data area.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        let loc = &self.locations[entry.index];
        loc.header_size + loc.data_size
    }

    /// Elastic metadata scan — streams archive bytes from `start_offset`.
    ///
    /// Opens a single `read_stream` and slides through blocks sequentially.
    /// Small data areas (≤ 10 MiB) are consumed in-stream.
    /// Large data areas are skipped via a surgical seek (new stream open).
    async fn run_elastic_scan(
        reader: &R,
        start_offset: u64,
        size: u64,
        progress: &Arc<Mutex<dyn ProgressReporter>>,
        snippet_config: Option<SnippetConfig>,
        limits: &crate::ResourceLimits,
    ) -> Result<(Vec<ArchiveEntry>, Vec<Rar4EntryLocation>), CoreError> {
        const LARGE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MiB

        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<Rar4EntryLocation> = Vec::new();
        let mut header_bytes_read: u64 = 0;

        if start_offset >= size {
            return Ok((entries, locations));
        }

        let mut stream = reader
            .read_stream(start_offset, size - start_offset)
            .await
            .map_err(CoreError::Io)?;

        // Sliding buffer: buf[buf_pos..] = archive bytes starting at `offset`.
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

            // Compact consumed prefix.
            if buf_pos > 4096 {
                buf.drain(..buf_pos);
                buf_pos = 0;
            }

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

            // RAR4 common block header: 7 bytes minimum, 11 with FLAG_LONG_BLOCK.
            fill!(11);
            if buf.len().saturating_sub(buf_pos) < 7 {
                break;
            }

            let block = parse_block_header(&buf[buf_pos..], offset)?;
            let head_size = block.head_size as usize;

            let this_header_len = head_size as u64;
            header_bytes_read += this_header_len;
            if header_bytes_read > limits.max_header_bytes {
                return Err(CoreError::ResourceLimitExceeded {
                    message: format!(
                        "RAR4 cumulative header size exceeds max_header_bytes {}",
                        limits.max_header_bytes
                    ),
                });
            }

            // Ensure full file header is in buf (head_size bytes from block start).
            fill!(head_size);

            match block.head_type {
                BLOCK_FILE => {
                    if entries.len() >= limits.max_entries {
                        return Err(CoreError::ResourceLimitExceeded {
                            message: format!(
                                "RAR4 entry count exceeds max_entries {}",
                                limits.max_entries
                            ),
                        });
                    }

                    let file_hdr = parse_file_header(&buf[buf_pos..], block)?;

                    let is_solid = file_hdr.raw.head_flags & FLAG_SOLID != 0;
                    let is_split =
                        file_hdr.raw.head_flags & (FLAG_CONTINUED_PREV | FLAG_CONTINUED_NEXT) != 0;

                    let header_size_u64 = file_hdr.raw.head_size as u64;
                    let pack_size = file_hdr.pack_size;
                    let total_advance = header_size_u64 + pack_size;

                    if !is_solid && !is_split {
                        let modified_at = dos_datetime_to_system_time(
                            (file_hdr.ftime >> 16) as u16,
                            (file_hdr.ftime & 0xFFFF) as u16,
                        );

                        let is_symlink = parse::is_rar4_symlink(file_hdr.host_os, file_hdr.attr);
                        let mut is_directory = file_hdr.is_directory;
                        if is_symlink && is_directory {
                            is_directory = false;
                        }

                        let entry = ArchiveEntry {
                            name: file_hdr.name.clone(),
                            compressed_size: pack_size,
                            uncompressed_size: file_hdr.unp_size,
                            is_directory,
                            is_encrypted: file_hdr.is_encrypted,
                            is_symlink,
                            modified_at,
                            compression_method: rar4_method_to_codec(file_hdr.method),
                            index: entries.len(),
                        };
                        let location = Rar4EntryLocation {
                            header_offset: offset,
                            header_size: header_size_u64,
                            data_offset: offset + header_size_u64,
                            data_size: pack_size,
                            crc32: file_hdr.file_crc,
                            method: file_hdr.method,
                            is_encrypted: file_hdr.is_encrypted,
                        };
                        entries.push(entry.clone());
                        locations.push(location);

                        // Snippet branch: produce a mini-archive for this entry.
                        if let Some(ref config) = snippet_config
                            && (config.filter)(&entry)
                        {
                            let head_bytes_owned = buf[buf_pos..buf_pos + head_size].to_vec();

                            let temp_file =
                                tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
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
                                .write_all(&head_bytes_owned)
                                .await
                                .map_err(snippet_write_err)?;

                            buf_pos += head_size;
                            let data_in_buf = (buf.len() - buf_pos).min(pack_size as usize);
                            if data_in_buf > 0 {
                                snippet_writer
                                    .write_all(&buf[buf_pos..buf_pos + data_in_buf])
                                    .await
                                    .map_err(snippet_write_err)?;
                            }
                            buf_pos += data_in_buf;

                            let data_remaining = pack_size - data_in_buf as u64;
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
                                    data_size: pack_size,
                                    crc32: Some(file_hdr.file_crc),
                                    blake2_hash: None,
                                })
                                .await
                                .is_err()
                            {
                                let _ = std::fs::remove_file(&snippet_path);
                                return Err(CoreError::BitstreamError {
                                    detail: "extraction consumer dropped unexpectedly".into(),
                                });
                            }

                            offset += total_advance;
                            progress
                                .lock()
                                .unwrap()
                                .on_scan_progress(offset, entries.len());
                            continue;
                        }
                    }

                    buf_pos += head_size; // advance past header

                    if pack_size <= LARGE_THRESHOLD {
                        let data_in_buf = (buf.len() - buf_pos).min(pack_size as usize);
                        buf_pos += data_in_buf;
                        let data_remaining = pack_size - data_in_buf as u64;
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
                        // Surgical jump.
                        offset += total_advance;
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

                    offset += total_advance;
                    progress
                        .lock()
                        .unwrap()
                        .on_scan_progress(offset, entries.len());
                    continue; // skip bottom offset += block.total_size
                }
                BLOCK_END => break,
                _ => {
                    // Unknown block: consume data area.
                    let data_size = block.add_size as u64;
                    buf_pos += head_size;
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

            offset += block.total_size;
            progress
                .lock()
                .unwrap()
                .on_scan_progress(offset, entries.len());
        }

        Ok((entries, locations))
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
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

        if loc.is_encrypted && password.is_none() {
            return Err(CoreError::PasswordRequired);
        }

        // Encrypted entries must go through unrar (it handles AES decryption).
        if loc.is_encrypted {
            return self
                .extract_compressed(entry, loc, sink, progress, password, limits)
                .await;
        }

        match loc.method {
            0x30 => self.extract_store(entry, loc, sink, progress, limits).await,
            0x31..=0x35 => {
                self.extract_compressed(entry, loc, sink, progress, None, limits)
                    .await
            }
            m => Err(CoreError::UnsupportedCompression {
                algorithm: format!("RAR4 method 0x{m:02X}"),
            }),
        }
    }

    /// Decompress and verify CRC without retaining output.
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

    /// Extract a Store (method 0x30) entry with CRC32 verification.
    async fn extract_store<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let compressed_size = loc.data_size;

        // Step 1 (async): streaming AsyncRead for the raw stored data.
        let stream = self
            .reader
            .read_stream(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        // Step 2 (spawn_blocking): read chunks, compute CRC32, accumulate.
        let handle = tokio::runtime::Handle::current();
        let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);
        let limits_clone = limits.clone();

        use std::io::Write;
        let temp_file =
            tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile, CoreError> {
                const CHUNK_SIZE: usize = 64 * 1024;

                let mut crc_hasher = crc32fast::Hasher::new();
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

                let actual = crc_hasher.finalize();
                if actual != expected_crc {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_complete(&entry_name, false);
                    return Err(CoreError::CrcMismatch {
                        entry: entry_name,
                        expected: expected_crc,
                        actual,
                    });
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

    /// Dispatch compressed extraction to `unrar` — local or cloud path.
    async fn extract_compressed<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        if let Some(path) = &self.local_path {
            tracing::debug!(name = %entry.name, path = %path.display(), "unrar local path");
            return Self::extract_via_unrar_local(
                entry, loc, path, sink, progress, password, limits,
            )
            .await;
        }
        self.extract_via_unrar_cloud(entry, loc, sink, progress, password, limits)
            .await
    }

    /// Extract a compressed (or encrypted) entry from a **local** archive via `unrar`.
    async fn extract_via_unrar_local<W: AsyncWrite + Send + Unpin>(
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
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
            crate::rar::VerifyData {
                name: &entry_name,
                expected_crc: Some(expected_crc),
                expected_blake2: None,
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
    /// Downloads the file header + compressed data, assembles them into a minimal
    /// valid RAR4 archive in a temp file, and decompresses with `unrar`.
    async fn extract_via_unrar_cloud<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;
        let password_bytes = password.map(|p| p.to_vec());

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry_name, uncompressed_size);
        }

        // 1. Download the file header bytes.
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

        let (cancel, on_progress) = build_unrar_progress_hooks(&progress);
        let handle = tokio::runtime::Handle::current();

        // 3. Assemble synthetic RAR4 archive and extract with unrar.
        let out_tmp =
            tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile, CoreError> {
                let temp_file =
                    tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                        detail: format!("failed to create temp file: {e}"),
                    })?;

                {
                    use std::io::Write;
                    let mut writer = std::io::BufWriter::new(temp_file.as_file());
                    for chunk in [archive_preamble.as_ref(), file_header_bytes.as_ref()] {
                        writer
                            .write_all(chunk)
                            .map_err(|e| CoreError::BitstreamError {
                                detail: format!("temp file write failed: {e}"),
                            })?;
                    }

                    // Stream compressed data from network to temp file (O(1) memory).
                    let mut sync_reader = SyncIoBridge::new_with_handle(compressed_stream, handle);
                    std::io::copy(&mut sync_reader, &mut writer).map_err(|e| {
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

                    writer.write_all(end_block.as_ref()).map_err(|e| {
                        CoreError::BitstreamError {
                            detail: format!("temp file write failed: {e}"),
                        }
                    })?;

                    writer.flush().map_err(|e| CoreError::BitstreamError {
                        detail: format!("temp file flush failed: {e}"),
                    })?;
                }

                // Create second temp file for extraction output.
                let out_tmp =
                    tempfile::NamedTempFile::new().map_err(|e| CoreError::BitstreamError {
                        detail: format!("failed to create extraction temp file: {e}"),
                    })?;
                let out_path = out_tmp.path().to_path_buf();

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
                    detail: "synthetic RAR4 archive produced no entries".into(),
                })?;
                header
                    .extract_to_with_progress(&out_path, Some(cancel), Some(on_progress))
                    .map_err(map_unrar_error)?;

                // temp_file dropped here — temp file deleted automatically.
                Ok(out_tmp)
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_stream_from_file(
            out_tmp.into_file(),
            crate::rar::VerifyData {
                name: &entry_name,
                expected_crc: Some(expected_crc),
                expected_blake2: None,
                compressed_size: loc.data_size,
            },
            sink,
            &progress,
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

        // Local path: delegate to default loop (unrar opens the real file directly).
        if self.local_path.is_some() {
            return extract_selected_default(
                self,
                entries,
                sink_factory,
                progress,
                password,
                limits,
            )
            .await;
        }

        // Cloud path: build clusters.
        let mut batch = BatchResult {
            bytes_written: 0,
            succeeded: 0,
            failed: vec![],
        };

        let work: Vec<(&ArchiveEntry, &Rar4EntryLocation)> = entries
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
            return Ok(batch);
        }

        let regions: Vec<FetchRegion<(&ArchiveEntry, &Rar4EntryLocation)>> = work
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
            if progress.lock().unwrap().should_cancel() {
                return Err(CoreError::Cancelled);
            }

            let first = &cluster[0];
            let last = &cluster[cluster.len() - 1];
            let cluster_start = first.offset;
            let cluster_end = last.offset.saturating_add(last.total_len).min(archive_size);
            let cluster_len = cluster_end.saturating_sub(cluster_start);

            let stream = self
                .reader
                .read_stream(cluster_start, cluster_len)
                .await
                .map_err(CoreError::Io)?;

            let cluster_data: Vec<(ArchiveEntry, Rar4EntryLocation)> = cluster
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
                        if prog.lock().unwrap().should_cancel() {
                            return Err(CoreError::Cancelled);
                        }

                        let gap = loc.header_offset.saturating_sub(stream_pos);
                        if gap > 0 {
                            std::io::copy(&mut (&mut sync_reader).take(gap), &mut std::io::sink())
                                .map_err(|e| CoreError::BitstreamError {
                                    detail: format!("skip gap failed: {e}"),
                                })?;
                            stream_pos += gap;
                        }

                        let mut header_bytes = vec![0u8; loc.header_size as usize];
                        sync_reader.read_exact(&mut header_bytes).map_err(|e| {
                            CoreError::BitstreamError {
                                detail: format!("read header failed: {e}"),
                            }
                        })?;
                        stream_pos += loc.header_size;

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
                    detail: format!("RAR4 cluster task panicked: {e}"),
                })??;

            for (result, region) in results.into_iter().zip(cluster.iter()) {
                let (entry, loc) = region.payload;
                match result {
                    Ok(out_tmp) => {
                        if let Some(mut sink) = sink_factory(entry) {
                            let verify_res = verify_and_stream_from_file(
                                out_tmp.into_file(),
                                crate::rar::VerifyData {
                                    name: &entry.name,
                                    expected_crc: Some(loc.crc32),
                                    expected_blake2: None,
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
}

// ── ArchiveReader trait implementation ────────────────────────────────────────

impl<R: ReadAt + Send + Sync + 'static> ArchiveReader for Rar4Archive<R> {
    fn entries(&self) -> &[ArchiveEntry] {
        Rar4Archive::entries(self)
    }

    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        Rar4Archive::extraction_cost(self, entry)
    }

    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError> {
        Rar4Archive::extract(self, entry, sink, progress, password, limits).await
    }

    async fn test(
        &self,
        entry: &ArchiveEntry,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        limits: &crate::ResourceLimits,
    ) -> Result<(), CoreError> {
        Rar4Archive::test(self, entry, progress, limits).await
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
        Rar4Archive::extract_selected(self, entries, sink_factory, progress, password, limits)
    }

    fn scan_info(&self) -> crate::scan::ScanInfo {
        if self.scan_complete {
            crate::scan::ScanInfo::Ready
        } else {
            crate::scan::ScanInfo::Required {
                estimated_bytes: self.archive_size,
                reason: "RAR4 archive requires a one-time sequential scan.",
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
            progress.lock().unwrap().on_scan_start(size);
        }
        let (entries, locations) =
            Self::run_elastic_scan(&self.reader, preamble_end, size, &progress, None, limits)
                .await?;
        let entry_count = entries.len();
        self.entries = entries;
        self.locations = locations;
        self.scan_complete = true;
        {
            progress.lock().unwrap().on_scan_complete(entry_count);
        }
        Ok(())
    }

    async fn extract_all_streaming<Filter, SinkFn>(
        &mut self,
        filter: Filter,
        mut sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
        limits: &crate::ResourceLimits,
    ) -> Result<u64, CoreError>
    where
        Filter: Fn(&ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        // Local path: loop over filtered entries, extract to buffer, write to sink.
        if self.local_path.is_some() {
            let matching: Vec<usize> = self
                .entries
                .iter()
                .filter(|e| !e.is_directory && filter(e))
                .map(|e| e.index)
                .collect();

            let mut total: u64 = 0;
            for idx in matching {
                let entry = self.entries[idx].clone();
                let Some(mut sync_sink) = sink_factory(&entry) else {
                    continue;
                };
                let mut buf = Vec::new();
                let bytes = Rar4Archive::extract(
                    self,
                    &entry,
                    &mut buf,
                    progress.clone(),
                    password,
                    limits,
                )
                .await?;
                use std::io::Write;
                sync_sink
                    .write_all(&buf)
                    .map_err(|e| CoreError::BitstreamError {
                        detail: format!("write to sink failed: {e}"),
                    })?;
                total += bytes;
            }
            return Ok(total);
        }

        // Cloud path: pipelined streaming splitter.
        let preamble_end = self.archive_preamble.len() as u64;
        let size = self.archive_size;

        let (tx, rx) = tokio::sync::mpsc::channel::<SnippetMsg>(2);
        let password_owned = password.map(|p| p.to_vec());

        // Spawn consumer.
        let consumer_progress = progress.clone();
        let consumer = tokio::spawn(consume_snippets(
            rx,
            sink_factory,
            consumer_progress,
            password_owned,
            limits.clone(),
        ));

        // Build snippet config.
        let config = SnippetConfig {
            sender: tx,
            preamble: self.archive_preamble.clone(),
            end_block: self.end_block.clone(),
            filter: Box::new(filter),
        };

        {
            progress.lock().unwrap().on_scan_start(size);
        }

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

fn snippet_write_err(e: std::io::Error) -> CoreError {
    CoreError::BitstreamError {
        detail: format!("snippet write: {e}"),
    }
}

/// Map a stream `io::Error` to `CoreError` for the elastic scan.
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

/// Map a RAR4 method byte to the codec layer's `CompressionMethod`.
///
/// Only Store (0x30) maps cleanly. Methods 0x31-0x35 are proprietary RAR4
/// algorithms handled by `unrar` — no `CompressionMethod` equivalent exists.
fn rar4_method_to_codec(method: u8) -> Option<CompressionMethod> {
    match method {
        0x30 => Some(CompressionMethod::Store),
        _ => None,
    }
}
