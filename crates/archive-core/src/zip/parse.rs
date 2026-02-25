//! Low-level ZIP / ZIP64 structure parsing.
//!
//! All functions operate directly on a [`ReadAt`] source, issuing the
//! minimum number of range reads needed.

use bytes::Bytes;

use crate::{ArchiveEntry, ArchiveError, ReadAt};

// ── ZIP magic numbers ───────────────────────────────────────────────
const EOCD_SIGNATURE: u32 = 0x06054b50;
const EOCD64_RECORD_SIGNATURE: u32 = 0x06064b50;
const EOCD64_LOCATOR_SIGNATURE: u32 = 0x07064b50;
const CD_ENTRY_SIGNATURE: u32 = 0x02014b50;
const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x04034b50;

/// ZIP64 sentinel: when u32 fields hold this value, the real value is in ZIP64 extensions.
const ZIP64_MAGIC32: u32 = 0xFFFF_FFFF;
/// Same for u16 fields (entry count).
const ZIP64_MAGIC16: u16 = 0xFFFF;

/// Fixed size of the EOCD record (without the variable-length comment).
const EOCD_MIN_SIZE: usize = 22;
/// Fixed size of the ZIP64 EOCD Locator.
const EOCD64_LOCATOR_SIZE: usize = 20;
/// Minimum fixed size of the ZIP64 EOCD Record (without the extensible data sector).
const EOCD64_RECORD_MIN_SIZE: usize = 56;

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

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

// ── EOCD ────────────────────────────────────────────────────────────

/// Parsed End of Central Directory — values are always u64 regardless of whether
/// the source was a standard ZIP or ZIP64 archive.
pub struct Eocd {
    /// Total number of entries in the Central Directory.
    pub entry_count: u64,
    /// Size of the Central Directory in bytes.
    pub cd_size: u64,
    /// Byte offset of the Central Directory from the start of the archive.
    pub cd_offset: u64,
}

/// Locate and parse the End of Central Directory record, with full ZIP64 support.
///
/// ## ZIP64 handling
/// If the standard EOCD contains sentinel values (`0xFFFF` / `0xFFFFFFFF`), we
/// locate the ZIP64 EOCD Locator (immediately before the standard EOCD) and read
/// the ZIP64 EOCD Record to get the real 64-bit field values.
pub async fn find_eocd(reader: &impl ReadAt) -> Result<Eocd, ArchiveError> {
    let file_size = reader.size().await?;

    if file_size < EOCD_MIN_SIZE as u64 {
        return Err(ArchiveError::Other(
            "file too small to be a valid ZIP".into(),
        ));
    }

    // Read the tail of the file (up to 65,557 bytes) to find the EOCD.
    let search_len = file_size.min(EOCD_SEARCH_LEN);
    let search_offset = file_size - search_len;
    let tail = reader.read_range(search_offset, search_len).await?;

    // Scan backwards for the standard EOCD signature.
    let tail_len = tail.len();
    let mut eocd_pos = None;
    for i in (0..=tail_len.saturating_sub(EOCD_MIN_SIZE)).rev() {
        if read_u32_le(&tail, i) == EOCD_SIGNATURE {
            eocd_pos = Some(i);
            break;
        }
    }

    let pos = eocd_pos.ok_or_else(|| {
        ArchiveError::Other("EOCD signature not found — not a valid ZIP file".into())
    })?;

    // Parse the standard EOCD fields.
    let entry_count_16 = read_u16_le(&tail, pos + 10);
    let cd_size_32 = read_u32_le(&tail, pos + 12);
    let cd_offset_32 = read_u32_le(&tail, pos + 16);

    // If any sentinel values are present, this is a ZIP64 archive.
    let is_zip64 = entry_count_16 == ZIP64_MAGIC16
        || cd_size_32 == ZIP64_MAGIC32
        || cd_offset_32 == ZIP64_MAGIC32;

    if !is_zip64 {
        return Ok(Eocd {
            entry_count: entry_count_16 as u64,
            cd_size: cd_size_32 as u64,
            cd_offset: cd_offset_32 as u64,
        });
    }

    // ── ZIP64 path ──────────────────────────────────────────────────────
    // The ZIP64 EOCD Locator sits immediately before the standard EOCD.
    // It points us to the absolute file offset of the ZIP64 EOCD Record.
    if pos < EOCD64_LOCATOR_SIZE {
        return Err(ArchiveError::Other(
            "ZIP64 EOCD Locator not found in scanned tail".into(),
        ));
    }

    let locator_pos = pos - EOCD64_LOCATOR_SIZE;
    if read_u32_le(&tail, locator_pos) != EOCD64_LOCATOR_SIGNATURE {
        return Err(ArchiveError::Other(
            "ZIP64 EOCD Locator signature missing".into(),
        ));
    }

    // Bytes 8-15 of the locator = absolute file offset of the ZIP64 EOCD Record.
    let eocd64_abs_offset = read_u64_le(&tail, locator_pos + 8);

    // Convert to an offset within our already-read tail buffer.
    let eocd64_in_tail = eocd64_abs_offset.checked_sub(search_offset).and_then(|o| {
        if o + EOCD64_RECORD_MIN_SIZE as u64 <= tail_len as u64 {
            Some(o as usize)
        } else {
            None
        }
    });

    let eocd64_pos = match eocd64_in_tail {
        Some(p) => p,
        None => {
            // The ZIP64 EOCD Record is outside our tail buffer — do an extra read.
            let rec = reader
                .read_range(eocd64_abs_offset, EOCD64_RECORD_MIN_SIZE as u64)
                .await?;

            if read_u32_le(&rec, 0) != EOCD64_RECORD_SIGNATURE {
                return Err(ArchiveError::Other(
                    "ZIP64 EOCD Record signature mismatch".into(),
                ));
            }

            return Ok(Eocd {
                entry_count: read_u64_le(&rec, 32),
                cd_size: read_u64_le(&rec, 40),
                cd_offset: read_u64_le(&rec, 48),
            });
        }
    };

    if read_u32_le(&tail, eocd64_pos) != EOCD64_RECORD_SIGNATURE {
        return Err(ArchiveError::Other(
            "ZIP64 EOCD Record signature mismatch".into(),
        ));
    }

    Ok(Eocd {
        entry_count: read_u64_le(&tail, eocd64_pos + 32),
        cd_size: read_u64_le(&tail, eocd64_pos + 40),
        cd_offset: read_u64_le(&tail, eocd64_pos + 48),
    })
}

// ── Central Directory ───────────────────────────────────────────────

/// Parse the Central Directory and return a list of archive entries.
///
/// Handles ZIP64 extra fields inside each CD entry to get real u64 sizes
/// and offsets when the standard u32 fields hold sentinel values.
pub async fn parse_central_directory(
    reader: &impl ReadAt,
    eocd: &Eocd,
) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    let cd_bytes = reader.read_range(eocd.cd_offset, eocd.cd_size).await?;

    let mut entries = Vec::with_capacity(eocd.entry_count.min(65536) as usize);
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
        let mut compressed_size = read_u32_le(&cd_bytes, cursor + 20) as u64;
        let mut uncompressed_size = read_u32_le(&cd_bytes, cursor + 24) as u64;
        let name_len = read_u16_le(&cd_bytes, cursor + 28) as usize;
        let extra_len = read_u16_le(&cd_bytes, cursor + 30) as usize;
        let comment_len = read_u16_le(&cd_bytes, cursor + 32) as usize;
        let mut local_header_offset = read_u32_le(&cd_bytes, cursor + 42) as u64;

        let name_start = cursor + 46;
        let name_end = name_start + name_len;

        if name_end > cd_bytes.len() {
            return Err(ArchiveError::Other(
                "file name extends beyond Central Directory".into(),
            ));
        }

        // ── ZIP64 extra field parsing ──────────────────────────────────
        // If any u32/u16 field holds a sentinel value (0xFFFFFFFF / 0xFFFF),
        // the real value is in the ZIP64 extra field block.
        let extra_start = name_end;
        let extra_end = extra_start + extra_len;
        if extra_end <= cd_bytes.len() {
            let need_zip64 = compressed_size == ZIP64_MAGIC32 as u64
                || uncompressed_size == ZIP64_MAGIC32 as u64
                || local_header_offset == ZIP64_MAGIC32 as u64;

            if need_zip64 {
                parse_zip64_extra(
                    &cd_bytes[extra_start..extra_end],
                    &mut uncompressed_size,
                    &mut compressed_size,
                    &mut local_header_offset,
                );
            }
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

/// Parse ZIP64 extra fields to extract real u64 sizes and offsets.
///
/// ZIP64 extra data is a sequence of tag (u16) + size (u16) + data blocks.
/// Tag 0x0001 is the ZIP64 extended information extra field.
fn parse_zip64_extra(
    extra: &[u8],
    uncompressed_size: &mut u64,
    compressed_size: &mut u64,
    local_header_offset: &mut u64,
) {
    let mut i = 0;
    while i + 4 <= extra.len() {
        let tag = read_u16_le(extra, i);
        let size = read_u16_le(extra, i + 2) as usize;
        i += 4;

        if tag == 0x0001 {
            // ZIP64 extended info. Fields are present only if the corresponding
            // standard field held the sentinel value — we must read them in order.
            let mut j = 0;
            if *uncompressed_size == ZIP64_MAGIC32 as u64 && j + 8 <= size {
                *uncompressed_size = read_u64_le(&extra[i..], j);
                j += 8;
            }
            if *compressed_size == ZIP64_MAGIC32 as u64 && j + 8 <= size {
                *compressed_size = read_u64_le(&extra[i..], j);
                j += 8;
            }
            if *local_header_offset == ZIP64_MAGIC32 as u64 && j + 8 <= size {
                *local_header_offset = read_u64_le(&extra[i..], j);
            }
            break;
        }

        i += size;
    }
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
