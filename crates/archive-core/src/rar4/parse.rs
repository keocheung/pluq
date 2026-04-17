//! Low-level RAR4 block parsing.
//!
//! RAR4 headers use fixed-size fields and CRC-16 checksums.
//! No variable-length integers — simpler than RAR5.

use bytes::Bytes;

use crate::CoreError;

// ── Block type bytes ────────────────────────────────────────────────────────

pub(crate) const BLOCK_MAIN: u8 = 0x73;
pub(crate) const BLOCK_FILE: u8 = 0x74;
pub(crate) const BLOCK_END: u8 = 0x7B;

// ── File header flag bits (HEAD_FLAGS for type 0x74) ───────────────────────

const FLAG_ENCRYPTED: u16 = 0x0004;
const FLAG_LARGE: u16 = 0x0100; // HIGH_PACK_SIZE + HIGH_UNP_SIZE present
const FLAG_UNICODE: u16 = 0x0200;
const FLAG_LONG_BLOCK: u16 = 0x8000; // ADD_SIZE field present

// ── Main header flag bits (HEAD_FLAGS for type 0x73) ───────────────────────

const MAIN_FLAG_VOLUME: u16 = 0x0001;
const MAIN_FLAG_SOLID: u16 = 0x0008;

// ── Common block header ────────────────────────────────────────────────────

/// Common 7-byte RAR4 block header (present in every block type).
#[allow(dead_code)]
pub(crate) struct Rar4BlockHeader {
    pub head_crc: u16,
    pub head_type: u8,
    pub head_flags: u16,
    /// Total header size in bytes (includes HEAD_CRC through end of header).
    pub head_size: u16,
    /// Data area size following the header (0 if `FLAG_LONG_BLOCK` not set).
    pub add_size: u32,
    /// Absolute position of this block in the archive.
    pub archive_offset: u64,
    /// `head_size as u64 + add_size as u64`
    pub total_size: u64,
}

/// Parse the common 7-byte block header from `data` at `archive_offset`.
///
/// `data` must contain at least 7 bytes; 11 bytes if `FLAG_LONG_BLOCK` may be set.
pub(crate) fn parse_block_header(
    data: &[u8],
    archive_offset: u64,
) -> Result<Rar4BlockHeader, CoreError> {
    if data.len() < 7 {
        return Err(CoreError::InvalidFormat {
            message: format!(
                "RAR4 block header too short at offset {archive_offset}: {} bytes",
                data.len()
            ),
        });
    }

    let head_crc = u16::from_le_bytes([data[0], data[1]]);
    let head_type = data[2];
    let head_flags = u16::from_le_bytes([data[3], data[4]]);
    let head_size = u16::from_le_bytes([data[5], data[6]]);

    let add_size = if head_flags & FLAG_LONG_BLOCK != 0 {
        if data.len() < 11 {
            return Err(CoreError::InvalidFormat {
                message: format!(
                    "RAR4 block at {archive_offset}: FLAG_LONG_BLOCK set but only {} bytes available",
                    data.len()
                ),
            });
        }
        u32::from_le_bytes([data[7], data[8], data[9], data[10]])
    } else {
        0
    };

    let total_size = head_size as u64 + add_size as u64;

    Ok(Rar4BlockHeader {
        head_crc,
        head_type,
        head_flags,
        head_size,
        add_size,
        archive_offset,
        total_size,
    })
}

// ── File header ────────────────────────────────────────────────────────────

/// Parsed RAR4 file block (type `0x74`).
pub(crate) struct Rar4FileHeader {
    pub raw: Rar4BlockHeader,
    /// Compressed size (low u32 + optional high u32 from `FLAG_LARGE`).
    pub pack_size: u64,
    /// Uncompressed size (low u32 + optional high u32 from `FLAG_LARGE`).
    pub unp_size: u64,
    /// CRC32 of uncompressed data.
    pub file_crc: u32,
    /// DOS timestamp packed as `(date << 16) | time`.
    pub ftime: u32,
    /// Compression method byte: 0x30 = Store, 0x31-0x35 = RAR compression levels.
    pub method: u8,
    /// File path with directory separators normalised to `/`.
    pub name: String,
    pub is_directory: bool,
    pub is_encrypted: bool,
}

/// Parse a RAR4 file block header.
///
/// `data` must cover the full header (`block.head_size` bytes).
pub(crate) fn parse_file_header(
    data: &[u8],
    block: Rar4BlockHeader,
) -> Result<Rar4FileHeader, CoreError> {
    let head_size = block.head_size as usize;
    if data.len() < head_size || head_size < 32 {
        return Err(CoreError::InvalidFormat {
            message: format!(
                "RAR4 file header at {}: too short ({} bytes, head_size={})",
                block.archive_offset,
                data.len(),
                head_size
            ),
        });
    }

    // Offsets within the block (from byte 0 of the block):
    // 0:  HEAD_CRC   (u16)
    // 2:  HEAD_TYPE  (u8)
    // 3:  HEAD_FLAGS (u16)
    // 5:  HEAD_SIZE  (u16)
    // 7:  PACK_SIZE  (u32 LE)  — compressed size low
    // 11: UNP_SIZE   (u32 LE)  — uncompressed size low
    // 15: HOST_OS    (u8)
    // 16: FILE_CRC   (u32 LE)
    // 20: FTIME      (u32 LE)
    // 24: UNP_VER    (u8)
    // 25: METHOD     (u8)
    // 26: NAME_SIZE  (u16 LE)
    // 28: ATTR       (u32 LE)
    // 32: [HIGH_PACK_SIZE (u32) + HIGH_UNP_SIZE (u32) if FLAG_LARGE]
    // then filename

    let pack_size_low = u32::from_le_bytes([data[7], data[8], data[9], data[10]]) as u64;
    let unp_size_low = u32::from_le_bytes([data[11], data[12], data[13], data[14]]) as u64;
    let host_os = data[15];
    let file_crc = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    let ftime = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
    let method = data[25];
    let name_size = u16::from_le_bytes([data[26], data[27]]) as usize;
    let attr = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);

    let (pack_size, unp_size, name_offset) = if block.head_flags & FLAG_LARGE != 0 {
        if head_size < 40 {
            return Err(CoreError::InvalidFormat {
                message: format!(
                    "RAR4 file header at {}: FLAG_LARGE set but head_size={head_size} < 40",
                    block.archive_offset
                ),
            });
        }
        let high_pack = u32::from_le_bytes([data[32], data[33], data[34], data[35]]) as u64;
        let high_unp = u32::from_le_bytes([data[36], data[37], data[38], data[39]]) as u64;
        let ps = pack_size_low | (high_pack << 32);
        let us = unp_size_low | (high_unp << 32);
        (ps, us, 40)
    } else {
        (pack_size_low, unp_size_low, 32)
    };

    let name_end = name_offset + name_size;
    if name_end > head_size {
        return Err(CoreError::InvalidFormat {
            message: format!(
                "RAR4 file header at {}: name_size={name_size} exceeds head_size={head_size}",
                block.archive_offset
            ),
        });
    }

    let name_bytes = &data[name_offset..name_end];
    let name = if block.head_flags & FLAG_UNICODE != 0 {
        // RAR4 Unicode: ASCII base up to null terminator, followed by a delta-encoded
        // Unicode suffix. For now, use the ASCII portion only.
        let ascii_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        String::from_utf8_lossy(&name_bytes[..ascii_end]).into_owned()
    } else {
        String::from_utf8_lossy(name_bytes).into_owned()
    };

    // Normalise backslashes (Windows paths) to forward slashes.
    let name = name.replace('\\', "/");

    // Detect directory: Windows ATTR bit 4 (0x10) = directory, or name ends with '/'.
    // HOST_OS 0x00 = MS-DOS, 0x02 = Windows — both use ATTR.
    let is_directory = name.ends_with('/') || (host_os <= 2 && attr & 0x10 != 0);

    let is_encrypted = block.head_flags & FLAG_ENCRYPTED != 0;

    Ok(Rar4FileHeader {
        raw: block,
        pack_size,
        unp_size,
        file_crc,
        ftime,
        method,
        name,
        is_directory,
        is_encrypted,
    })
}

// ── Main archive header ────────────────────────────────────────────────────

/// Parsed RAR4 main archive header (type `0x73`).
pub(crate) struct Rar4MainHeader {
    pub raw: Rar4BlockHeader,
    pub is_solid: bool,
    #[allow(dead_code)]
    pub is_volume: bool,
}

/// Parse a RAR4 main archive header.
///
/// `data` must cover at least 13 bytes (7-byte common header + 4 reserved bytes).
pub(crate) fn parse_main_header(
    data: &[u8],
    archive_offset: u64,
) -> Result<Rar4MainHeader, CoreError> {
    let block = parse_block_header(data, archive_offset)?;

    if block.head_type != BLOCK_MAIN {
        return Err(CoreError::InvalidFormat {
            message: format!(
                "expected RAR4 main header (0x73) at {archive_offset}, got 0x{:02X}",
                block.head_type
            ),
        });
    }

    let is_solid = block.head_flags & MAIN_FLAG_SOLID != 0;
    let is_volume = block.head_flags & MAIN_FLAG_VOLUME != 0;

    Ok(Rar4MainHeader {
        raw: block,
        is_solid,
        is_volume,
    })
}

// ── End-of-archive block builder ───────────────────────────────────────────

/// Build a minimal RAR4 end-of-archive block (type `0x7B`), 7 bytes.
///
/// Layout: HEAD_CRC(2) + HEAD_TYPE(1) + HEAD_FLAGS(2) + HEAD_SIZE(2)
pub(crate) fn build_rar4_end_block() -> Bytes {
    // head_type = 0x7B, head_flags = 0x4000, head_size = 7
    let head_type: u8 = BLOCK_END;
    let head_flags: u16 = 0x4000;
    let head_size: u16 = 7;

    let body: [u8; 5] = [
        head_type,
        head_flags as u8,
        (head_flags >> 8) as u8,
        head_size as u8,
        (head_size >> 8) as u8,
    ];

    let crc = crc16_arc(&body);
    let mut block = [0u8; 7];
    block[0] = crc as u8;
    block[1] = (crc >> 8) as u8;
    block[2..].copy_from_slice(&body);

    Bytes::copy_from_slice(&block)
}

// ── CRC-16/ARC (polynomial 0xA001, reflected) ─────────────────────────────

/// CRC-16/ARC used in RAR4 block headers.
///
/// Polynomial 0xA001 (reflected form of 0x8005), initial value 0, no final XOR.
fn crc16_arc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}
