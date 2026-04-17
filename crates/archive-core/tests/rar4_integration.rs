//! Integration tests for RAR4 support.
//!
//! Uses `rar4_sample.rar` (from the fixtures directory) for listing tests.
//! Extraction tests use in-memory synthetic RAR4 Store archives built by
//! `build_rar4_store_archive`, since modern `rar` (v7) no longer creates RAR4 archives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use archive_core::{
    ArchiveEntry, ArchiveReader, CoreError, LocalReadAt, ProgressReporter, Rar4Archive,
};
use bytes::Bytes;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn write_tempfile(bytes: &[u8]) -> (tempfile::NamedTempFile, String) {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), bytes).unwrap();
    let path = f.path().to_str().unwrap().to_owned();
    (f, path)
}

async fn open_rar4(path: &str) -> Rar4Archive<LocalReadAt> {
    Rar4Archive::open(LocalReadAt::new(path))
        .await
        .expect("open RAR4")
}

async fn extract_bytes(
    archive: &Rar4Archive<LocalReadAt>,
    entry: &ArchiveEntry,
    password: Option<&[u8]>,
) -> Result<Bytes, CoreError> {
    let mut buf = Vec::new();
    let progress = Arc::new(Mutex::new(()));
    archive.extract(entry, &mut buf, progress, password).await?;
    Ok(Bytes::from(buf))
}

/// CRC-16/ARC (polynomial 0xA001, reflected) — same algorithm as RAR4 block headers.
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

/// Build a minimal RAR4 archive containing Store entries (method 0x30).
///
/// Each entry's compressed data equals its uncompressed data.
/// Constraints: `name.len() <= 255`, `data.len() <= u32::MAX`.
fn build_rar4_store_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    // RAR4 marker block (signature): 7 bytes
    const RAR4_SIG: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];

    // Main archive header (type 0x73), HEAD_SIZE = 13
    // Layout: HEAD_CRC(2) HEAD_TYPE(1) HEAD_FLAGS(2) HEAD_SIZE(2) [6 reserved bytes]
    let mut main_hdr = [0u8; 13];
    main_hdr[2] = 0x73; // HEAD_TYPE
    main_hdr[3] = 0x00; // HEAD_FLAGS lo
    main_hdr[4] = 0x00; // HEAD_FLAGS hi
    main_hdr[5] = 13; // HEAD_SIZE lo
    main_hdr[6] = 0x00; // HEAD_SIZE hi
    // bytes 7-12: reserved (all zero)
    let crc = crc16_arc(&main_hdr[2..13]);
    main_hdr[0] = crc as u8;
    main_hdr[1] = (crc >> 8) as u8;

    // End-of-archive block (type 0x7B), 7 bytes
    let mut eoa = [0u8; 7];
    eoa[2] = 0x7B; // HEAD_TYPE
    eoa[3] = 0x00; // HEAD_FLAGS lo  (0x4000)
    eoa[4] = 0x40; // HEAD_FLAGS hi
    eoa[5] = 7; // HEAD_SIZE lo
    eoa[6] = 0x00; // HEAD_SIZE hi
    let crc = crc16_arc(&eoa[2..7]);
    eoa[0] = crc as u8;
    eoa[1] = (crc >> 8) as u8;

    let mut archive = RAR4_SIG.to_vec();
    archive.extend_from_slice(&main_hdr);

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len();
        let pack_size = data.len() as u32;
        let unp_size = data.len() as u32;
        let file_crc = crc32fast::hash(data);
        let head_size = (32 + name_len) as u16;

        // FILE header (type 0x74), FLAG_LONG_BLOCK (0x8000) required so
        // parse_block_header reads PACK_SIZE as add_size.
        let head_flags: u16 = 0x8000;

        let mut hdr = vec![0u8; head_size as usize];
        hdr[2] = 0x74; // HEAD_TYPE
        hdr[3] = head_flags as u8; // HEAD_FLAGS lo
        hdr[4] = (head_flags >> 8) as u8; // HEAD_FLAGS hi
        hdr[5] = head_size as u8; // HEAD_SIZE lo
        hdr[6] = (head_size >> 8) as u8; // HEAD_SIZE hi
        hdr[7..11].copy_from_slice(&pack_size.to_le_bytes()); // PACK_SIZE
        hdr[11..15].copy_from_slice(&unp_size.to_le_bytes()); // UNP_SIZE
        hdr[15] = 3; // HOST_OS = Unix
        hdr[16..20].copy_from_slice(&file_crc.to_le_bytes()); // FILE_CRC
        // FTIME at [20..24]: a plausible DOS timestamp (2015-08-03 16:09:00)
        let ftime: u32 = 0x46C3_8120;
        hdr[20..24].copy_from_slice(&ftime.to_le_bytes());
        hdr[24] = 0x14; // UNP_VER = 2.0
        hdr[25] = 0x30; // METHOD = Store
        hdr[26] = name_len as u8; // NAME_SIZE lo
        hdr[27] = (name_len >> 8) as u8; // NAME_SIZE hi
        hdr[28] = 0x20; // ATTR lo (Archive bit)
        // bytes 29-31: ATTR hi bytes, all zero
        hdr[32..32 + name_len].copy_from_slice(name_bytes); // filename

        let crc = crc16_arc(&hdr[2..head_size as usize]);
        hdr[0] = crc as u8;
        hdr[1] = (crc >> 8) as u8;

        archive.extend_from_slice(&hdr);
        archive.extend_from_slice(data);
    }

    archive.extend_from_slice(&eoa);
    archive
}

// ── Listing ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar4_listing_sample_fixture() {
    // rar4_sample.rar contains one encrypted entry: .gitignore (18 bytes)
    let archive = open_rar4(&fixture("rar4_sample.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 1, "expected 1 entry in rar4_sample.rar");
    assert_eq!(entries[0].name, ".gitignore");
    assert_eq!(entries[0].uncompressed_size, 18);
    assert!(entries[0].is_encrypted, "entry should be marked encrypted");
    assert!(!entries[0].is_directory);
}

#[tokio::test]
async fn rar4_listing_synthetic_two_files() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);

    let archive = open_rar4(&path).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
    assert_eq!(entries[0].uncompressed_size, file1.len() as u64);
    assert_eq!(entries[1].uncompressed_size, file2.len() as u64);
    assert!(!entries[0].is_directory);
    assert!(!entries[0].is_encrypted);
    assert!(!entries[1].is_encrypted);
}

// ── Extraction ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar4_store_extraction_exact_content() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);
    let archive = open_rar4(&path).await;

    let data1 = extract_bytes(&archive, &archive.entries()[0], None)
        .await
        .expect("extract file1");
    assert_eq!(data1.as_ref(), file1 as &[u8]);

    let data2 = extract_bytes(&archive, &archive.entries()[1], None)
        .await
        .expect("extract file2");
    assert_eq!(data2.as_ref(), file2 as &[u8]);
}

#[tokio::test]
async fn rar4_extraction_cost_includes_header() {
    let bytes = build_rar4_store_archive(&[("test.txt", b"payload")]);
    let (_g, path) = write_tempfile(&bytes);
    let archive = open_rar4(&path).await;
    let entry = &archive.entries()[0];
    let cost = archive.extraction_cost(entry);
    let loc = &archive.locations()[0];
    assert_eq!(
        cost,
        loc.header_size + loc.data_size,
        "RAR4 cost must be header + data"
    );
    assert!(loc.header_size > 0);
}

// ── CRC verification ──────────────────────────────────────────────────────────

#[tokio::test]
async fn rar4_crc_corruption_detected() {
    let data = b"corruption test payload";
    let mut bytes = build_rar4_store_archive(&[("test.txt", data)]);

    // Corrupt the last data byte (7-byte EOA is at the end).
    let corrupt_idx = bytes.len() - 7 - 1;
    bytes[corrupt_idx] ^= 0xFF;

    let (_g, path) = write_tempfile(&bytes);
    let archive = open_rar4(&path).await;
    let result = extract_bytes(&archive, &archive.entries()[0], None).await;
    assert!(
        matches!(result, Err(CoreError::CrcMismatch { .. })),
        "expected CrcMismatch, got: {result:?}"
    );
}

// ── Integrity test ────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar4_test_integrity_passes() {
    let bytes = build_rar4_store_archive(&[("hello.txt", b"hello world")]);
    let (_g, path) = write_tempfile(&bytes);
    let archive = open_rar4(&path).await;
    archive
        .test(&archive.entries()[0], Arc::new(Mutex::new(())))
        .await
        .expect("integrity check should pass");
}

// ── Elastic scan: scan parity ──────────────────────────────────────────────────

#[tokio::test]
async fn rar4_elastic_scan_parity_sample_fixture() {
    let archive = open_rar4(&fixture("rar4_sample.rar")).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, ".gitignore");
    assert_eq!(entries[0].uncompressed_size, 18);
    assert!(entries[0].is_encrypted);
}

#[tokio::test]
async fn rar4_elastic_scan_parity_synthetic() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);

    let archive = open_rar4(&path).await;
    let entries = archive.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
}

// ── Elastic scan: scan progress callbacks ─────────────────────────────────────

struct ScanRecorder {
    scan_started: Vec<u64>,
    scan_progress: Vec<(u64, usize)>,
    scan_completed: Vec<usize>,
}

impl ScanRecorder {
    fn new() -> Self {
        Self {
            scan_started: vec![],
            scan_progress: vec![],
            scan_completed: vec![],
        }
    }
}

impl ProgressReporter for ScanRecorder {
    fn on_scan_start(&mut self, total_bytes: u64) {
        self.scan_started.push(total_bytes);
    }
    fn on_scan_progress(&mut self, bytes: u64, count: usize) {
        self.scan_progress.push((bytes, count));
    }
    fn on_scan_complete(&mut self, count: usize) {
        self.scan_completed.push(count);
    }
}

#[tokio::test]
async fn rar4_elastic_scan_progress_callbacks() {
    let file1 = b"Hello from file one\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1)]);
    let (_g, path) = write_tempfile(&bytes);

    let mut archive = Rar4Archive::open(LocalReadAt::new(&path))
        .await
        .expect("open");
    let recorder = Arc::new(Mutex::new(ScanRecorder::new()));
    archive
        .scan_metadata(recorder.clone())
        .await
        .expect("scan_metadata");

    let r = recorder.lock().unwrap();
    let archive_size = archive.archive_size();

    assert_eq!(r.scan_started.len(), 1);
    assert_eq!(r.scan_started[0], archive_size);
    assert!(!r.scan_progress.is_empty());
    let (last_offset, _) = r.scan_progress.last().copied().unwrap();
    assert!(last_offset > 0);
    assert!(last_offset <= archive_size);
    assert_eq!(r.scan_completed.len(), 1);
    assert_eq!(r.scan_completed[0], 1);
}

// ── extract_all_streaming (local path) ────────────────────────────────────────

/// Simple `Write` impl that appends to a shared `Vec<u8>`.
struct LockedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LockedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn rar4_extract_all_streaming_local_byte_count() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);

    let mut archive = Rar4Archive::open(LocalReadAt::new(&path))
        .await
        .expect("open");
    let progress = Arc::new(Mutex::new(()));
    let total = archive
        .extract_all_streaming(
            |_| true,
            |_entry| Some(Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>),
            progress,
            None,
        )
        .await
        .expect("extract_all_streaming");

    assert_eq!(total, (file1.len() + file2.len()) as u64);
}

#[tokio::test]
async fn rar4_extract_all_streaming_local_content() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);

    let mut archive = Rar4Archive::open(LocalReadAt::new(&path))
        .await
        .expect("open");

    let buf1 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let buf2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let buf1c = buf1.clone();
    let buf2c = buf2.clone();
    let mut call = 0_usize;

    archive
        .extract_all_streaming(
            |_| true,
            move |_entry| {
                call += 1;
                if call == 1 {
                    Some(Box::new(LockedSink(buf1c.clone())) as Box<dyn std::io::Write + Send>)
                } else {
                    Some(Box::new(LockedSink(buf2c.clone())) as Box<dyn std::io::Write + Send>)
                }
            },
            Arc::new(Mutex::new(())),
            None,
        )
        .await
        .expect("extract_all_streaming");

    assert_eq!(buf1.lock().unwrap().as_slice(), file1 as &[u8]);
    assert_eq!(buf2.lock().unwrap().as_slice(), file2 as &[u8]);
}

#[tokio::test]
async fn rar4_extract_all_streaming_local_filter() {
    let file1 = b"Hello from file one\n";
    let file2 = b"Second file content\n";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_g, path) = write_tempfile(&bytes);

    let mut archive = Rar4Archive::open(LocalReadAt::new(&path))
        .await
        .expect("open");
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let bufc = buf.clone();

    let progress = Arc::new(Mutex::new(()));
    let total =
        archive
            .extract_all_streaming(
                |e| e.name == "file1.txt", // only extract file1
                move |_entry| {
                    Some(Box::new(LockedSink(bufc.clone())) as Box<dyn std::io::Write + Send>)
                },
                progress,
                None,
            )
            .await
            .expect("extract_all_streaming");

    assert_eq!(total, file1.len() as u64);
    assert_eq!(buf.lock().unwrap().as_slice(), file1 as &[u8]);
}
