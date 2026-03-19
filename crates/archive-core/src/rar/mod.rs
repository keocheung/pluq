//! RAR format support.
//!
//! v1 scope: RAR5 non-solid archives (listing + extraction).
//! RAR4 archives are detected and rejected with a clear error.

pub(crate) mod parse;

use std::time::SystemTime;

use bytes::Bytes;

use crate::codec::CompressionMethod;
use crate::rar::parse::{
    QoCachedBlock, RAR4_SIGNATURE, RAR5_SIGNATURE, parse_block_header, parse_file_header,
    parse_main_header, parse_quick_open_data,
};
use crate::{ArchiveEntry, CoreError, ReadAt};

// ── Location data (private) ───────────────────────────────────────────────────

/// Private per-entry location data, parallel to `RarArchive::entries`.
///
/// Holds byte offsets and extraction metadata that are not part of the
/// public `ArchiveEntry` API.
// Fields are used in Steps 5-6 (store/compressed extraction + hash verification).
#[allow(dead_code)]
pub(crate) struct RarEntryLocation {
    /// Absolute offset of the file header block in the archive.
    pub header_offset: u64,
    /// Byte length of the file header block (header fields only, not data area).
    pub header_size: u64,
    /// Absolute offset of the compressed data in the archive.
    pub data_offset: u64,
    /// Compressed data size in bytes.
    pub data_size: u64,
    /// RAR5 compression method: 0 = Store, 1–5 = proprietary compression.
    pub method: u8,
    /// Compression dictionary size in bytes.
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
    // All pub(crate) fields below are used in Steps 5-6 (store/compressed extraction).
    // Allow dead_code until those steps are implemented.
    #[allow(dead_code)]
    pub(crate) reader: R,
    entries: Vec<ArchiveEntry>,
    #[allow(dead_code)]
    pub(crate) locations: Vec<RarEntryLocation>,
    /// RAR5 signature + main header bytes (for synthetic archive construction).
    #[allow(dead_code)]
    pub(crate) archive_preamble: Bytes,
    /// Minimal end-of-archive block (for synthetic archive construction).
    #[allow(dead_code)]
    pub(crate) end_block: Bytes,
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
    /// Returns [`CoreError::InvalidFormat`] if the RAR5 signature is absent.
    /// Returns [`CoreError::UnsupportedFormat`] if a RAR4 signature is detected.
    pub async fn open(reader: R) -> Result<Self, CoreError> {
        let size = reader.size().await?;

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

        // 4. Try Quick Open optimisation (avoids full sequential scan for QO-enabled archives).
        //    On failure we fall back to the sequential scan below.
        if let Some(locator) = &main_header.locator
            && let Some(qo_offset) = locator.quick_open_offset
        {
            let qo_abs = main_header.raw.archive_offset + qo_offset;
            if let Ok(Some((entries, locations))) =
                Self::try_quick_open(&reader, qo_abs, size).await
            {
                let end_block = build_end_of_archive_block();
                return Ok(Self {
                    reader,
                    entries,
                    locations,
                    archive_preamble,
                    end_block,
                });
            }
        }

        // 5. Sequential header scan — iterate blocks from end of main header.
        let mut entries: Vec<ArchiveEntry> = Vec::new();
        let mut locations: Vec<RarEntryLocation> = Vec::new();

        let mut offset = preamble_end as u64;
        // We reuse the initial chunk as the first buffer (buffer_start = 0).
        let mut buffer: Bytes = chunk;
        let mut buffer_start: u64 = 0;

        loop {
            if offset >= size {
                break;
            }

            // Refetch when we've exhausted the current buffer or are too close to
            // its end to reliably read even the minimum block preamble (5 bytes).
            let buf_remaining = (buffer_start + buffer.len() as u64).saturating_sub(offset);
            if buf_remaining < 64 {
                let fetch_size = (64 * 1024_u64).min(size - offset);
                buffer = reader.read_range(offset, fetch_size).await?;
                buffer_start = offset;
            }

            let local_offset = (offset - buffer_start) as usize;
            let block = parse_block_header(&buffer[local_offset..], offset)?;

            // Save total_size before potentially moving `block` into parse_file_header.
            let total_size = block.total_size;

            match block.block_type {
                2 => {
                    // File header: may need a larger slice if it spans the buffer end.
                    let header_end = local_offset + (total_size - block.data_size) as usize;
                    if header_end > buffer.len() {
                        // Header body extends past our current buffer — refetch.
                        let fetch_size = (64 * 1024_u64).min(size - offset);
                        buffer = reader.read_range(offset, fetch_size).await?;
                        buffer_start = offset;
                    }
                    let local_offset = (offset - buffer_start) as usize;
                    let file_hdr =
                        parse_file_header(&buffer[local_offset..], block)?;

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
                    // Solid entries are silently skipped — not included in listing.
                }
                3 => {} // Service header (QO, RR, CMT) — skip.
                5 => break, // End of archive.
                _ => {}     // Unknown block type — skip gracefully.
            }

            offset += total_size;
        }

        let end_block = build_end_of_archive_block();

        Ok(Self { reader, entries, locations, archive_preamble, end_block })
    }

    /// All entries in the archive (non-solid only; solid entries are omitted).
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
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

// ── Helper functions ──────────────────────────────────────────────────────────

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
    fn build_minimal_rar5(
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

        let mut crc_region = Vec::new();
        // Encode header_size as vint (1 byte, assuming < 128).
        crc_region.push(header_size as u8);
        crc_region.push(0x02u8); // block_type = 2
        crc_region.push(0x02u8); // flags = 0x0002 (data area flag)
        crc_region.push(data_len); // data_area_size vint
        crc_region.extend_from_slice(&body);

        let file_hdr_crc = crc32fast::hash(&crc_region);
        let mut file_block = Vec::new();
        file_block.extend_from_slice(&file_hdr_crc.to_le_bytes());
        file_block.extend_from_slice(&crc_region);
        file_block.extend_from_slice(data); // data area

        // ── Main archive header (type 1) ──────────────────────────────────────
        // file_flags = 0 (no volume, no comment, no solid)
        let archive_flags: u8 = 0;
        let mut main_body = Vec::new();
        main_body.push(archive_flags); // archive_flags vint

        let main_header_content = 1 + 1 + main_body.len(); // block_type + flags + body
        let main_header_size = main_header_content;

        let mut main_crc_region = Vec::new();
        main_crc_region.push(main_header_size as u8); // header_size vint
        main_crc_region.push(0x01u8);                 // block_type = 1 (main header)
        main_crc_region.push(0x00u8);                 // flags = 0 (no extra, no data)
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
        std::fs::write(tmp.path(), &bytes).unwrap();

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
}

