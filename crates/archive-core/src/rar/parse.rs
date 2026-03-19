//! RAR5 low-level parsing primitives.
//!
//! All functions operate on `&[u8]` buffers already fetched via `ReadAt`.
//! No I/O happens here — the scanner layer is responsible for fetching data.

// Many struct fields are populated for use in Steps 3–6 (QO, extraction, encryption).
// Suppress dead_code until those steps are implemented.
#![allow(dead_code)]

use crate::CoreError;

// ── Signatures ────────────────────────────────────────────────────────────────

pub(crate) const RAR5_SIGNATURE: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];
pub(crate) const RAR4_SIGNATURE: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];

// ── Variable-length integer ───────────────────────────────────────────────────

/// Decode a RAR5 variable-length integer from `data` at `pos`.
///
/// Advances `pos` past consumed bytes. The high bit of each byte is a continuation
/// flag; the low 7 bits carry data in little-endian order.
pub(crate) fn read_vint(data: &[u8], pos: &mut usize) -> Result<u64, CoreError> {
    let start = *pos;
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return Err(CoreError::CorruptHeader {
                offset: start as u64,
                reason: "truncated vint".into(),
            });
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 63 {
            return Err(CoreError::CorruptHeader {
                offset: start as u64,
                reason: "vint overflow".into(),
            });
        }
    }
}

// ── Common block header ───────────────────────────────────────────────────────

/// Common fields present in every RAR5 block header.
#[derive(Debug)]
pub(crate) struct RawBlockHeader {
    /// Stored CRC32 of the header (covers header_size vint through end of extra area).
    pub header_crc: u32,
    /// Size of header from block_type vint through end of extra area (bytes).
    pub header_size: u64,
    /// Block type: 1=main, 2=file, 3=service, 4=archive encrypt, 5=end.
    pub block_type: u64,
    /// Block flags.
    pub flags: u64,
    /// Size of the extra area in bytes (0 if no extra area).
    pub extra_area_size: u64,
    /// Size of the data area in bytes (0 if no data area).
    pub data_size: u64,
    /// Absolute byte offset of this block in the archive stream.
    pub archive_offset: u64,
    /// Total bytes consumed by this block (header bytes + data area).
    pub total_size: u64,
    /// Offset within the `data` slice where type-specific fields begin.
    pub body_offset: usize,
}

/// Parse a RAR5 block header from `data`, which must start at the block boundary.
///
/// `archive_offset` is the absolute position of this block in the archive.
/// The caller must ensure `data` contains at least the full header before calling;
/// use a first-pass read to determine `total_size`, then fetch more if needed.
pub(crate) fn parse_block_header(
    data: &[u8],
    archive_offset: u64,
) -> Result<RawBlockHeader, CoreError> {
    if data.len() < 5 {
        return Err(CoreError::CorruptHeader {
            offset: archive_offset,
            reason: "buffer too small for block header".into(),
        });
    }

    // CRC32 (4 bytes LE) at offset 0.
    let header_crc = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

    // header_size vint at offset 4. CRC region starts here.
    let mut pos = 4usize;
    let crc_start = pos;
    let header_size = read_vint(data, &mut pos)?;

    // CRC region: header_size vint bytes + header_size bytes of header content.
    let crc_end = pos + header_size as usize;
    if crc_end > data.len() {
        return Err(CoreError::CorruptHeader {
            offset: archive_offset,
            reason: format!(
                "header claims size {header_size} but only {} bytes available",
                data.len().saturating_sub(pos)
            ),
        });
    }

    let computed_crc = crc32fast::hash(&data[crc_start..crc_end]);
    if computed_crc != header_crc {
        return Err(CoreError::CorruptHeader {
            offset: archive_offset,
            reason: format!(
                "header CRC mismatch: expected {header_crc:#010x}, got {computed_crc:#010x}"
            ),
        });
    }

    let block_type = read_vint(data, &mut pos)?;
    let flags = read_vint(data, &mut pos)?;

    let extra_area_size = if flags & 0x0001 != 0 {
        read_vint(data, &mut pos)?
    } else {
        0
    };

    let data_size = if flags & 0x0002 != 0 {
        read_vint(data, &mut pos)?
    } else {
        0
    };

    let body_offset = pos;
    let total_size = crc_end as u64 + data_size;

    Ok(RawBlockHeader {
        header_crc,
        header_size,
        block_type,
        flags,
        extra_area_size,
        data_size,
        archive_offset,
        total_size,
        body_offset,
    })
}

// ── Compression info ──────────────────────────────────────────────────────────

/// Decoded RAR5 compression info vint from a file header.
pub(crate) struct RarCompressionInfo {
    /// Algorithm version (0 or 1).
    pub version: u8,
    /// Per-file solid flag (entry depends on preceding entries' dictionary state).
    pub solid: bool,
    /// Compression method: 0=Store, 1–5=RAR proprietary compression.
    pub method: u8,
    /// Dictionary size in bytes (128 KiB << dict_exponent).
    pub dict_size: u64,
}

pub(crate) fn parse_compression_info(raw: u64) -> RarCompressionInfo {
    let version = (raw & 0x3F) as u8;
    let solid = (raw >> 6) & 0x01 != 0;
    let method = ((raw >> 7) & 0x07) as u8;
    let dict_exponent = (raw >> 10) & 0x1F;
    let dict_size = (128 * 1024u64).checked_shl(dict_exponent as u32).unwrap_or(u64::MAX);
    RarCompressionInfo { version, solid, method, dict_size }
}

// ── Extra area record structs ─────────────────────────────────────────────────

/// Parsed file encryption extra area record (type 0x01, AES-256).
pub(crate) struct RarFileEncryption {
    pub version: u64,
    /// PBKDF2 iteration count = 2^kdf_count.
    pub kdf_count: u8,
    pub salt: [u8; 16],
    pub iv: [u8; 16],
    /// 8-byte check value (present when encryption flags bit 0x0001 is set).
    pub check_value: Option<[u8; 8]>,
}

/// High-precision timestamps from extra area record type 0x03.
pub(crate) struct RarHighPrecisionTime {
    /// Modification time in nanoseconds since Unix epoch, if present.
    pub mtime_ns: Option<u64>,
    /// Creation time in nanoseconds since Unix epoch, if present.
    pub ctime_ns: Option<u64>,
    /// Access time in nanoseconds since Unix epoch, if present.
    pub atime_ns: Option<u64>,
}

// ── File / service header ─────────────────────────────────────────────────────

const FILE_FLAG_DIRECTORY: u64 = 0x0001;
const FILE_FLAG_MTIME_PRESENT: u64 = 0x0002;
const FILE_FLAG_CRC32_PRESENT: u64 = 0x0004;
const FILE_FLAG_UNPACKED_SIZE_UNKNOWN: u64 = 0x0008;

/// Parsed RAR5 file or service block header (block_type 2 or 3).
pub(crate) struct RarFileHeader {
    pub raw: RawBlockHeader,
    pub file_flags: u64,
    /// Uncompressed size (0 if `FILE_FLAG_UNPACKED_SIZE_UNKNOWN` is set).
    pub unpacked_size: u64,
    pub attributes: u64,
    /// Unix timestamp (seconds since 1970-01-01), if present.
    pub mtime: Option<u32>,
    /// CRC32 of the uncompressed data, if present.
    pub data_crc32: Option<u32>,
    pub compression: RarCompressionInfo,
    /// 0 = Windows, 1 = Unix.
    pub host_os: u8,
    /// Entry name, always UTF-8 with `/`-separated path components.
    pub name: String,
    pub is_directory: bool,
    // Extra area records:
    pub encryption: Option<RarFileEncryption>,
    pub blake2_hash: Option<[u8; 32]>,
    pub high_precision_time: Option<RarHighPrecisionTime>,
}

/// Parse a RAR5 file or service header.
///
/// `data` is the same slice passed to [`parse_block_header`], starting at the
/// block boundary (offset 0 = first byte of the block). `raw` is the already-parsed
/// common header from that same block.
pub(crate) fn parse_file_header(
    data: &[u8],
    raw: RawBlockHeader,
) -> Result<RarFileHeader, CoreError> {
    let offset = raw.archive_offset;
    let mut pos = raw.body_offset;

    let file_flags = read_vint(data, &mut pos)?;

    let unpacked_size = if file_flags & FILE_FLAG_UNPACKED_SIZE_UNKNOWN == 0 {
        read_vint(data, &mut pos)?
    } else {
        0
    };

    let attributes = read_vint(data, &mut pos)?;

    let mtime = if file_flags & FILE_FLAG_MTIME_PRESENT != 0 {
        check_len(data, pos, 4, offset, "mtime")?;
        let t = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        Some(t)
    } else {
        None
    };

    let data_crc32 = if file_flags & FILE_FLAG_CRC32_PRESENT != 0 {
        check_len(data, pos, 4, offset, "data_crc32")?;
        let c = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        Some(c)
    } else {
        None
    };

    let compression_raw = read_vint(data, &mut pos)?;
    let compression = parse_compression_info(compression_raw);

    let host_os_raw = read_vint(data, &mut pos)?;
    let host_os = host_os_raw as u8;

    let name_len = read_vint(data, &mut pos)? as usize;
    check_len(data, pos, name_len, offset, "filename")?;
    let name = String::from_utf8_lossy(&data[pos..pos + name_len])
        .into_owned()
        .replace('\\', "/");
    pos += name_len;

    let is_directory =
        (file_flags & FILE_FLAG_DIRECTORY != 0) || name.ends_with('/');

    let extra_area_size = raw.extra_area_size as usize;
    let (encryption, blake2_hash, high_precision_time) = if extra_area_size > 0 {
        check_len(data, pos, extra_area_size, offset, "extra area")?;
        parse_file_extra_area(&data[pos..pos + extra_area_size], offset)?
    } else {
        (None, None, None)
    };

    Ok(RarFileHeader {
        raw,
        file_flags,
        unpacked_size,
        attributes,
        mtime,
        data_crc32,
        compression,
        host_os,
        name,
        is_directory,
        encryption,
        blake2_hash,
        high_precision_time,
    })
}

/// Parse extra area records from a file/service header.
///
/// Unknown record types are skipped gracefully. Parse errors within individual
/// records are also skipped — a truncated record does not fail the whole header.
#[allow(clippy::type_complexity)]
fn parse_file_extra_area(
    data: &[u8],
    archive_offset: u64,
) -> Result<
    (
        Option<RarFileEncryption>,
        Option<[u8; 32]>,
        Option<RarHighPrecisionTime>,
    ),
    CoreError,
> {
    let mut encryption = None;
    let mut blake2_hash = None;
    let mut high_precision_time = None;

    let mut pos = 0usize;
    while pos < data.len() {
        let record_size = read_vint(data, &mut pos)? as usize;
        if pos + record_size > data.len() {
            break; // truncated record — stop, return what we have
        }
        let record_data = &data[pos..pos + record_size];
        pos += record_size;

        if record_data.is_empty() {
            continue;
        }

        let mut rpos = 0usize;
        let Ok(record_type) = read_vint(record_data, &mut rpos) else {
            continue;
        };

        match record_type {
            0x01 => {
                if let Ok(enc) =
                    parse_encryption_record(record_data, rpos, archive_offset)
                {
                    encryption = Some(enc);
                }
            }
            0x02 => {
                if let Ok(hash) = parse_hash_record(record_data, rpos) {
                    blake2_hash = Some(hash);
                }
            }
            0x03 => {
                if let Ok(time) = parse_time_record(record_data, rpos) {
                    high_precision_time = Some(time);
                }
            }
            _ => {} // unknown record type — skip
        }
    }

    Ok((encryption, blake2_hash, high_precision_time))
}

fn parse_encryption_record(
    data: &[u8],
    mut pos: usize,
    archive_offset: u64,
) -> Result<RarFileEncryption, CoreError> {
    let version = read_vint(data, &mut pos)?;
    let flags = read_vint(data, &mut pos)?;

    check_len(data, pos, 1, archive_offset, "encryption kdf_count")?;
    let kdf_count = data[pos];
    pos += 1;

    check_len(data, pos, 16, archive_offset, "encryption salt")?;
    let salt: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
    pos += 16;

    check_len(data, pos, 16, archive_offset, "encryption iv")?;
    let iv: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
    pos += 16;

    let check_value = if flags & 0x0001 != 0 && pos + 8 <= data.len() {
        Some(data[pos..pos + 8].try_into().unwrap())
    } else {
        None
    };

    Ok(RarFileEncryption { version, kdf_count, salt, iv, check_value })
}

fn parse_hash_record(data: &[u8], mut pos: usize) -> Result<[u8; 32], CoreError> {
    // hash_type vint: 0 = BLAKE2sp
    let hash_type = read_vint(data, &mut pos)?;
    if hash_type != 0 {
        return Err(CoreError::CorruptHeader {
            offset: 0,
            reason: format!("unsupported hash type {hash_type}"),
        });
    }
    if pos + 32 > data.len() {
        return Err(CoreError::CorruptHeader {
            offset: 0,
            reason: "hash record truncated".into(),
        });
    }
    Ok(data[pos..pos + 32].try_into().unwrap())
}

fn parse_time_record(data: &[u8], mut pos: usize) -> Result<RarHighPrecisionTime, CoreError> {
    if pos >= data.len() {
        return Ok(RarHighPrecisionTime { mtime_ns: None, ctime_ns: None, atime_ns: None });
    }

    let flags = read_vint(data, &mut pos)?;
    let unix_mode = flags & 0x0008 != 0;

    let mtime_ns = if flags & 0x0001 != 0 {
        parse_one_timestamp(data, &mut pos, unix_mode)
    } else {
        None
    };

    let ctime_ns = if flags & 0x0002 != 0 {
        parse_one_timestamp(data, &mut pos, unix_mode)
    } else {
        None
    };

    let atime_ns = if flags & 0x0004 != 0 {
        parse_one_timestamp(data, &mut pos, unix_mode)
    } else {
        None
    };

    Ok(RarHighPrecisionTime { mtime_ns, ctime_ns, atime_ns })
}

/// Parse one timestamp from the time record extra area.
///
/// In Unix mode: 4-byte u32 LE (seconds since 1970). In Windows FILETIME mode:
/// vint (100ns intervals since 1601-01-01).
fn parse_one_timestamp(data: &[u8], pos: &mut usize, unix_mode: bool) -> Option<u64> {
    if unix_mode {
        if *pos + 4 > data.len() {
            return None;
        }
        let secs = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap()) as u64;
        *pos += 4;
        Some(secs * 1_000_000_000)
    } else {
        // Windows FILETIME: 100ns ticks since 1601-01-01
        let ft = read_vint(data, pos).ok()?;
        // Subtract offset between 1601-01-01 and 1970-01-01: 11644473600 seconds
        let secs = (ft / 10_000_000).saturating_sub(11_644_473_600);
        let ns = (ft % 10_000_000) * 100;
        Some(secs * 1_000_000_000 + ns)
    }
}

// ── Main archive header ───────────────────────────────────────────────────────

/// Quick Open (QO) locator from the main header extra area (record type 0x01).
pub(crate) struct RarLocator {
    /// Offset of the QO service block measured from the **start** of the main header block
    /// (i.e. from its CRC32 field). Add this to `main_header.raw.archive_offset` to get the
    /// absolute archive offset of the QO block.
    pub quick_open_offset: Option<u64>,
    /// Offset of the recovery record from the start of the main header block.
    pub recovery_offset: Option<u64>,
}

/// Parsed RAR5 main archive header (block type 1).
pub(crate) struct RarMainHeader {
    pub raw: RawBlockHeader,
    pub archive_flags: u64,
    /// True if the archive is solid (files share a compression dictionary).
    pub is_solid: bool,
    /// Locator record from the main header extra area, if present.
    pub locator: Option<RarLocator>,
}

/// Parse the main archive header starting at `block_offset` within `data`.
///
/// `data` must be a buffer starting at offset 0 of the archive (after the RAR5
/// signature at bytes 0–7). Typically called with `block_offset = 8`.
pub(crate) fn parse_main_header(
    data: &[u8],
    block_offset: usize,
) -> Result<RarMainHeader, CoreError> {
    let archive_offset = block_offset as u64;
    let raw = parse_block_header(&data[block_offset..], archive_offset)?;

    if raw.block_type != 1 {
        return Err(CoreError::CorruptHeader {
            offset: archive_offset,
            reason: format!(
                "expected main header (type 1), got type {}",
                raw.block_type
            ),
        });
    }

    // Parse main-header-specific fields from the body slice (relative to block start).
    let block_data = &data[block_offset..];
    let mut pos = raw.body_offset;

    let archive_flags = read_vint(block_data, &mut pos)?;

    // Skip volume_number vint if present (flag 0x0002).
    if archive_flags & 0x0002 != 0 {
        read_vint(block_data, &mut pos)?;
    }

    let is_solid = archive_flags & 0x0004 != 0;

    let locator = if raw.extra_area_size > 0 {
        let extra_end = pos + raw.extra_area_size as usize;
        if extra_end <= block_data.len() {
            parse_main_extra_area(&block_data[pos..extra_end])
        } else {
            None
        }
    } else {
        None
    };

    Ok(RarMainHeader { raw, archive_flags, is_solid, locator })
}

/// Scan the main header extra area for the locator record (type 0x01).
fn parse_main_extra_area(data: &[u8]) -> Option<RarLocator> {
    let mut pos = 0usize;
    while pos < data.len() {
        let record_size = read_vint(data, &mut pos).ok()? as usize;
        if pos + record_size > data.len() {
            break;
        }
        let record_data = &data[pos..pos + record_size];
        pos += record_size;

        if record_data.is_empty() {
            continue;
        }

        let mut rpos = 0usize;
        let record_type = read_vint(record_data, &mut rpos).ok()?;

        if record_type == 0x01 {
            // Locator record
            let flags = read_vint(record_data, &mut rpos).ok()?;
            let quick_open_offset = if flags & 0x0001 != 0 {
                read_vint(record_data, &mut rpos).ok()
            } else {
                None
            };
            let recovery_offset = if flags & 0x0002 != 0 {
                read_vint(record_data, &mut rpos).ok()
            } else {
                None
            };
            return Some(RarLocator { quick_open_offset, recovery_offset });
        }
    }
    None
}

// ── Quick Open (QO) data parsing ─────────────────────────────────────────────

/// One cached block entry from the QO service block data area.
///
/// The `data` field contains the raw bytes of a cached file/service header block,
/// exactly as it appears at `original_archive_offset` in the archive. Callers should
/// parse `data` with [`parse_block_header`] followed by [`parse_file_header`] using
/// `original_archive_offset` as the `archive_offset` argument.
pub(crate) struct QoCachedBlock {
    /// Absolute archive offset where the original header block lives.
    pub original_archive_offset: u64,
    /// Raw bytes of the cached header block (header fields only, no data area).
    pub data: Vec<u8>,
}

/// Parse the data area of a QO service block.
///
/// `qo_data` is the raw byte slice of the QO block's data area (the bytes after
/// the QO service header). `qo_block_offset` is the absolute archive offset of the
/// QO service block header (used to resolve the relative `offset` vints stored in
/// each cache entry).
///
/// Each cache entry has the layout:
/// - `structure_crc32` (4 bytes LE) — CRC32 of the remaining structure fields
/// - `structure_size`  vint — byte count of the fields that follow the CRC
/// - `flags`           vint — currently always 0; reserved for future use
/// - `offset`          vint — distance from the QO block start to original header
/// - `data_size`       vint — byte count of the cached header bytes that follow
/// - `data`            bytes — the cached header block bytes
///
/// Unknown or corrupt entries are skipped gracefully.
pub(crate) fn parse_quick_open_data(
    qo_data: &[u8],
    qo_block_offset: u64,
) -> Vec<QoCachedBlock> {
    let mut blocks = Vec::new();
    let mut pos = 0usize;

    while pos < qo_data.len() {
        // ── CRC32 of the structure (4 bytes LE) ──────────────────────────────
        if pos + 4 > qo_data.len() {
            break;
        }
        let structure_crc =
            u32::from_le_bytes(qo_data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        // ── structure_size vint ───────────────────────────────────────────────
        let Ok(structure_size) = read_vint(qo_data, &mut pos) else {
            break;
        };
        let structure_start = pos; // CRC covers from here through structure_size bytes
        let structure_end = pos + structure_size as usize;
        if structure_end > qo_data.len() {
            break;
        }
        // A zero-size structure can never contain a valid entry.  More critically,
        // `structure_end == pos` means the loop would not advance — infinite loop.
        if structure_size == 0 {
            break;
        }

        // Verify CRC of the structure bytes.
        let computed_crc = crc32fast::hash(&qo_data[structure_start..structure_end]);
        if computed_crc != structure_crc {
            // Corrupt or misaligned — stop trying to parse further.
            break;
        }

        let structure_data = &qo_data[structure_start..structure_end];
        let mut spos = 0usize;

        // ── flags vint ───────────────────────────────────────────────────────
        let Ok(_flags) = read_vint(structure_data, &mut spos) else {
            pos = structure_end;
            continue;
        };

        // ── offset vint — distance from QO block start to original header ────
        let Ok(relative_offset) = read_vint(structure_data, &mut spos) else {
            pos = structure_end;
            continue;
        };
        let original_archive_offset = qo_block_offset + relative_offset;

        // ── data_size vint ────────────────────────────────────────────────────
        let Ok(data_size) = read_vint(structure_data, &mut spos) else {
            pos = structure_end;
            continue;
        };
        let data_size = data_size as usize;

        // ── cached header bytes ───────────────────────────────────────────────
        if spos + data_size > structure_data.len() {
            pos = structure_end;
            continue;
        }
        let data = structure_data[spos..spos + data_size].to_vec();

        blocks.push(QoCachedBlock { original_archive_offset, data });
        pos = structure_end;
    }

    blocks
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn check_len(
    data: &[u8],
    pos: usize,
    needed: usize,
    offset: u64,
    field: &str,
) -> Result<(), CoreError> {
    if pos + needed > data.len() {
        Err(CoreError::CorruptHeader {
            offset,
            reason: format!(
                "truncated {field}: need {needed} bytes at pos {pos}, have {}",
                data.len().saturating_sub(pos)
            ),
        })
    } else {
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── read_vint ─────────────────────────────────────────────────────────────

    #[test]
    fn vint_single_byte_value_5() {
        let data = [0x05u8];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 5);
        assert_eq!(pos, 1);
    }

    #[test]
    fn vint_max_single_byte() {
        // 0x7F = 127 — high bit clear, no continuation.
        let data = [0x7Fu8];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 127);
        assert_eq!(pos, 1);
    }

    #[test]
    fn vint_two_bytes_128() {
        // 128 = 0x80 0x01: first byte has high bit set (continue), value 0; second 1.
        let data = [0x80u8, 0x01];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 128);
        assert_eq!(pos, 2);
    }

    #[test]
    fn vint_two_bytes_300() {
        // 300 = 0x12C. In vint: 300 & 0x7F = 0x2C | 0x80 = 0xAC; 300 >> 7 = 2.
        let data = [0xACu8, 0x02];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 300);
        assert_eq!(pos, 2);
    }

    #[test]
    fn vint_three_bytes_16384() {
        // 16384 = 0x4000. Bytes: 0x80 (0 << 0 | continue), 0x80 (0 << 7 | continue), 0x01.
        let data = [0x80u8, 0x80, 0x01];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 16384);
        assert_eq!(pos, 3);
    }

    #[test]
    fn vint_zero() {
        let data = [0x00u8];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 0);
    }

    #[test]
    fn vint_truncated_returns_error() {
        let data: &[u8] = &[]; // empty
        let mut pos = 0;
        assert!(read_vint(data, &mut pos).is_err());
    }

    #[test]
    fn vint_truncated_mid_sequence() {
        // High bit set but no continuation byte follows.
        let data = [0x80u8];
        let mut pos = 0;
        assert!(read_vint(&data, &mut pos).is_err());
    }

    #[test]
    fn vint_pos_advances_correctly_in_stream() {
        // Two consecutive vints in a buffer.
        let data = [0x05u8, 0x7F, 0x80, 0x01];
        let mut pos = 0;
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 5);
        assert_eq!(pos, 1);
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 127);
        assert_eq!(pos, 2);
        assert_eq!(read_vint(&data, &mut pos).unwrap(), 128);
        assert_eq!(pos, 4);
    }

    // ── parse_block_header ────────────────────────────────────────────────────

    /// Build a minimal end-of-archive block (type 5, flags 0, no extra/data areas).
    ///
    /// Layout: CRC(4) + header_size_vint(1=0x02) + block_type_vint(1=0x05) + flags_vint(1=0x00)
    fn make_end_block() -> Vec<u8> {
        // header_size = 2 (covers block_type + flags vints)
        // CRC region = data[4..7] = [0x02, 0x05, 0x00]
        let crc_region = [0x02u8, 0x05, 0x00];
        let crc = crc32fast::hash(&crc_region);
        let mut block = Vec::new();
        block.extend_from_slice(&crc.to_le_bytes());
        block.extend_from_slice(&crc_region);
        block
    }

    #[test]
    fn parse_end_of_archive_block() {
        let block = make_end_block();
        let hdr = parse_block_header(&block, 0).unwrap();
        assert_eq!(hdr.block_type, 5);
        assert_eq!(hdr.flags, 0);
        assert_eq!(hdr.extra_area_size, 0);
        assert_eq!(hdr.data_size, 0);
        assert_eq!(hdr.total_size, 7);
        assert_eq!(hdr.header_size, 2);
    }

    #[test]
    fn parse_block_header_crc_mismatch_returns_error() {
        let mut block = make_end_block();
        // Corrupt the CRC field.
        block[0] ^= 0xFF;
        let result = parse_block_header(&block, 0);
        assert!(
            matches!(result, Err(CoreError::CorruptHeader { .. })),
            "expected CorruptHeader, got: {result:?}"
        );
    }

    #[test]
    fn parse_block_header_too_short_returns_error() {
        let data = [0u8; 3];
        assert!(parse_block_header(&data, 0).is_err());
    }

    #[test]
    fn parse_block_header_archive_offset_stored() {
        let block = make_end_block();
        let hdr = parse_block_header(&block, 42).unwrap();
        assert_eq!(hdr.archive_offset, 42);
    }

    /// Build a minimal file header block (type 2) with Store compression.
    ///
    /// Entry: name="test", unpacked_size=4, mtime and CRC32 present, no extra area.
    fn make_store_file_block() -> Vec<u8> {
        // Type-specific body:
        //   file_flags: 0x06 (mtime + crc32 present, no dir, size known)
        //   unpacked_size: 4
        //   attributes: 0x20
        //   mtime: 1_700_000_000u32 LE
        //   data_crc32: 0xAABBCCDD LE
        //   compression_info: 0x00 (store, not solid, version 0)
        //   host_os: 0x01 (Unix)
        //   name_len: 4
        //   name: "test"
        let mtime: u32 = 1_700_000_000;
        let crc: u32 = 0xAABBCCDD;
        let data_size: u64 = 4; // compressed == uncompressed for Store

        let mut body = Vec::new();
        body.push(0x06u8);                          // file_flags
        body.push(0x04);                            // unpacked_size = 4
        body.push(0x20);                            // attributes
        body.extend_from_slice(&mtime.to_le_bytes()); // mtime (4 bytes)
        body.extend_from_slice(&crc.to_le_bytes());   // data_crc32 (4 bytes)
        body.push(0x00);                            // compression_info (store)
        body.push(0x01);                            // host_os (Unix)
        body.push(0x04);                            // name_len = 4
        body.extend_from_slice(b"test");            // name

        // block_type = 2, flags = 0x0002 (data area present), data_area_size = 4
        // header_size = block_type(1) + flags(1) + data_area_size_vint(1) + body.len()
        let header_size = 1 + 1 + 1 + body.len();

        // Build the CRC region: header_size_vint + block_type + flags + data_area_size + body
        let mut crc_region = Vec::new();
        crc_region.push(header_size as u8); // header_size vint (fits in 1 byte here)
        crc_region.push(0x02);              // block_type = 2
        crc_region.push(0x02);              // flags = 0x0002 (data area flag)
        crc_region.push(data_size as u8);   // data_area_size vint = 4
        crc_region.extend_from_slice(&body);

        let header_crc = crc32fast::hash(&crc_region);

        let mut block = Vec::new();
        block.extend_from_slice(&header_crc.to_le_bytes());
        block.extend_from_slice(&crc_region);
        block
    }

    #[test]
    fn parse_file_block_header_fields() {
        let block = make_store_file_block();
        let raw = parse_block_header(&block, 100).unwrap();
        assert_eq!(raw.block_type, 2);
        assert_eq!(raw.flags, 0x0002);       // data area flag
        assert_eq!(raw.data_size, 4);        // compressed data size
        assert_eq!(raw.extra_area_size, 0);
        assert_eq!(raw.archive_offset, 100);
    }

    #[test]
    fn parse_file_header_name_and_sizes() {
        let block = make_store_file_block();
        let raw = parse_block_header(&block, 0).unwrap();
        let file_hdr = parse_file_header(&block, raw).unwrap();

        assert_eq!(file_hdr.name, "test");
        assert_eq!(file_hdr.unpacked_size, 4);
        assert!(!file_hdr.is_directory);
        assert!(!file_hdr.compression.solid);
        assert_eq!(file_hdr.compression.method, 0); // Store
        assert_eq!(file_hdr.mtime, Some(1_700_000_000));
        assert_eq!(file_hdr.data_crc32, Some(0xAABBCCDD));
        assert_eq!(file_hdr.host_os, 1); // Unix
    }

    #[test]
    fn parse_file_header_no_extra_area_yields_none() {
        let block = make_store_file_block();
        let raw = parse_block_header(&block, 0).unwrap();
        let file_hdr = parse_file_header(&block, raw).unwrap();
        assert!(file_hdr.encryption.is_none());
        assert!(file_hdr.blake2_hash.is_none());
        assert!(file_hdr.high_precision_time.is_none());
    }

    // ── parse_compression_info ────────────────────────────────────────────────

    #[test]
    fn compression_info_store() {
        // method=0, solid=false, version=0, dict_exponent=0
        let info = parse_compression_info(0x0000);
        assert_eq!(info.method, 0);
        assert!(!info.solid);
        assert_eq!(info.version, 0);
        assert_eq!(info.dict_size, 128 * 1024);
    }

    #[test]
    fn compression_info_compressed_method3_solid() {
        // version=0, solid=1(bit6), method=3(bits7-9), dict_exp=1(bits10-14)
        // raw = 0 | (1<<6) | (3<<7) | (1<<10) = 0x40 | 0x180 | 0x400 = 0x5C0
        let raw: u64 = (1 << 6) | (3 << 7) | (1 << 10);
        let info = parse_compression_info(raw);
        assert_eq!(info.method, 3);
        assert!(info.solid);
        assert_eq!(info.version, 0);
        assert_eq!(info.dict_size, 128 * 1024 * 2); // 128KB << 1
    }

    // ── RAR4 signature detection ──────────────────────────────────────────────

    #[test]
    fn rar4_signature_is_distinct_from_rar5() {
        assert_ne!(&RAR4_SIGNATURE[..], &RAR5_SIGNATURE[..7]);
    }

    #[test]
    fn rar5_signature_bytes() {
        assert_eq!(
            RAR5_SIGNATURE,
            [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00]
        );
        assert_eq!(
            RAR4_SIGNATURE,
            [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00]
        );
    }

    // ── parse_quick_open_data ─────────────────────────────────────────────────

    /// Helper: build a single QO cache entry.
    ///
    /// Structure layout:
    ///   structure_crc32 (4 bytes LE) — CRC of structure bytes
    ///   structure_size  (vint)
    ///   [structure start] flags (vint=0) + offset (vint) + data_size (vint) + data bytes
    fn build_qo_entry(relative_offset: u64, header_data: &[u8]) -> Vec<u8> {
        // Build the structure body (everything after structure_size vint)
        let mut structure = Vec::new();
        // flags vint = 0
        structure.push(0x00u8);
        // offset vint (we assume it fits in 1-2 bytes for tests)
        encode_vint(relative_offset, &mut structure);
        // data_size vint
        encode_vint(header_data.len() as u64, &mut structure);
        // data
        structure.extend_from_slice(header_data);

        let structure_crc = crc32fast::hash(&structure);
        let structure_size = structure.len() as u64;

        let mut entry = Vec::new();
        entry.extend_from_slice(&structure_crc.to_le_bytes());
        encode_vint(structure_size, &mut entry);
        entry.extend_from_slice(&structure);
        entry
    }

    fn encode_vint(mut value: u64, buf: &mut Vec<u8>) {
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

    #[test]
    fn qo_empty_input_yields_no_blocks() {
        let blocks = parse_quick_open_data(&[], 0);
        assert!(blocks.is_empty());
    }

    #[test]
    fn qo_single_valid_entry_parsed() {
        let fake_header = b"FAKEHEADER";
        let relative_offset = 100u64;
        let qo_block_offset = 500u64;

        let entry = build_qo_entry(relative_offset, fake_header);
        let blocks = parse_quick_open_data(&entry, qo_block_offset);

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].original_archive_offset,
            qo_block_offset + relative_offset
        );
        assert_eq!(blocks[0].data, fake_header);
    }

    #[test]
    fn qo_two_entries_both_parsed() {
        let data1 = b"HEADER_ONE";
        let data2 = b"HEADER_TWO_LONGER";
        let qo_block_offset = 1000u64;

        let mut buf = Vec::new();
        buf.extend(build_qo_entry(50, data1));
        buf.extend(build_qo_entry(200, data2));

        let blocks = parse_quick_open_data(&buf, qo_block_offset);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].original_archive_offset, qo_block_offset + 50);
        assert_eq!(blocks[0].data, data1);
        assert_eq!(blocks[1].original_archive_offset, qo_block_offset + 200);
        assert_eq!(blocks[1].data, data2);
    }

    #[test]
    fn qo_corrupt_crc_stops_parsing() {
        let data1 = b"GOOD_HEADER";
        let data2 = b"BAD_HEADER";
        let qo_block_offset = 0u64;

        let mut buf = Vec::new();
        buf.extend(build_qo_entry(10, data1));
        let mut bad_entry = build_qo_entry(20, data2);
        // Corrupt the CRC of the second entry.
        bad_entry[0] ^= 0xFF;
        buf.extend(bad_entry);

        let blocks = parse_quick_open_data(&buf, qo_block_offset);
        // Only the first valid entry should be returned.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data, data1);
    }

    #[test]
    fn qo_zero_size_structure_stops_parsing() {
        // CRC32 of empty input = 0x00000000. A structure_size of 0 with a matching
        // stored CRC would cause an infinite loop without the zero-size guard.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // CRC = hash([]) = 0
        buf.push(0x00u8); // structure_size vint = 0 — no structure bytes follow

        // A valid second entry that would be reached if we skipped instead of stopped.
        buf.extend(build_qo_entry(50, b"WOULD_SEE_IF_SKIPPED"));

        // Must not hang; must return 0 blocks (zero-size triggers break).
        let blocks = parse_quick_open_data(&buf, 0);
        assert_eq!(blocks.len(), 0);
    }

    #[test]
    fn qo_inner_data_overflow_skips_entry_and_continues() {
        // Build an entry whose structure has a valid CRC but whose data_size vint
        // claims more bytes than are available inside the structure.
        // Expected: that entry is skipped (not a hard stop), and the next valid
        // entry is still parsed.
        let structure = [0x00u8, 0x0A, 0x20]; // flags=0, offset=10, data_size=32 (> 0 remaining)
        let structure_crc = crc32fast::hash(&structure);

        let mut entry1 = Vec::new();
        entry1.extend_from_slice(&structure_crc.to_le_bytes());
        encode_vint(structure.len() as u64, &mut entry1);
        entry1.extend_from_slice(&structure);

        let fake_header = b"SECOND_ENTRY";
        let entry2 = build_qo_entry(200, fake_header);

        let mut buf = Vec::new();
        buf.extend_from_slice(&entry1);
        buf.extend_from_slice(&entry2);

        let blocks = parse_quick_open_data(&buf, 0);
        assert_eq!(blocks.len(), 1, "overflowed first entry must be skipped, not abort");
        assert_eq!(blocks[0].original_archive_offset, 200);
        assert_eq!(blocks[0].data, fake_header);
    }

    #[test]
    fn qo_truncated_input_does_not_panic() {
        let data = b"TRUNCATED";
        let entry = build_qo_entry(42, data);
        // Feed only the first half of the entry.
        let half = &entry[..entry.len() / 2];
        let blocks = parse_quick_open_data(half, 0);
        // May return 0 or partial results — must not panic.
        let _ = blocks;
    }
}
