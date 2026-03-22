//! RAR4 format support.
//!
//! Custom header parser for listing (enables cloud sniping via range requests).
//! Extraction delegates to `unrar` — local shortcut or synthetic archive for cloud.

pub(crate) mod parse;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

use crate::archive_reader::ArchiveReader;
use crate::codec::CompressionMethod;
use crate::io::LocalPath;
use crate::junk;
use crate::progress::ProgressReporter;
use crate::rar::{map_unrar_error, verify_and_write, RAR4_SIGNATURE};
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

/// Private per-entry location data, parallel to `Rar4Archive::entries`.
pub(crate) struct Rar4EntryLocation {
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
    #[allow(private_bounds)]
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

        // 4. Always scan sequentially (RAR4 has no Quick Open).
        let (entries, locations) =
            Self::run_sequential_scan(&reader, chunk, 0, preamble_end as u64, size).await?;

        Ok(Self {
            reader,
            entries,
            locations,
            archive_preamble,
            end_block,
            local_path,
        })
    }

    /// All entries in the archive (solid and split-volume entries are omitted).
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Entries with OS-generated junk filtered out.
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries.iter().filter(|e| !junk::is_junk(e)).collect()
    }

    /// Bytes that must be transferred to produce this entry.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        self.locations[entry.index].data_size
    }

    /// Sequential metadata scan.
    ///
    /// Iterates RAR4 blocks from `start_offset`, building the entry and location lists.
    async fn run_sequential_scan(
        reader: &R,
        initial_buffer: Bytes,
        initial_buffer_start: u64,
        start_offset: u64,
        size: u64,
    ) -> Result<(Vec<ArchiveEntry>, Vec<Rar4EntryLocation>), CoreError> {
        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<Rar4EntryLocation> = Vec::new();

        let mut offset = start_offset;
        let mut buffer: Bytes = initial_buffer;
        let mut buffer_start: u64 = initial_buffer_start;

        loop {
            if offset >= size {
                break;
            }

            // Refetch when we're too close to the buffer end to parse even a minimal header.
            let buf_remaining =
                (buffer_start + buffer.len() as u64).saturating_sub(offset);
            if buf_remaining < 64 {
                let fetch_size = (64 * 1024_u64).min(size - offset);
                buffer = reader.read_range(offset, fetch_size).await?;
                buffer_start = offset;
            }

            let local_offset = (offset - buffer_start) as usize;
            let block = parse_block_header(&buffer[local_offset..], offset)?;
            let total_size = block.total_size;

            match block.head_type {
                BLOCK_FILE => {
                    // Ensure the full file header is in the buffer.
                    let header_end = local_offset + block.head_size as usize;
                    if header_end > buffer.len() {
                        let fetch_size = (64 * 1024_u64).min(size - offset);
                        buffer = reader.read_range(offset, fetch_size).await?;
                        buffer_start = offset;
                    }

                    let local_offset = (offset - buffer_start) as usize;
                    let file_hdr = parse_file_header(&buffer[local_offset..], block)?;

                    let is_solid = file_hdr.raw.head_flags & FLAG_SOLID != 0;
                    let is_split = file_hdr.raw.head_flags & (FLAG_CONTINUED_PREV | FLAG_CONTINUED_NEXT) != 0;

                    let header_size = file_hdr.raw.head_size as u64;
                    let data_size = file_hdr.pack_size;
                    let advance_by = header_size + data_size;

                    if !is_solid && !is_split {
                        let modified_at = dos_datetime_to_system_time(
                            (file_hdr.ftime >> 16) as u16,
                            (file_hdr.ftime & 0xFFFF) as u16,
                        );
                        let compression_method = rar4_method_to_codec(file_hdr.method);
                        let entry = ArchiveEntry {
                            name: file_hdr.name.clone(),
                            compressed_size: data_size,
                            uncompressed_size: file_hdr.unp_size,
                            is_directory: file_hdr.is_directory,
                            is_encrypted: file_hdr.is_encrypted,
                            modified_at,
                            compression_method,
                            index: entries.len(),
                        };
                        let location = Rar4EntryLocation {
                            header_offset: offset,
                            header_size,
                            data_offset: offset + header_size,
                            data_size,
                            crc32: file_hdr.file_crc,
                            method: file_hdr.method,
                            is_encrypted: file_hdr.is_encrypted,
                        };
                        entries.push(entry);
                        locations.push(location);
                    }

                    offset += advance_by;
                }
                BLOCK_END => break,
                _ => {
                    offset += total_size;
                }
            }
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
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        let progress = Arc::new(Mutex::new(()));
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
                let (data, _) = header.read().map_err(map_unrar_error)?;

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

    async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        Rar4Archive::test(self, entry).await
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

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
