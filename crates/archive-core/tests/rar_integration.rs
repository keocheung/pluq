//! Integration tests for RAR5 support.
//!
//! Uses pre-built `.rar` fixtures from the `tests/fixtures/` directory.
//! `rar5_unicode_entry.rar` is copied from the `unrar` crate's own test data;
//! it is a RAR5 non-solid archive containing exactly one compressed entry.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use archive_core::{ArchiveEntry, CoreError, ProgressReporter, RarArchive, Source};

async fn open_rar(path: &str) -> RarArchive<Source> {
    RarArchive::open(Source::detect(path)).await.expect("open RAR")
}

async fn extract_bytes(
    archive: &RarArchive<Source>,
    entry: &ArchiveEntry,
    password: Option<&[u8]>,
) -> Result<Bytes, CoreError> {
    let mut buf = Vec::new();
    let progress = Arc::new(Mutex::new(()));
    archive.extract(entry, &mut buf, progress, password).await?;
    Ok(Bytes::from(buf))
}

fn fixture(name: &str) -> String {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// ── Format detection ──────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_format_detection() {
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    assert!(!archive.entries().is_empty(), "archive should have entries");
}

#[tokio::test]
async fn rar4_gives_unsupported_format_error() {
    // rar4_sample.rar carries the 7-byte RAR4 magic — must return UnsupportedFormat.
    let result = RarArchive::open(Source::detect(&fixture("rar4_sample.rar"))).await;
    assert!(
        matches!(result, Err(CoreError::UnsupportedFormat { .. })),
        "expected UnsupportedFormat for RAR4 archive, got an unexpected result"
    );
}

// ── Listing ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_listing_unicode_entry_archive() {
    // unicode-entry.rar contains exactly one file: "unicodefilename❤️.txt"
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 1, "should have exactly 1 entry");
    assert_eq!(entries[0].name, "unicodefilename❤️.txt");
    assert!(!entries[0].is_directory);
    assert!(!entries[0].is_encrypted);
}

// ── Extraction ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_extract_unicode_entry_content() {
    // unicodefilename❤️.txt contains "foobar\n" (verified against unrar test suite).
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    let entry = &archive.entries()[0];
    let data: Bytes = extract_bytes(&archive, entry, None)
    .await
    .expect("extract");
    assert_eq!(data.as_ref(), b"foobar\n");
}

// ── Extraction cost ───────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_extraction_cost_positive() {
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    let entry = &archive.entries()[0];
    let cost = archive.extraction_cost(entry);
    assert!(cost > 0, "extraction cost must be positive for a non-empty entry");
}

// ── Progress reporting ────────────────────────────────────────────────────────

struct RecordingReporter {
    started: Vec<(String, u64)>,
    bytes_reported: Vec<u64>,
    completed: Vec<(String, bool)>,
}
impl RecordingReporter {
    fn new() -> Self {
        Self {
            started: vec![],
            bytes_reported: vec![],
            completed: vec![],
        }
    }
}
impl ProgressReporter for RecordingReporter {
    fn on_entry_start(&mut self, name: &str, size: u64) {
        self.started.push((name.to_string(), size));
    }
    fn on_bytes_progress(&mut self, bytes: u64) {
        self.bytes_reported.push(bytes);
    }
    fn on_entry_complete(&mut self, name: &str, success: bool) {
        self.completed.push((name.to_string(), success));
    }
}

#[tokio::test]
async fn rar5_progress_reporting_on_extraction() {
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    let entry = &archive.entries()[0];
    let reporter = Arc::new(Mutex::new(RecordingReporter::new()));

    let mut buf = Vec::new();
    archive
        .extract(entry, &mut buf, reporter.clone(), None)
        .await
        .expect("extract");

    let r = reporter.lock().unwrap();
    assert_eq!(r.started.len(), 1, "on_entry_start must fire once");
    assert_eq!(r.started[0].0, "unicodefilename❤️.txt");
    assert!(!r.bytes_reported.is_empty(), "on_bytes_progress must fire");
    assert_eq!(r.completed.len(), 1, "on_entry_complete must fire once");
    assert!(r.completed[0].1, "on_entry_complete success must be true");
}

// ── Integrity test ────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_test_integrity_passes() {
    let archive = open_rar(&fixture("rar5_unicode_entry.rar")).await;
    let entry = &archive.entries()[0];
    archive
        .test(entry)
        .await
        .expect("test integrity check should pass");
}

// ── UTF-8 filename (store fixture) ────────────────────────────────────────────

/// `rar5_utf8.rar` holds one empty Store entry whose name contains an em-dash.
#[tokio::test]
async fn rar5_utf8_filename_listing() {
    let archive = open_rar(&fixture("rar5_utf8.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "foo\u{2014}bar"); // U+2014 EM DASH
    assert!(!entries[0].is_directory);
    assert!(!entries[0].is_encrypted);
    assert_eq!(entries[0].uncompressed_size, 0);
}

#[tokio::test]
async fn rar5_utf8_store_extraction_empty_file() {
    let archive = open_rar(&fixture("rar5_utf8.rar")).await;
    let entry = &archive.entries()[0];
    let data = extract_bytes(&archive, entry, None)
    .await
    .expect("extract");
    assert_eq!(data.as_ref(), b"");
}

// ── In-memory Store archive helpers ──────────────────────────────────────────

/// Build a minimal valid RAR5 archive containing one or more Store entries.
///
/// Constraints (for 1-byte vint encoding): `filename.len() < 128`, `data.len() < 128`.
fn build_rar5_store_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    const RAR5_SIG: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];

    // End-of-archive block: CRC(4) + [header_size=2, type=5, flags=0]
    let eoa_inner = [0x02u8, 0x05, 0x00];
    let eoa_crc = crc32fast::hash(&eoa_inner);
    let mut eoa = eoa_crc.to_le_bytes().to_vec();
    eoa.extend_from_slice(&eoa_inner);

    // Main archive header: CRC(4) + [header_size=3, type=1, flags=0, arch_flags=0]
    let main_crc_region = [0x03u8, 0x01, 0x00, 0x00];
    let main_crc = crc32fast::hash(&main_crc_region);
    let mut main_block = main_crc.to_le_bytes().to_vec();
    main_block.extend_from_slice(&main_crc_region);

    // File blocks
    let mut file_blocks: Vec<Vec<u8>> = Vec::new();
    for (filename, data) in entries {
        let name_bytes = filename.as_bytes();
        assert!(name_bytes.len() < 128);
        assert!(data.len() < 128);

        let data_crc = crc32fast::hash(data);
        let mtime: u32 = 1_700_000_000;

        // File-specific body
        let mut body = Vec::new();
        body.push(0x06u8);                               // file_flags: mtime + crc32
        body.push(data.len() as u8);                     // unpacked_size (1-byte vint)
        body.push(0x20u8);                               // attributes
        body.extend_from_slice(&mtime.to_le_bytes());    // mtime (4 bytes)
        body.extend_from_slice(&data_crc.to_le_bytes()); // data_crc32 (4 bytes)
        body.push(0x00u8);                               // compression_info: Store, not solid
        body.push(0x01u8);                               // host_os: Unix
        body.push(name_bytes.len() as u8);               // name_length (1-byte vint)
        body.extend_from_slice(name_bytes);              // name

        // header_content_len = block_type(1) + flags(1) + data_size(1) + body
        let header_content_len = 1 + 1 + 1 + body.len();
        assert!(header_content_len < 128);

        // CRC region = [header_size_vint, block_type, flags, data_size, body...]
        let mut crc_region = Vec::new();
        crc_region.push(header_content_len as u8); // header_size vint
        crc_region.push(0x02u8);                   // block_type = 2 (file)
        crc_region.push(0x02u8);                   // flags = 0x02 (data area present)
        crc_region.push(data.len() as u8);         // data_size vint
        crc_region.extend_from_slice(&body);

        let file_crc = crc32fast::hash(&crc_region);
        let mut block = file_crc.to_le_bytes().to_vec();
        block.extend_from_slice(&crc_region);
        block.extend_from_slice(data); // data area (raw bytes for Store)

        file_blocks.push(block);
    }

    let mut archive = RAR5_SIG.to_vec();
    archive.extend_from_slice(&main_block);
    for block in &file_blocks {
        archive.extend_from_slice(block);
    }
    archive.extend_from_slice(&eoa);
    archive
}

/// Write bytes to a temp file; returns the guard + path string.
fn write_tempfile(bytes: &[u8]) -> (tempfile::NamedTempFile, String) {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), bytes).unwrap();
    let path = f.path().to_str().unwrap().to_owned();
    (f, path)
}

// ── Multi-file Store listing ──────────────────────────────────────────────────

#[tokio::test]
async fn rar5_store_two_file_listing() {
    let file1_data = b"Hello from file one\n";
    let file2_data = b"Second file content\n";
    let bytes = build_rar5_store_archive(&[("file1.txt", file1_data), ("file2.txt", file2_data)]);
    let (_guard, path) = write_tempfile(&bytes);

    let archive = open_rar(&path).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
    assert_eq!(entries[0].uncompressed_size, file1_data.len() as u64);
    assert_eq!(entries[1].uncompressed_size, file2_data.len() as u64);
    assert!(!entries[0].is_directory);
    assert!(!entries[1].is_directory);
    assert!(!entries[0].is_encrypted);
}

// ── Multi-file Store extraction ───────────────────────────────────────────────

#[tokio::test]
async fn rar5_store_extraction_exact_content() {
    let file1_data = b"Hello from file one\n";
    let file2_data = b"Second file content\n";
    let bytes = build_rar5_store_archive(&[("file1.txt", file1_data), ("file2.txt", file2_data)]);
    let (_guard, path) = write_tempfile(&bytes);

    let archive = open_rar(&path).await;

    let data1 = extract_bytes(&archive, &archive.entries()[0], None)
    .await
    .expect("extract file1");
    assert_eq!(data1.as_ref(), file1_data as &[u8]);

    let data2 = extract_bytes(&archive, &archive.entries()[1], None)
    .await
    .expect("extract file2");
    assert_eq!(data2.as_ref(), file2_data as &[u8]);
}

// ── Real RAR CLI fixtures ─────────────────────────────────────────────────────
//
// `rar5_store.rar` and `rar5_compressed.rar` were created with RAR 7.20:
//   rar a -m0 -ma5 rar5_store.rar file1.txt file2.txt
//   rar a -m3 -ma5 rar5_compressed.rar file1.txt file2.txt
// file1.txt = "Hello from file one\n", file2.txt = "Second file content\n"

#[tokio::test]
async fn rar5_real_store_listing() {
    let archive = open_rar(&fixture("rar5_store.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
    assert_eq!(entries[0].uncompressed_size, 20);
    assert_eq!(entries[1].uncompressed_size, 20);
    assert!(!entries[0].is_encrypted);
}

#[tokio::test]
async fn rar5_real_store_extraction() {
    let archive = open_rar(&fixture("rar5_store.rar")).await;

    let data1 = extract_bytes(&archive, &archive.entries()[0], None)
    .await
    .expect("extract file1");
    assert_eq!(data1.as_ref(), b"Hello from file one\n");

    let data2 = extract_bytes(&archive, &archive.entries()[1], None)
    .await
    .expect("extract file2");
    assert_eq!(data2.as_ref(), b"Second file content\n");
}

#[tokio::test]
async fn rar5_real_compressed_listing() {
    let archive = open_rar(&fixture("rar5_compressed.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
    assert_eq!(entries[0].uncompressed_size, 20);
}

#[tokio::test]
async fn rar5_real_compressed_extraction() {
    let archive = open_rar(&fixture("rar5_compressed.rar")).await;

    let data1 = extract_bytes(&archive, &archive.entries()[0], None)
    .await
    .expect("extract file1");
    assert_eq!(data1.as_ref(), b"Hello from file one\n");

    let data2 = extract_bytes(&archive, &archive.entries()[1], None)
    .await
    .expect("extract file2");
    assert_eq!(data2.as_ref(), b"Second file content\n");
}

// ── CRC corruption ────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_crc_corruption_detected() {
    let data = b"corruption test payload";
    let mut bytes = build_rar5_store_archive(&[("test.txt", data)]);

    // Corrupt the last data byte (the 7-byte EOA block is at the very end).
    // Flipping this byte changes the data without touching the header CRC field,
    // so the post-extraction CRC32 check catches the mismatch.
    let corrupt_idx = bytes.len() - 7 - 1;
    bytes[corrupt_idx] ^= 0xFF;

    let (_guard, path) = write_tempfile(&bytes);
    let archive = open_rar(&path).await;
    let entry = &archive.entries()[0];

    let result = extract_bytes(&archive, entry, None).await;
    assert!(
        matches!(result, Err(CoreError::CrcMismatch { .. })),
        "expected CrcMismatch, got: {result:?}"
    );
}
