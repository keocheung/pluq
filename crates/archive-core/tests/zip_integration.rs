//! End-to-end ZIP integration tests via `ZipArchive::open()`.
//!
//! Exercises the full extraction pipeline through the public facade — not
//! `ZipArchive` directly — covering: listing, unencrypted extraction,
//! ZipCrypto extraction, WinZip AES extraction, progress reporting,
//! and cancellation.

use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use archive_core::{ArchiveEntry, CompressionMethod, CoreError, ProgressReporter, Source, ZipArchive};

async fn open_zip(path: &str) -> ZipArchive<Source> {
    ZipArchive::open(Source::detect(path)).await.expect("open ZIP")
}

async fn extract_bytes(
    archive: &ZipArchive<Source>,
    entry: &ArchiveEntry,
    password: Option<&[u8]>,
) -> Result<bytes::Bytes, CoreError> {
    let mut buf = Vec::new();
    let progress = std::sync::Arc::new(std::sync::Mutex::new(()));
    archive.extract(entry, &mut buf, progress, password).await?;
    Ok(bytes::Bytes::from(buf))
}
use zip::unstable::write::FileOptionsExt as _;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod as ZipMethod;

// ── Test data ──────────────────────────────────────────────────────────────

const ZIP_CRYPTO_PASSWORD: &[u8] = b"zipcrypto-integration-pw";
const AES_PASSWORD: &str = "aes-integration-pw";

fn large_content() -> Vec<u8> {
    (0u32..)
        .flat_map(|i| i.to_le_bytes())
        .take(300 * 1024)
        .collect()
}

const SMALL_CONTENT: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09";

fn zipcrypto_content() -> Vec<u8> {
    b"ZipCrypto deflated secret for integration test. "
        .iter()
        .cycle()
        .take(4096)
        .copied()
        .collect()
}

const AES_CONTENT: &[u8] = b"WinZip AES-256 stored secret for integration test!";

// ── Shared ZIP builder ─────────────────────────────────────────────────────

/// Build a test ZIP with all entry types. Returns `(NamedTempFile, path_string)`.
///
/// Entries:
/// 1. `plain_large.txt`        — ~300 KB, Deflated, no encryption
/// 2. `plain_small.bin`        — 10 bytes, Stored, no encryption
/// 3. `zipcrypto_deflated.txt` — Deflated + ZipCrypto (PKWARE) encryption
/// 4. `aes256_stored.bin`      — Stored + WinZip AES-256 encryption
/// 5. `subdir/`                — directory entry
fn build_integration_zip() -> (tempfile::NamedTempFile, String) {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));

        // 1. Large unencrypted Deflated file
        writer
            .start_file(
                "plain_large.txt",
                SimpleFileOptions::default().compression_method(ZipMethod::Deflated),
            )
            .unwrap();
        writer.write_all(&large_content()).unwrap();

        // 2. Small unencrypted Stored file
        writer
            .start_file(
                "plain_small.bin",
                SimpleFileOptions::default().compression_method(ZipMethod::Stored),
            )
            .unwrap();
        writer.write_all(SMALL_CONTENT).unwrap();

        // 3. ZipCrypto (traditional PKWARE) encrypted + Deflated
        writer
            .start_file(
                "zipcrypto_deflated.txt",
                SimpleFileOptions::default()
                    .compression_method(ZipMethod::Deflated)
                    .with_deprecated_encryption(ZIP_CRYPTO_PASSWORD).unwrap(),
            )
            .unwrap();
        writer.write_all(&zipcrypto_content()).unwrap();

        // 4. WinZip AES-256 encrypted + Stored
        writer
            .start_file(
                "aes256_stored.bin",
                SimpleFileOptions::default()
                    .compression_method(ZipMethod::Stored)
                    .with_aes_encryption(zip::AesMode::Aes256, AES_PASSWORD),
            )
            .unwrap();
        writer.write_all(AES_CONTENT).unwrap();

        // 5. Directory entry
        writer
            .add_directory("subdir/", SimpleFileOptions::default())
            .unwrap();

        let buf = writer.finish().unwrap().into_inner();
        std::fs::write(tmp.path(), &buf).unwrap();
    }
    let path = tmp.path().to_string_lossy().into_owned();
    (tmp, path)
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Verify entry count, names, encryption flags, and compression methods.
#[tokio::test]
async fn list_entries_count_names_flags_methods() {
    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 5, "expected 5 entries total");

    let plain_large = entries
        .iter()
        .find(|e| e.name == "plain_large.txt")
        .expect("plain_large.txt missing");
    let plain_small = entries
        .iter()
        .find(|e| e.name == "plain_small.bin")
        .expect("plain_small.bin missing");
    let zipcrypto = entries
        .iter()
        .find(|e| e.name == "zipcrypto_deflated.txt")
        .expect("zipcrypto_deflated.txt missing");
    let aes = entries
        .iter()
        .find(|e| e.name == "aes256_stored.bin")
        .expect("aes256_stored.bin missing");
    let dir = entries
        .iter()
        .find(|e| e.name == "subdir/")
        .expect("subdir/ missing");

    // Sizes
    assert_eq!(plain_large.uncompressed_size, 300 * 1024);
    assert_eq!(plain_small.uncompressed_size, SMALL_CONTENT.len() as u64);

    // Encryption flags
    assert!(!plain_large.is_encrypted, "plain_large must not be encrypted");
    assert!(!plain_small.is_encrypted, "plain_small must not be encrypted");
    assert!(zipcrypto.is_encrypted, "ZipCrypto entry must be encrypted");
    assert!(aes.is_encrypted, "AES entry must be encrypted");
    assert!(!dir.is_encrypted, "directory must not be encrypted");

    // Compression methods
    assert_eq!(plain_large.compression_method, Some(CompressionMethod::Deflate));
    assert_eq!(plain_small.compression_method, Some(CompressionMethod::Store));
    assert_eq!(
        zipcrypto.compression_method,
        Some(CompressionMethod::Deflate)
    );
    // AES entry: must report the real method (Store), not the AES marker (99)
    assert_eq!(
        aes.compression_method,
        Some(CompressionMethod::Store),
        "AES entry must report real compression method (Store), not AES marker 99"
    );

    // Directory flag
    assert!(dir.is_directory, "subdir/ must be flagged as a directory");
    assert!(!plain_large.is_directory);
}

/// Extract unencrypted entries and verify content matches.
#[tokio::test]
async fn extract_unencrypted_entries() {
    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_large.txt")
        .unwrap();
    let bytes = extract_bytes(&archive, entry, None).await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        large_content().as_slice(),
        "large Deflated entry content mismatch"
    );

    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_small.bin")
        .unwrap();
    let bytes = extract_bytes(&archive, entry, None).await.unwrap();
    assert_eq!(bytes.as_ref(), SMALL_CONTENT, "small Stored entry content mismatch");
}

/// Extract encrypted entries with correct passwords.
#[tokio::test]
async fn extract_encrypted_entries_correct_passwords() {
    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    // ZipCrypto
    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "zipcrypto_deflated.txt")
        .unwrap();
    let bytes = extract_bytes(&archive, entry, Some(ZIP_CRYPTO_PASSWORD))
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        zipcrypto_content().as_slice(),
        "ZipCrypto content mismatch"
    );

    // AES-256
    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "aes256_stored.bin")
        .unwrap();
    let bytes = extract_bytes(&archive, entry, Some(AES_PASSWORD.as_bytes()))
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), AES_CONTENT, "AES-256 content mismatch");
}

/// Extract encrypted entries with wrong passwords — verify correct error types.
#[tokio::test]
async fn extract_encrypted_entries_wrong_passwords() {
    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    // ZipCrypto with wrong password
    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "zipcrypto_deflated.txt")
        .unwrap();
    let result = extract_bytes(&archive, entry, Some(b"wrong-pw")).await;
    assert!(
        matches!(
            result,
            Err(CoreError::WrongPassword { .. })
                | Err(CoreError::CrcMismatch { .. })
                | Err(CoreError::BitstreamError { .. })
        ),
        "expected a password error for ZipCrypto wrong password, got: {result:?}"
    );

    // AES-256 with wrong password
    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "aes256_stored.bin")
        .unwrap();
    let result = extract_bytes(&archive, entry, Some(b"wrong-pw")).await;
    assert!(
        matches!(result, Err(CoreError::WrongPassword { .. })),
        "expected WrongPassword for AES-256 wrong password, got: {result:?}"
    );
}

/// `test()` passes for unencrypted entries and returns `PasswordRequired` for encrypted.
#[tokio::test]
async fn test_integrity_unencrypted_ok_encrypted_requires_password() {
    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    // Unencrypted entries pass
    let plain_large = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_large.txt")
        .unwrap();
    archive.test(plain_large, Arc::new(Mutex::new(()))).await.unwrap();

    let plain_small = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_small.bin")
        .unwrap();
    archive.test(plain_small, Arc::new(Mutex::new(()))).await.unwrap();

    // Encrypted entries without password → PasswordRequired
    let zipcrypto = archive
        .entries()
        .iter()
        .find(|e| e.name == "zipcrypto_deflated.txt")
        .unwrap();
    let result = archive.test(zipcrypto, Arc::new(Mutex::new(()))).await;
    assert!(
        matches!(result, Err(CoreError::PasswordRequired)),
        "expected PasswordRequired for ZipCrypto test(), got: {result:?}"
    );

    let aes = archive
        .entries()
        .iter()
        .find(|e| e.name == "aes256_stored.bin")
        .unwrap();
    let result = archive.test(aes, Arc::new(Mutex::new(()))).await;
    assert!(
        matches!(result, Err(CoreError::PasswordRequired)),
        "expected PasswordRequired for AES test(), got: {result:?}"
    );
}

/// Progress reporter receives correctly ordered callbacks during extraction.
#[tokio::test]
async fn progress_callbacks_recorded_correctly() {
    #[derive(Default)]
    struct State {
        entry_start_name: Option<String>,
        entry_start_size: u64,
        progress_values: Vec<u64>,
        entry_complete_success: Option<bool>,
    }

    struct RecordingReporter {
        state: Arc<Mutex<State>>,
    }

    impl ProgressReporter for RecordingReporter {
        fn on_entry_start(&mut self, name: &str, size: u64) {
            let mut s = self.state.lock().unwrap();
            s.entry_start_name = Some(name.to_owned());
            s.entry_start_size = size;
        }
        fn on_bytes_progress(&mut self, bytes_so_far: u64) {
            self.state.lock().unwrap().progress_values.push(bytes_so_far);
        }
        fn on_entry_complete(&mut self, _name: &str, success: bool) {
            self.state.lock().unwrap().entry_complete_success = Some(success);
        }
    }

    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_large.txt")
        .unwrap();
    let entry_name = entry.name.clone();
    let entry_size = entry.uncompressed_size;

    let state = Arc::new(Mutex::new(State::default()));
    let progress: Arc<Mutex<dyn ProgressReporter>> =
        Arc::new(Mutex::new(RecordingReporter { state: Arc::clone(&state) }));

    let mut buf = Vec::new();
    archive.extract(entry, &mut buf, progress, None).await.unwrap();
    assert_eq!(buf, large_content(), "extracted content must match original");

    let s = state.lock().unwrap();

    // on_entry_start called with correct args
    assert_eq!(
        s.entry_start_name.as_deref(),
        Some(entry_name.as_str()),
        "on_entry_start name must match entry name"
    );
    assert_eq!(
        s.entry_start_size, entry_size,
        "on_entry_start size must match uncompressed_size"
    );

    // on_bytes_progress called multiple times (~300 KB / 64 KB = ~5 chunks)
    assert!(
        s.progress_values.len() > 1,
        "expected multiple progress callbacks, got {}",
        s.progress_values.len()
    );

    // Final value equals uncompressed size
    assert_eq!(
        *s.progress_values.last().unwrap(),
        entry_size,
        "final progress value must equal uncompressed_size"
    );

    // Values must be strictly increasing
    for w in s.progress_values.windows(2) {
        assert!(w[1] > w[0], "progress values must be strictly increasing: {w:?}");
    }

    // on_entry_complete called with success = true
    assert_eq!(
        s.entry_complete_success,
        Some(true),
        "on_entry_complete must be called with success=true"
    );
}

// ── New-codec integration tests ────────────────────────────────────────────

/// Build a minimal ZIP in memory with one entry using the given compression method.
fn build_codec_zip(method: ZipMethod, name: &str, content: &[u8]) -> (tempfile::NamedTempFile, String) {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(name, SimpleFileOptions::default().compression_method(method))
        .unwrap();
    writer.write_all(content).unwrap();
    let buf = writer.finish().unwrap().into_inner();
    std::fs::write(tmp.path(), &buf).unwrap();
    let path = tmp.path().to_string_lossy().into_owned();
    (tmp, path)
}

/// BZip2: small entry round-trip (listing + extraction).
#[tokio::test]
async fn zip_bzip2_small_entry_round_trip() {
    let (_tmp, path) = build_codec_zip(ZipMethod::Bzip2, "bzip2_small.bin", SMALL_CONTENT);
    let archive = open_zip(&path).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "bzip2_small.bin");
    assert_eq!(entries[0].uncompressed_size, SMALL_CONTENT.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::BZip2));
    assert!(!entries[0].is_encrypted);

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), SMALL_CONTENT);
}

/// BZip2: large entry round-trip (~300 KB).
#[tokio::test]
async fn zip_bzip2_large_entry_round_trip() {
    let content = large_content();
    let (_tmp, path) = build_codec_zip(ZipMethod::Bzip2, "bzip2_large.bin", &content);
    let archive = open_zip(&path).await;
    let entries = archive.entries();

    assert_eq!(entries[0].uncompressed_size, content.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::BZip2));

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), content.as_slice());
}

/// Zstd: small entry round-trip.
#[tokio::test]
async fn zip_zstd_small_entry_round_trip() {
    let (_tmp, path) = build_codec_zip(ZipMethod::Zstd, "zstd_small.bin", SMALL_CONTENT);
    let archive = open_zip(&path).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "zstd_small.bin");
    assert_eq!(entries[0].uncompressed_size, SMALL_CONTENT.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::Zstd));
    assert!(!entries[0].is_encrypted);

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), SMALL_CONTENT);
}

/// Zstd: large entry round-trip (~300 KB).
#[tokio::test]
async fn zip_zstd_large_entry_round_trip() {
    let content = large_content();
    let (_tmp, path) = build_codec_zip(ZipMethod::Zstd, "zstd_large.bin", &content);
    let archive = open_zip(&path).await;
    let entries = archive.entries();

    assert_eq!(entries[0].uncompressed_size, content.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::Zstd));

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), content.as_slice());
}

// The `zip` crate (v2) does not support writing LZMA-compressed entries, so we
// use static fixture files created with Python's `zipfile` module (ZIP_LZMA).
// Fixture generation (run once, output committed):
//   python3 -c "import zipfile, struct, pathlib
//   fixtures = pathlib.Path('crates/archive-core/tests/fixtures')
//   with zipfile.ZipFile(fixtures/'lzma_small.zip', 'w', zipfile.ZIP_LZMA) as z:
//       z.writestr('lzma_small.bin', bytes(range(10)))
//   large = b''.join(i.to_bytes(4,'little') for i in range(300*1024//4))
//   with zipfile.ZipFile(fixtures/'lzma_large.zip', 'w', zipfile.ZIP_LZMA) as z:
//       z.writestr('lzma_large.bin', large)"

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// LZMA: small entry round-trip (static fixture).
#[tokio::test]
async fn zip_lzma_small_entry_round_trip() {
    let archive = open_zip(&fixture("lzma_small.zip")).await;
    let entries = archive.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "lzma_small.bin");
    assert_eq!(entries[0].uncompressed_size, SMALL_CONTENT.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::Lzma));
    assert!(!entries[0].is_encrypted);

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), SMALL_CONTENT);
}

/// LZMA: large entry round-trip (~300 KB, static fixture).
#[tokio::test]
async fn zip_lzma_large_entry_round_trip() {
    let content = large_content();
    let archive = open_zip(&fixture("lzma_large.zip")).await;
    let entries = archive.entries();

    assert_eq!(entries[0].uncompressed_size, content.len() as u64);
    assert_eq!(entries[0].compression_method, Some(CompressionMethod::Lzma));

    let bytes = extract_bytes(&archive, &entries[0], None).await.unwrap();
    assert_eq!(bytes.as_ref(), content.as_slice());
}

// ───────────────────────────────────────────────────────────────────────────

/// Cancellation after 50% of the entry yields `CoreError::Cancelled`.
#[tokio::test]
async fn cancel_after_50_percent_returns_cancelled() {
    struct CancelAfter50Pct {
        total_size: u64,
        bytes_so_far: u64,
    }

    impl ProgressReporter for CancelAfter50Pct {
        fn on_entry_start(&mut self, _name: &str, size: u64) {
            self.total_size = size;
        }
        fn on_bytes_progress(&mut self, bytes_so_far: u64) {
            self.bytes_so_far = bytes_so_far;
        }
        fn should_cancel(&self) -> bool {
            self.total_size > 0 && self.bytes_so_far > self.total_size / 2
        }
    }

    let (_tmp, path) = build_integration_zip();
    let archive = open_zip(&path).await;

    let entry = archive
        .entries()
        .iter()
        .find(|e| e.name == "plain_large.txt")
        .unwrap();

    let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(CancelAfter50Pct {
        total_size: 0,
        bytes_so_far: 0,
    }));

    let mut buf = Vec::new();
    let result = archive.extract(entry, &mut buf, progress, None).await;

    assert!(
        matches!(result, Err(CoreError::Cancelled)),
        "expected Cancelled after exceeding 50% of entry size, got: {result:?}"
    );
}
