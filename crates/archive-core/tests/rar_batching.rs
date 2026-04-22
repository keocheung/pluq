//! Batch-extraction integration tests for RAR5.
//!
//! Uses `rar5_store.rar` (2 store entries, 20 bytes each).
//! `CountingReadAt` returns `LocalPath::local_path() == None` which
//! forces the cloud batch path in `RarArchive::extract_selected`.

#[path = "common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use archive_core::{ArchiveEntry, ArchiveReader, RarArchive};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[tokio::test]
async fn rar5_extract_selected_batches_into_single_cluster() {
    let path = fixture("rar5_store.rar");

    let reader = common::CountingReadAt::new(&path);
    let stream_calls = reader.stream_calls.clone();

    let mut archive = RarArchive::open(reader, &archive_core::ResourceLimits::default())
        .await
        .expect("open RAR5");

    let progress: Arc<Mutex<dyn archive_core::ProgressReporter>> = Arc::new(Mutex::new(()));
    archive
        .scan_metadata(progress.clone(), &archive_core::ResourceLimits::default())
        .await
        .expect("scan_metadata");

    let entries: Vec<&ArchiveEntry> = archive
        .entries()
        .iter()
        .filter(|e| !e.is_directory)
        .collect();
    assert_eq!(entries.len(), 2, "rar5_store.rar must have 2 file entries");

    let expected_bytes: u64 = entries.iter().map(|e| e.uncompressed_size).sum();

    let calls_before = stream_calls.load(std::sync::atomic::Ordering::SeqCst);

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
        "expected fewer read_stream calls than entries after scan (got {delta} for {} entries — batching not active?)",
        entries.len()
    );
}
