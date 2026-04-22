//! Batch-extraction integration tests for ZIP.
//!
//! Verifies that `extract_selected` makes fewer `read_stream` calls than
//! there are entries (O(M) clusters instead of O(N) entries).

#[path = "common/mod.rs"]
mod common;

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use archive_core::{ArchiveEntry, ZipArchive};
use zip::CompressionMethod as ZipMethod;
use zip::write::SimpleFileOptions;

fn build_multi_entry_zip() -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();

    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file(
        "file1.txt",
        SimpleFileOptions::default().compression_method(ZipMethod::Stored),
    )
    .unwrap();
    w.write_all(b"Hello from file one!").unwrap();
    w.start_file(
        "file2.txt",
        SimpleFileOptions::default().compression_method(ZipMethod::Stored),
    )
    .unwrap();
    w.write_all(b"Second file content!").unwrap();
    let buf = w.finish().unwrap().into_inner();
    std::fs::write(&path, &buf).unwrap();

    (tmp, path)
}

#[tokio::test]
async fn zip_extract_selected_batches_into_single_cluster() {
    let (_tmp, path) = build_multi_entry_zip();

    let reader = common::CountingReadAt::new(&path);
    let stream_calls = reader.stream_calls.clone();

    let archive = ZipArchive::open(reader, &archive_core::ResourceLimits::default())
        .await
        .expect("open ZIP");

    let entries: Vec<&ArchiveEntry> = archive
        .entries()
        .iter()
        .filter(|e| !e.is_directory)
        .collect();
    assert_eq!(entries.len(), 2, "fixture must have exactly 2 entries");

    let expected_bytes: u64 = entries.iter().map(|e| e.uncompressed_size).sum();

    let calls_before = stream_calls.load(std::sync::atomic::Ordering::SeqCst);

    let progress = Arc::new(Mutex::new(()));
    let result = archive
        .extract_selected(
            &entries,
            |_| Some(Box::new(tokio::io::sink()) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>),
            progress,
            None,
            &archive_core::ResourceLimits::default(),
        )
        .await
        .expect("extract_selected failed");

    let calls_after = stream_calls.load(std::sync::atomic::Ordering::SeqCst);
    let delta = calls_after - calls_before;

    assert_eq!(result.succeeded, 2, "both entries must succeed");
    assert_eq!(
        result.bytes_written, expected_bytes,
        "bytes_written must equal sum of uncompressed sizes"
    );
    assert!(
        delta < entries.len(),
        "expected fewer read_stream calls than entries (got {delta} for {} entries — batching not active?)",
        entries.len()
    );
}

#[tokio::test]
async fn zip_extract_selected_max_total_unpacked_rejected() {
    let (_tmp, path) = build_multi_entry_zip();
    let reader = common::CountingReadAt::new(&path);
    let archive = ZipArchive::open(reader, &archive_core::ResourceLimits::default())
        .await
        .expect("open ZIP");

    let entries: Vec<&ArchiveEntry> = archive
        .entries()
        .iter()
        .filter(|e| !e.is_directory)
        .collect();

    // The two files together are around 40 bytes. We set limit to 10 bytes.
    let limits = archive_core::ResourceLimits {
        max_total_unpacked: 10,
        ..Default::default()
    };

    let progress = Arc::new(Mutex::new(()));
    let result = archive
        .extract_selected(
            &entries,
            |_| Some(Box::new(tokio::io::sink()) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>),
            progress,
            None,
            &limits,
        )
        .await;

    assert!(
        matches!(result, Err(archive_core::CoreError::ResourceLimitExceeded { .. })),
        "expected ResourceLimitExceeded for max_total_unpacked, got: {:?}",
        result
    );
}
