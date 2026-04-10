//! RAR4 format support.
//!
//! Custom header parser for listing (enables cloud sniping via range requests).
//! Extraction delegates to `unrar` — local shortcut or synthetic archive for cloud.

pub(crate) mod parse;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

use crate::archive_reader::ArchiveReader;
use crate::codec::CompressionMethod;
use crate::io::LocalPath;
use crate::junk;
use crate::progress::ProgressReporter;
use crate::rar::{build_unrar_progress_hooks, consume_snippets, map_unrar_error, verify_and_write, SnippetConfig, SnippetMsg, RAR4_SIGNATURE};
use crate::rar4::parse::{
    build_rar4_end_block, parse_block_header, parse_file_header, parse_main_header, BLOCK_END,
    BLOCK_FILE,
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
    pub async fn open(reader: R) -> Result<Self, CoreError>
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

        let preamble_end =
            (main_header.raw.archive_offset + main_header.raw.total_size) as usize;
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
                Self::run_elastic_scan(&reader, preamble_end as u64, size, &no_op, None).await?;
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
        if self.scan_complete { &self.entries } else { &[] }
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
    ) -> Result<(Vec<ArchiveEntry>, Vec<Rar4EntryLocation>), CoreError> {
        const LARGE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MiB

        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<Rar4EntryLocation> = Vec::new();

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

            // RAR4 common block header: 7 bytes minimum, 11 with FLAG_LONG_BLOCK.
            fill!(11);
            if buf.len().saturating_sub(buf_pos) < 7 {
                break;
            }

            let block = parse_block_header(&buf[buf_pos..], offset)?;
            let head_size = block.head_size as usize;

            // Ensure full file header is in buf (head_size bytes from block start).
            fill!(head_size);

            match block.head_type {
                BLOCK_FILE => {
                    let file_hdr = parse_file_header(&buf[buf_pos..], block)?;

                    let is_solid = file_hdr.raw.head_flags & FLAG_SOLID != 0;
                    let is_split = file_hdr.raw.head_flags
                        & (FLAG_CONTINUED_PREV | FLAG_CONTINUED_NEXT)
                        != 0;

                    let header_size_u64 = file_hdr.raw.head_size as u64;
                    let pack_size = file_hdr.pack_size;
                    let total_advance = header_size_u64 + pack_size;

                    if !is_solid && !is_split {
                        let modified_at = dos_datetime_to_system_time(
                            (file_hdr.ftime >> 16) as u16,
                            (file_hdr.ftime & 0xFFFF) as u16,
                        );
                        let entry = ArchiveEntry {
                            name: file_hdr.name.clone(),
                            compressed_size: pack_size,
                            uncompressed_size: file_hdr.unp_size,
                            is_directory: file_hdr.is_directory,
                            is_encrypted: file_hdr.is_encrypted,
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
                            let head_bytes_owned =
                                buf[buf_pos..buf_pos + head_size].to_vec();

                                let temp_file = tokio::task::spawn_blocking(
                                    tempfile::NamedTempFile::new,
                                )
                                .await
                                .map_err(|e| CoreError::BitstreamError {
                                    detail: format!("spawn temp: {e}"),
                                })?
                                .map_err(|e| CoreError::BitstreamError {
                                    detail: format!("create temp: {e}"),
                                })?;

                                let std_file = temp_file
                                    .as_file()
                                    .try_clone()
                                    .map_err(|e| CoreError::BitstreamError {
                                        detail: format!("clone temp file: {e}"),
                                    })?;
                                let mut snippet_writer =
                                    tokio::io::BufWriter::with_capacity(
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
                                let data_in_buf =
                                    (buf.len() - buf_pos).min(pack_size as usize);
                                if data_in_buf > 0 {
                                    snippet_writer
                                        .write_all(
                                            &buf[buf_pos..buf_pos + data_in_buf],
                                        )
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
                                snippet_writer
                                    .flush()
                                    .await
                                    .map_err(snippet_write_err)?;
                                drop(snippet_writer);

                                let (_, snippet_path) = temp_file.keep().map_err(
                                    |e| CoreError::BitstreamError {
                                        detail: format!("keep temp file: {e}"),
                                    },
                                )?;

                                if config
                                    .sender
                                    .send(SnippetMsg {
                                        path: snippet_path.clone(),
                                        entry: entry.clone(),
                                        crc32: Some(file_hdr.file_crc),
                                        blake2_hash: None,
                                    })
                                    .await
                                    .is_err()
                                {
                                    let _ = std::fs::remove_file(&snippet_path);
                                    return Err(CoreError::BitstreamError {
                                        detail: "extraction consumer dropped unexpectedly"
                                            .into(),
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
    ) -> Result<u64, CoreError> {
        if entry.is_directory {
            return Ok(0);
        }

        let loc = &self.locations[entry.index];

        if loc.is_encrypted && password.is_none() {
            return Err(CoreError::PasswordRequired);
        }

        // Encrypted entries must go through unrar (it handles AES decryption).
        if loc.is_encrypted {
            return self.extract_compressed(entry, loc, sink, progress, password).await;
        }

        match loc.method {
            0x30 => self.extract_store(entry, loc, sink, progress).await,
            0x31..=0x35 => self.extract_compressed(entry, loc, sink, progress, None).await,
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
    ) -> Result<(), CoreError> {
        self.extract(entry, &mut tokio::io::sink(), progress, None)
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
    ) -> Result<u64, CoreError> {
        let entry_name = entry.name.clone();
        let uncompressed_size = entry.uncompressed_size;
        let expected_crc = loc.crc32;

        // Step 1 (async): streaming AsyncRead for the raw stored data.
        let stream = self
            .reader
            .read_stream(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        // Step 2 (spawn_blocking): read chunks, compute CRC32, accumulate.
        let handle = tokio::runtime::Handle::current();
        let mut sync_reader = SyncIoBridge::new_with_handle(stream, handle);

        let verified: Vec<u8> =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
                const CHUNK_SIZE: usize = 64 * 1024;

                let mut crc_hasher = crc32fast::Hasher::new();
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
                            && let Ok(read_error) =
                                inner.downcast::<crate::io::ReadError>()
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

    /// Dispatch compressed extraction to `unrar` — local or cloud path.
    async fn extract_compressed<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, CoreError> {
        if let Some(path) = &self.local_path {
            return self
                .extract_via_unrar_local(entry, loc, path, sink, progress, password)
                .await;
        }
        self.extract_via_unrar_cloud(entry, loc, sink, progress, password)
            .await
    }

    /// Extract a compressed (or encrypted) entry from a **local** archive via `unrar`.
    async fn extract_via_unrar_local<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        loc: &Rar4EntryLocation,
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
        let password_bytes = password.map(|p| p.to_vec());

        {
            let mut p = progress.lock().unwrap();
            p.on_entry_start(&entry_name, uncompressed_size);
        }

        let (cancel, on_progress) = build_unrar_progress_hooks(&progress);
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
                        let (data, _) = header
                            .read_with_progress(Some(cancel), Some(on_progress))
                            .map_err(map_unrar_error)?;
                        return Ok(data);
                    }
                    archive = header.skip().map_err(map_unrar_error)?;
                }
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_write(&data, &entry_name, Some(expected_crc), None, sink, &progress).await
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

        // 2. Download the compressed data.
        let compressed_data = self
            .reader
            .read_range(loc.data_offset, loc.data_size)
            .await
            .map_err(CoreError::Io)?;

        let archive_preamble = self.archive_preamble.clone();
        let end_block = self.end_block.clone();

        let (cancel, on_progress) = build_unrar_progress_hooks(&progress);

        // 3. Assemble synthetic RAR4 archive and extract with unrar.
        let data =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
                let temp_file = tempfile::NamedTempFile::new().map_err(|e| {
                    CoreError::BitstreamError {
                        detail: format!("failed to create temp file: {e}"),
                    }
                })?;

                {
                    use std::io::Write;
                    let mut writer = std::io::BufWriter::new(temp_file.as_file());
                    for chunk in [
                        archive_preamble.as_ref(),
                        file_header_bytes.as_ref(),
                        compressed_data.as_ref(),
                        end_block.as_ref(),
                    ] {
                        writer.write_all(chunk).map_err(|e| CoreError::BitstreamError {
                            detail: format!("temp file write failed: {e}"),
                        })?;
                    }
                    writer.flush().map_err(|e| CoreError::BitstreamError {
                        detail: format!("temp file flush failed: {e}"),
                    })?;
                }

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
                let (data, _) = header
                    .read_with_progress(Some(cancel), Some(on_progress))
                    .map_err(map_unrar_error)?;

                // temp_file dropped here — temp file deleted automatically.
                Ok(data)
            })
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("extraction task panicked: {e}"),
            })??;

        verify_and_write(&data, &entry_name, Some(expected_crc), None, sink, &progress).await
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
    ) -> Result<u64, CoreError> {
        Rar4Archive::extract(self, entry, sink, progress, password).await
    }

    async fn test(&self, entry: &ArchiveEntry, progress: Arc<Mutex<dyn ProgressReporter>>) -> Result<(), CoreError> {
        Rar4Archive::test(self, entry, progress).await
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
    ) -> Result<(), CoreError> {
        let preamble_end = self.archive_preamble.len() as u64;
        let size = self.archive_size;
        {
            progress.lock().unwrap().on_scan_start(size);
        }
        let (entries, locations) =
            Self::run_elastic_scan(&self.reader, preamble_end, size, &progress, None).await?;
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
                let bytes =
                    Rar4Archive::extract(self, &entry, &mut buf, progress.clone(), password)
                        .await?;
                use std::io::Write;
                sync_sink.write_all(&buf).map_err(|e| CoreError::BitstreamError {
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
        let consumer =
            tokio::spawn(consume_snippets(rx, sink_factory, consumer_progress, password_owned));

        // Build snippet config.
        let config = SnippetConfig {
            sender: tx,
            preamble: self.archive_preamble.clone(),
            end_block: self.end_block.clone(),
            filter: Box::new(filter),
        };

        { progress.lock().unwrap().on_scan_start(size); }

        // Run producer (in current task — borrows self.reader).
        let (entries, locations) =
            Self::run_elastic_scan(&self.reader, preamble_end, size, &progress, Some(config))
                .await?;

        // Await consumer.
        let total_bytes = consumer
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("consumer panicked: {e}"),
            })??;

        // Populate self with harvested metadata.
        let entry_count = entries.len();
        self.entries = entries;
        self.locations = locations;
        self.scan_complete = true;

        { progress.lock().unwrap().on_scan_complete(entry_count); }

        Ok(total_bytes)
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn snippet_write_err(e: std::io::Error) -> CoreError {
    CoreError::BitstreamError { detail: format!("snippet write: {e}") }
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
