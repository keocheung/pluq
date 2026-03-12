//! Low-level ZIP / ZIP64 structure parsing.
//!
//! All functions operate directly on a [`ReadAt`] source, issuing the
//! minimum number of range reads needed.
//!
//! `parse_central_directory` now returns two parallel `Vec`s:
//! - `Vec<ArchiveEntry>` — the public, format-agnostic metadata
//! - `Vec<ZipEntryLocation>` — private ZIP-specific location details
//!
//! Both slices are indexed identically: `entries[i]` corresponds to `locations[i]`.

use std::time::{Duration, SystemTime};

use crate::codec::CompressionMethod;
use crate::{ArchiveEntry, CoreError, ReadAt};

// ── Filename decoding ─────────────────────────────────────────────────────

/// Decode a ZIP entry filename from raw bytes, respecting the UTF-8 flag.
///
/// - GP bit 11 set → UTF-8 (modern tools always set this)
/// - GP bit 11 clear → legacy CP437 / Windows-1252 (ZIP's original codepage)
///
/// Using `from_utf8_lossy` for the legacy branch silently corrupts accented
/// characters (é, å, 한글). We use `encoding_rs::WINDOWS_1252` instead,
/// which is a strict superset of CP437 for the printable range.
pub(crate) fn decode_zip_filename(bytes: &[u8], general_purpose_flags: u16) -> String {
    if general_purpose_flags & (1 << 11) != 0 {
        // Modern, explicit UTF-8.
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        // Legacy codepage — decode as Windows-1252 (superset of CP437).
        let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
        decoded.into_owned()
    }
}

// ── ZIP magic numbers ─────────────────────────────────────────────────────
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

// ── ZIP compression methods ───────────────────────────────────────────────
/// File is stored without compression.
pub const METHOD_STORED: u16 = 0;
/// File is compressed with Deflate.
pub const METHOD_DEFLATE: u16 = 8;

// ── ZIP general-purpose bit flags ─────────────────────────────────────────
/// Bit 0: file is encrypted.
const GP_FLAG_ENCRYPTED: u16 = 1 << 0;

// ── Private location metadata (parallel to ArchiveEntry) ─────────────────

/// ZIP-specific location details for a single entry.
///
/// Never exposed outside `archive-core`. The format parser holds
/// `entries: Vec<ArchiveEntry>` + `locations: Vec<ZipEntryLocation>` as
/// parallel arrays — same index maps an entry to its location.
pub(crate) struct ZipEntryLocation {
    /// Byte offset of the Local File Header in the archive.
    pub(crate) offset: u64,
    /// Raw ZIP compression method ID (0 = stored, 8 = deflate, 99 = AES, …).
    pub(crate) method_id: u16,
    /// CRC-32 of the uncompressed data, as stored in the Central Directory.
    /// For AE-2 entries this is 0; rely on HMAC instead.
    pub(crate) crc32: u32,
    /// Populated when method_id == 99 (WinZip AES marker).
    pub(crate) aes_info: Option<AesExtraInfo>,
}

/// Parsed WinZip AES extra field (tag 0x9901).
#[derive(Debug, Clone)]
pub(crate) struct AesExtraInfo {
    /// AE-1 (0x0001) or AE-2 (0x0002).
    pub(crate) vendor_version: u16,
    /// Key strength: 1 = AES-128, 2 = AES-192, 3 = AES-256.
    pub(crate) strength: u8,
    /// The real compression method (Deflate, Store, …) hidden behind the AES marker.
    pub(crate) actual_method_id: u16,
}

// ── Helper: read little-endian integers from a byte slice ─────────────────

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

// ── EOCD ──────────────────────────────────────────────────────────────────

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
pub async fn find_eocd(reader: &impl ReadAt) -> Result<Eocd, CoreError> {
    let file_size = reader.size().await.map_err(CoreError::Io)?;

    if file_size < EOCD_MIN_SIZE as u64 {
        return Err(CoreError::InvalidFormat {
            message: "file too small to be a valid ZIP".into(),
        });
    }

    // Read the tail of the file (up to 65,557 bytes) to find the EOCD.
    let search_len = file_size.min(EOCD_SEARCH_LEN);
    let search_offset = file_size - search_len;
    let tail = reader
        .read_range(search_offset, search_len)
        .await
        .map_err(CoreError::Io)?;

    // Scan backwards for the standard EOCD signature.
    let tail_len = tail.len();
    let mut eocd_pos = None;
    for i in (0..=tail_len.saturating_sub(EOCD_MIN_SIZE)).rev() {
        if read_u32_le(&tail, i) == EOCD_SIGNATURE {
            eocd_pos = Some(i);
            break;
        }
    }

    let pos = eocd_pos.ok_or_else(|| CoreError::InvalidFormat {
        message: "EOCD signature not found — not a valid ZIP file".into(),
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

    // ── ZIP64 path ────────────────────────────────────────────────────────
    // The ZIP64 EOCD Locator sits immediately before the standard EOCD.
    if pos < EOCD64_LOCATOR_SIZE {
        return Err(CoreError::InvalidFormat {
            message: "ZIP64 EOCD Locator not found in scanned tail".into(),
        });
    }

    let locator_pos = pos - EOCD64_LOCATOR_SIZE;
    if read_u32_le(&tail, locator_pos) != EOCD64_LOCATOR_SIGNATURE {
        return Err(CoreError::InvalidFormat {
            message: "ZIP64 EOCD Locator signature missing".into(),
        });
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
                .await
                .map_err(CoreError::Io)?;

            if read_u32_le(&rec, 0) != EOCD64_RECORD_SIGNATURE {
                return Err(CoreError::InvalidFormat {
                    message: "ZIP64 EOCD Record signature mismatch".into(),
                });
            }

            return Ok(Eocd {
                entry_count: read_u64_le(&rec, 32),
                cd_size: read_u64_le(&rec, 40),
                cd_offset: read_u64_le(&rec, 48),
            });
        }
    };

    if read_u32_le(&tail, eocd64_pos) != EOCD64_RECORD_SIGNATURE {
        return Err(CoreError::InvalidFormat {
            message: "ZIP64 EOCD Record signature mismatch".into(),
        });
    }

    Ok(Eocd {
        entry_count: read_u64_le(&tail, eocd64_pos + 32),
        cd_size: read_u64_le(&tail, eocd64_pos + 40),
        cd_offset: read_u64_le(&tail, eocd64_pos + 48),
    })
}

// ── Central Directory ──────────────────────────────────────────────────────

/// Parse the Central Directory and return parallel lists of archive entries
/// and their ZIP-specific location details.
///
/// The two returned `Vec`s are indexed identically: `entries[i]` is located
/// at `locations[i]`.
///
/// Handles ZIP64 extra fields inside each CD entry to get real u64 sizes
/// and offsets when the standard u32 fields hold sentinel values.
pub async fn parse_central_directory(
    reader: &impl ReadAt,
    eocd: &Eocd,
) -> Result<(Vec<ArchiveEntry>, Vec<ZipEntryLocation>), CoreError> {
    let cd_bytes = reader
        .read_range(eocd.cd_offset, eocd.cd_size)
        .await
        .map_err(CoreError::Io)?;

    let capacity = eocd.entry_count.min(65536) as usize;
    let mut entries: Vec<ArchiveEntry> = Vec::with_capacity(capacity);
    let mut locations: Vec<ZipEntryLocation> = Vec::with_capacity(capacity);
    let mut cursor = 0usize;

    for index in 0..eocd.entry_count as usize {
        if cursor + 46 > cd_bytes.len() {
            return Err(CoreError::InvalidFormat {
                message: "unexpected end of Central Directory".into(),
            });
        }

        let sig = read_u32_le(&cd_bytes, cursor);
        if sig != CD_ENTRY_SIGNATURE {
            return Err(CoreError::InvalidFormat {
                message: format!("invalid CD entry signature: {sig:#010x}"),
            });
        }

        // ── CD fixed fields ───────────────────────────────────────────────
        let general_purpose_flags = read_u16_le(&cd_bytes, cursor + 8);
        let method_id = read_u16_le(&cd_bytes, cursor + 10);
        let last_mod_time = read_u16_le(&cd_bytes, cursor + 12);
        let last_mod_date = read_u16_le(&cd_bytes, cursor + 14);
        let crc32 = read_u32_le(&cd_bytes, cursor + 16);
        let mut compressed_size = read_u32_le(&cd_bytes, cursor + 20) as u64;
        let mut uncompressed_size = read_u32_le(&cd_bytes, cursor + 24) as u64;
        let name_len = read_u16_le(&cd_bytes, cursor + 28) as usize;
        let extra_len = read_u16_le(&cd_bytes, cursor + 30) as usize;
        let comment_len = read_u16_le(&cd_bytes, cursor + 32) as usize;
        let mut local_header_offset = read_u32_le(&cd_bytes, cursor + 42) as u64;

        let name_start = cursor + 46;
        let name_end = name_start + name_len;

        if name_end > cd_bytes.len() {
            return Err(CoreError::InvalidFormat {
                message: "file name extends beyond Central Directory".into(),
            });
        }

        // ── Extra field parsing (ZIP64 + AES) ─────────────────────────────
        let extra_start = name_end;
        let extra_end = extra_start + extra_len;
        let mut aes_info: Option<AesExtraInfo> = None;

        if extra_end <= cd_bytes.len() {
            let extra_slice = &cd_bytes[extra_start..extra_end];

            let need_zip64 = compressed_size == ZIP64_MAGIC32 as u64
                || uncompressed_size == ZIP64_MAGIC32 as u64
                || local_header_offset == ZIP64_MAGIC32 as u64;

            if need_zip64 {
                parse_zip64_extra(
                    extra_slice,
                    &mut uncompressed_size,
                    &mut compressed_size,
                    &mut local_header_offset,
                );
            }

            aes_info = parse_aes_extra(extra_slice);
        }

        // ── Derive entry fields ────────────────────────────────────────────
        // GP bit 0 = encrypted. AES entries (method 99) are always encrypted
        // even if some tools don't set GP bit 0.
        let is_encrypted =
            (general_purpose_flags & GP_FLAG_ENCRYPTED != 0) || method_id == 99;

        let name = decode_zip_filename(&cd_bytes[name_start..name_end], general_purpose_flags);
        let is_directory = name.ends_with('/');

        let modified_at = dos_datetime_to_system_time(last_mod_date, last_mod_time);

        // For AES entries the real compression method lives in the extra field.
        let compression_method = if let Some(ref aes) = aes_info {
            zip_method_to_codec(aes.actual_method_id)
        } else {
            zip_method_to_codec(method_id)
        };

        entries.push(ArchiveEntry {
            name,
            compressed_size,
            uncompressed_size,
            is_directory,
            is_encrypted,
            modified_at,
            index,
            compression_method,
        });

        locations.push(ZipEntryLocation {
            offset: local_header_offset,
            method_id,
            crc32,
            aes_info,
        });

        cursor = name_end + extra_len + comment_len;
    }

    Ok((entries, locations))
}

/// Translate a raw ZIP compression method ID to the shared [`CompressionMethod`] enum.
///
/// Returns `None` for unknown method IDs — defers errors to extraction time
/// per the architecture spec ("Defer errors to extraction, not parse time").
pub(crate) fn zip_method_to_codec(id: u16) -> Option<CompressionMethod> {
    match id {
        METHOD_STORED => Some(CompressionMethod::Store),
        METHOD_DEFLATE => Some(CompressionMethod::Deflate),
        12 => Some(CompressionMethod::BZip2),
        14 => Some(CompressionMethod::Lzma),
        93 => Some(CompressionMethod::Zstd),
        _ => None,
    }
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

/// Scan extra fields for the WinZip AES tag (0x9901) and parse it.
///
/// Returns `None` if the tag is absent or malformed.
fn parse_aes_extra(extra: &[u8]) -> Option<AesExtraInfo> {
    let mut i = 0;
    while i + 4 <= extra.len() {
        let tag = read_u16_le(extra, i);
        let size = read_u16_le(extra, i + 2) as usize;
        i += 4;

        if tag == 0x9901 && size >= 7 && i + size <= extra.len() {
            // Layout: vendor_version(2) + vendor_id(2) + strength(1) + actual_method(2)
            let vendor_version = read_u16_le(extra, i);
            // vendor_id bytes at i+2..i+4 should be b"AE" — we don't enforce this
            let strength = extra[i + 4];
            let actual_method_id = read_u16_le(extra, i + 5);
            return Some(AesExtraInfo { vendor_version, strength, actual_method_id });
        }

        i += size;
    }
    None
}

/// Convert a ZIP DOS-format date+time pair to `SystemTime`.
///
/// DOS date: bits 15-9 = year offset from 1980, 8-5 = month (1-12), 4-0 = day (1-31)
/// DOS time: bits 15-11 = hour, 10-5 = minute, 4-0 = 2-second count
///
/// Returns `None` if the date is zero (absent) or invalid.
fn dos_datetime_to_system_time(date: u16, time: u16) -> Option<SystemTime> {
    if date == 0 {
        return None;
    }

    let year = ((date >> 9) & 0x7F) as u32 + 1980;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as u64;
    let minute = ((time >> 5) & 0x3F) as u64;
    let second = ((time & 0x1F) as u64) * 2;

    if month == 0 || month > 12 || day == 0 || day > 31 {
        return None;
    }

    // Compute days since Unix epoch (1970-01-01).
    // We use a simple Gregorian calendar calculation.
    let days_since_epoch = days_since_unix_epoch(year, month, day)?;

    let secs = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second;

    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Compute the number of days between 1970-01-01 and the given Gregorian date.
/// Returns `None` for dates before 1970 (shouldn't happen in ZIP's 1980+ range).
fn days_since_unix_epoch(year: u32, month: u32, day: u32) -> Option<u64> {
    // Days in each month (non-leap year)
    const DAYS_IN_MONTH: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    if year < 1970 {
        return None;
    }

    let is_leap = |y: u32| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));

    let mut days: u64 = 0;

    // Full years from 1970 to year-1
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    // Full months from January to month-1 in the target year
    for m in 1..month {
        let m_idx = (m - 1) as usize;
        days += DAYS_IN_MONTH[m_idx] as u64;
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }

    // Days in the target month (day is 1-based)
    days += (day - 1) as u64;

    Some(days)
}

// ── Local File Header + Data ───────────────────────────────────────────────

/// Parse the Local File Header and return the byte offset of the compressed
/// data that immediately follows it.
///
/// This is the "first half" of extraction — consumers call this then use
/// `read_stream(data_offset, compressed_size)` to get a streaming reader.
pub async fn resolve_data_offset(
    reader: &impl ReadAt,
    location: &ZipEntryLocation,
) -> Result<u64, crate::io::ReadError> {
    // Read the fixed-size part of the Local File Header (30 bytes).
    let lfh = reader
        .read_range(location.offset, 30)
        .await?;

    let sig = read_u32_le(&lfh, 0);
    if sig != LOCAL_FILE_HEADER_SIGNATURE {
        // We can't use CoreError here — this returns ReadError.
        // Callers that want CoreError can map this.
        // Use a sentinel Transport error so the caller can distinguish it.
        return Err(crate::io::ReadError::Transport(
            format!("invalid Local File Header signature: {sig:#010x}").into(),
        ));
    }

    let name_len = read_u16_le(&lfh, 26) as u64;
    let extra_len = read_u16_le(&lfh, 28) as u64;

    // Compressed data starts right after the LFH + name + extra fields.
    Ok(location.offset + 30 + name_len + extra_len)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── decode_zip_filename ────────────────────────────────────────────────

    /// GP bit 11 set → treat as UTF-8.
    #[test]
    fn decode_utf8_filename_when_bit11_set() {
        let name = "café.txt";
        let bytes = name.as_bytes();
        let flags: u16 = 1 << 11; // UTF-8 bit
        assert_eq!(decode_zip_filename(bytes, flags), "café.txt");
    }

    /// GP bit 11 clear → decode as Windows-1252.
    /// 0xE9 is 'é' in Windows-1252 / Latin-1. `from_utf8_lossy` would turn
    /// it into the replacement character (U+FFFD) — we must NOT do that.
    #[test]
    fn decode_windows1252_filename_when_bit11_clear() {
        // "café" encoded in Windows-1252: c=0x63, a=0x61, f=0x66, é=0xE9
        let bytes: &[u8] = b"caf\xE9.txt";
        let flags: u16 = 0; // bit 11 clear → legacy codepage
        let result = decode_zip_filename(bytes, flags);
        assert_eq!(result, "café.txt");
    }

    /// Sanity-check that the two branches differ for non-ASCII bytes.
    /// `from_utf8_lossy` on 0xE9 alone yields the replacement char \u{FFFD};
    /// Windows-1252 decoding yields 'é'. They must not be equal.
    #[test]
    fn utf8_lossy_and_windows1252_are_distinct_for_non_ascii() {
        let bytes: &[u8] = &[0xE9]; // 'é' in Win-1252, invalid lone byte in UTF-8
        let utf8_lossy = String::from_utf8_lossy(bytes).into_owned();
        let win1252 = decode_zip_filename(bytes, 0 /* bit 11 clear */);
        assert_ne!(
            utf8_lossy, win1252,
            "Windows-1252 decode must differ from UTF-8 lossy for 0xE9"
        );
        assert_eq!(win1252, "é");
    }
}
