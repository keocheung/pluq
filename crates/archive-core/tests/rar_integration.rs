//! Integration tests for RAR5 support.
//!
//! Uses pre-built `.rar` fixtures from the `tests/fixtures/` directory.
//! `rar5_unicode_entry.rar` is copied from the `unrar` crate's own test data;
//! it is a RAR5 non-solid archive containing exactly one compressed entry.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use archive_core::{ArchiveCore, CoreError, ProgressReporter};

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
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("should open RAR5 archive");
    assert!(!archive.entries().is_empty(), "archive should have entries");
}

#[tokio::test]
async fn rar4_gives_unsupported_format_error() {
    // rar4_sample.rar carries the 7-byte RAR4 magic — must return UnsupportedFormat.
    let result = ArchiveCore::open(&fixture("rar4_sample.rar")).await;
    assert!(
        matches!(result, Err(CoreError::UnsupportedFormat { .. })),
        "expected UnsupportedFormat for RAR4 archive, got an unexpected result"
    );
}

// ── Listing ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_listing_unicode_entry_archive() {
    // unicode-entry.rar contains exactly one file: "unicodefilename❤️.txt"
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("open");
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
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("open");
    let entry = &archive.entries()[0];
    let data: Bytes = archive
        .extract_to_bytes(entry, None)
        .await
        .expect("extract");
    assert_eq!(data.as_ref(), b"foobar\n");
}

// ── Extraction cost ───────────────────────────────────────────────────────────

#[tokio::test]
async fn rar5_extraction_cost_positive() {
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("open");
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
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("open");
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
    let archive = ArchiveCore::open(&fixture("rar5_unicode_entry.rar"))
        .await
        .expect("open");
    let entry = &archive.entries()[0];
    archive
        .test(entry)
        .await
        .expect("test integrity check should pass");
}
