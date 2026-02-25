//! Low-level ZIP structure parsing.
//!
//! All functions operate directly on a [`ReadAt`] source, issuing the
//! minimum number of range reads needed.

use bytes::Bytes;

use crate::{ArchiveEntry, ArchiveError, ReadAt};

// ── ZIP magic numbers ───────────────────────────────────────────────
const EOCD_SIGNATURE: u32 = 0x06054b50;
const CD_ENTRY_SIGNATURE: u32 = 0x02014b50;
const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x04034b50;

/// Fixed size of the EOCD record (without the variable-length comment).
const EOCD_MIN_SIZE: usize = 22;

/// Maximum bytes to scan backwards for the EOCD signature.
/// Covers the EOCD itself (22 bytes) + max comment length (65535 bytes).
const EOCD_SEARCH_LEN: u64 = 65557;

// ── ZIP compression methods ─────────────────────────────────────────
/// File is stored without compression.
pub const METHOD_STORED: u16 = 0;
/// File is compressed with Deflate.
pub const METHOD_DEFLATE: u16 = 8;

// ── Helper: read little-endian integers from a byte slice ───────────

fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

// ── EOCD ────────────────────────────────────────────────────────────

/// Parsed End of Central Directory record.
pub struct Eocd {
    /// Total number of entries in the Central Directory.
    pub entry_count: u16,
    /// Size of the Central Directory in bytes.
    pub cd_size: u32,
    /// Byte offset of the Central Directory from the start of the archive.
    pub cd_offset: u32,
}

/// Locate and parse the End of Central Directory record.
///
/// Reads at most the last 65,557 bytes of the source and scans backwards
/// for the EOCD signature.
pub async fn find_eocd(reader: &impl ReadAt) -> Result<Eocd, ArchiveError> {
    let file_size = reader.size().await?;

    if file_size < EOCD_MIN_SIZE as u64 {
        return Err(ArchiveError::Other(
            "file too small to be a valid ZIP".into(),
        ));
    }

    // Read the tail of the file (up to 65,557 bytes).
    let search_len = file_size.min(EOCD_SEARCH_LEN);
    let search_offset = file_size - search_len;
    let tail = reader.read_range(search_offset, search_len).await?;

    // Scan backwards for the EOCD signature (0x06054b50).
    let tail_len = tail.len();
    let mut pos = None;
    for i in (0..=tail_len.saturating_sub(EOCD_MIN_SIZE)).rev() {
        if read_u32_le(&tail, i) == EOCD_SIGNATURE {
            pos = Some(i);
            break;
        }
    }

    let pos = pos.ok_or_else(|| {
        ArchiveError::Other("EOCD signature not found — not a valid ZIP file".into())
    })?;

    // Parse the EOCD fields.
    let entry_count = read_u16_le(&tail, pos + 10);
    let cd_size = read_u32_le(&tail, pos + 12);
    let cd_offset = read_u32_le(&tail, pos + 16);

    Ok(Eocd {
        entry_count,
        cd_size,
        cd_offset,
    })
}

// ── Central Directory ───────────────────────────────────────────────

/// Parse the Central Directory and return a list of archive entries.
pub async fn parse_central_directory(
    reader: &impl ReadAt,
    eocd: &Eocd,
) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    let cd_bytes = reader
        .read_range(eocd.cd_offset as u64, eocd.cd_size as u64)
        .await?;

    let mut entries = Vec::with_capacity(eocd.entry_count as usize);
    let mut cursor = 0usize;

    for _ in 0..eocd.entry_count {
        if cursor + 46 > cd_bytes.len() {
            return Err(ArchiveError::Other(
                "unexpected end of Central Directory".into(),
            ));
        }

        let sig = read_u32_le(&cd_bytes, cursor);
        if sig != CD_ENTRY_SIGNATURE {
            return Err(ArchiveError::Other(format!(
                "invalid CD entry signature: {sig:#010x}"
            )));
        }

        let compression_method = read_u16_le(&cd_bytes, cursor + 10);
        let compressed_size = read_u32_le(&cd_bytes, cursor + 20) as u64;
        let uncompressed_size = read_u32_le(&cd_bytes, cursor + 24) as u64;
        let name_len = read_u16_le(&cd_bytes, cursor + 28) as usize;
        let extra_len = read_u16_le(&cd_bytes, cursor + 30) as usize;
        let comment_len = read_u16_le(&cd_bytes, cursor + 32) as usize;
        let local_header_offset = read_u32_le(&cd_bytes, cursor + 42) as u64;

        let name_start = cursor + 46;
        let name_end = name_start + name_len;

        if name_end > cd_bytes.len() {
            return Err(ArchiveError::Other(
                "file name extends beyond Central Directory".into(),
            ));
        }

        let name = String::from_utf8_lossy(&cd_bytes[name_start..name_end]).into_owned();
        let is_directory = name.ends_with('/');

        entries.push(ArchiveEntry {
            name,
            compressed_size,
            uncompressed_size,
            is_directory,
            offset: local_header_offset,
            compression_method,
        });

        cursor = name_end + extra_len + comment_len;
    }

    Ok(entries)
}

// ── Local File Header + Data ────────────────────────────────────────

/// Read the compressed data for a single entry by parsing its Local File
/// Header to determine the actual data offset.
pub async fn read_local_file_data(
    reader: &impl ReadAt,
    entry: &ArchiveEntry,
) -> Result<Bytes, ArchiveError> {
    // Read the fixed-size part of the Local File Header (30 bytes).
    let lfh = reader.read_range(entry.offset, 30).await?;

    let sig = read_u32_le(&lfh, 0);
    if sig != LOCAL_FILE_HEADER_SIGNATURE {
        return Err(ArchiveError::Other(format!(
            "invalid Local File Header signature: {sig:#010x}"
        )));
    }

    let name_len = read_u16_le(&lfh, 26) as u64;
    let extra_len = read_u16_le(&lfh, 28) as u64;

    // Compressed data starts right after the LFH + name + extra fields.
    let data_offset = entry.offset + 30 + name_len + extra_len;

    if entry.compressed_size == 0 {
        return Ok(Bytes::new());
    }

    reader.read_range(data_offset, entry.compressed_size).await
}
