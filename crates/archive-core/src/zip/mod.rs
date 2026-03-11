//! ZIP archive reader — built on top of [`ReadAt`].
//!
//! ```ignore
//! let reader = LocalReadAt::new("archive.zip");
//! let zip = ZipArchive::open(reader).await?;
//!
//! for entry in zip.entries() {
//!     println!("{} ({} bytes)", entry.name, entry.uncompressed_size);
//! }
//!
//! let mut sink = tokio::io::sink();
//! zip.extract(&zip.entries()[0], &mut sink).await?;
//! ```

pub mod index;
mod parse;

pub use index::ZipIndex;
use parse::ZipEntryLocation;

use std::io::Read;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::io::SyncIoBridge;

use crate::archive_reader::ArchiveReader;
use crate::codec::build_decoder;
use crate::junk;
use crate::progress::ProgressReporter;
use crate::{ArchiveEntry, CoreError, ReadAt};

/// A parsed ZIP archive backed by any [`ReadAt`] source.
///
/// Created via [`ZipArchive::open`], which reads the EOCD and Central
/// Directory to build the file listing. Extraction calls issue additional
/// targeted range reads.
///
/// # Internal layout
///
/// Holds two parallel `Vec`s at the same indices:
/// - `entries` — public, format-agnostic [`ArchiveEntry`] metadata
/// - `locations` — private [`ZipEntryLocation`] with byte offsets, method IDs, CRC values
///
/// `entry.index` is the key from `entries` into `locations`.
pub struct ZipArchive<R: ReadAt> {
    reader: R,
    entries: Vec<ArchiveEntry>,
    locations: Vec<ZipEntryLocation>,
}

impl<R: ReadAt> ZipArchive<R> {
    /// Open a ZIP archive by reading its EOCD and Central Directory.
    ///
    /// This issues exactly **2 range reads** (or 1 if the file is small).
    pub async fn open(reader: R) -> Result<Self, CoreError> {
        let eocd = parse::find_eocd(&reader).await?;
        let (entries, locations) = parse::parse_central_directory(&reader, &eocd).await?;

        Ok(Self {
            reader,
            entries,
            locations,
        })
    }

    /// Return all entries (files and directories) in the archive, including
    /// OS-generated junk (`.DS_Store`, `__MACOSX/`, `Thumbs.db`, etc.).
    ///
    /// Prefer [`entries_clean`](Self::entries_clean) for user-facing output.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Return entries with OS-generated junk files filtered out.
    ///
    /// This is the **default** for user-facing output (CLI, UI).
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries.iter().filter(|e| !junk::is_junk(e)).collect()
    }

    /// Bytes that must be downloaded and decompressed to produce this entry.
    ///
    /// For ZIP every entry is independently seekable, so this is always
    /// `entry.compressed_size`.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        entry.compressed_size
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// CRC verification is mandatory and structural — it is the primary
    /// corruption and wrong-password detection mechanism.
    ///
    /// Follows the mandated extraction pattern (ARCHITECTURE.md §Async/Sync Boundary):
    /// 1. **async**: `read_range()` for LFH header (small, bounded)
    /// 2. **async**: `read_stream()` → compressed `AsyncRead` (could be GBs)
    /// 3. **`spawn_blocking`**: `SyncIoBridge` wraps stream → sync `Read`;
    ///    decompress via `build_decoder`, CRC-verify, write to sync side of bridge
    /// 4. **async**: flush sink
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<u64, CoreError> {
        if entry.is_directory {
            return Ok(0);
        }

        if entry.is_encrypted {
            return Err(CoreError::PasswordRequired);
        }

        let location = &self.locations[entry.index];

        // ── Step 1 (async): resolve data offset via Local File Header ─────
        // LFH is 30 bytes — small, use read_range.
        let data_offset = parse::resolve_data_offset(&self.reader, location)
            .await
            .map_err(|e| match e {
                // LFH signature mismatch — structural corruption
                crate::io::ReadError::Transport(msg) => CoreError::InvalidFormat {
                    message: msg.to_string(),
                },
                other => CoreError::Io(other),
            })?;

        let compressed_size = entry.compressed_size;
        let uncompressed_size = entry.uncompressed_size;

        // Map the raw method ID to the shared CompressionMethod enum.
        let method = parse::zip_method_to_codec(location.method_id).ok_or_else(|| {
            CoreError::UnsupportedCompression {
                algorithm: format!("ZIP method id {}", location.method_id),
            }
        })?;

        // ── Step 2 (async): get streaming AsyncRead for compressed data ────
        // read_stream returns a live byte stream — no buffering of multi-GB data.
        let compressed_stream = self
            .reader
            .read_stream(data_offset, compressed_size)
            .await
            .map_err(CoreError::Io)?;

        // ── Step 3 (spawn_blocking): decompress + CRC verify ─────────────
        // SyncIoBridge wraps the AsyncRead into a sync Read by driving the
        // async polling from the current runtime handle.
        let entry_name = entry.name.clone();
        let expected_crc = location.crc32;
        let handle = tokio::runtime::Handle::current();
        let sync_reader = SyncIoBridge::new_with_handle(compressed_stream, handle);

        let verified: Vec<u8> = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CoreError> {
            let mut decoder = build_decoder(method, sync_reader, &[])?;

            const CHUNK_SIZE: usize = 64 * 1024; // 64 KB
            let mut hasher = crc32fast::Hasher::new();
            let mut total_written: u64 = 0;
            let mut chunk_buf = vec![0u8; CHUNK_SIZE];
            let mut verified_buf = Vec::with_capacity(uncompressed_size as usize);

            {
                let mut p = progress.lock().unwrap();
                p.on_entry_start(&entry_name, uncompressed_size);
            }

            loop {
                let n = decoder.read(&mut chunk_buf).map_err(|io_err| {
                    // Try to recover a structured ReadError embedded by read_stream impls.
                    // If the io::Error wraps a ReadError (from mid-stream failure), extract it.
                    // Otherwise, it's a decompressor error — classify as BitstreamError.
                    if let Some(inner) = io_err.into_inner()
                        && let Ok(read_error) = inner.downcast::<crate::io::ReadError>()
                    {
                        return CoreError::Io(*read_error);
                    }
                    CoreError::BitstreamError {
                        detail: format!("decompression failed for '{entry_name}'"),
                    }
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&chunk_buf[..n]);
                verified_buf.extend_from_slice(&chunk_buf[..n]);
                total_written += n as u64;

                {
                    let mut p = progress.lock().unwrap();
                    p.on_bytes_progress(total_written);
                    if p.should_cancel() {
                        p.on_entry_complete(&entry_name, false);
                        return Err(CoreError::Cancelled);
                    }
                }
            }

            // CRC32 verification — mandatory before returning any bytes.
            let actual_crc = hasher.finalize();
            if actual_crc != expected_crc {
                let mut p = progress.lock().unwrap();
                p.on_entry_complete(&entry_name, false);
                return Err(CoreError::CrcMismatch {
                    entry: entry_name,
                    expected: expected_crc,
                    actual: actual_crc,
                });
            }

            {
                let mut p = progress.lock().unwrap();
                p.on_entry_complete(&entry_name, true);
            }

            Ok(verified_buf)
        })
        .await
        .map_err(|e| CoreError::BitstreamError {
            detail: format!("decompression task panicked: {e}"),
        })??;

        // ── Step 4 (async): stream verified bytes to sink ─────────────────
        let bytes_written = verified.len() as u64;
        sink.write_all(&verified)
            .await
            .map_err(|e| CoreError::BitstreamError {
                detail: format!("write to sink failed: {e}"),
            })?;

        Ok(bytes_written)
    }

    /// Decompress and verify CRC without writing output anywhere.
    ///
    /// Equivalent to [`extract`](Self::extract) to a no-op async sink.
    /// Used for archive integrity checking.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        let progress = Arc::new(Mutex::new(()));
        self.extract(entry, &mut tokio::io::sink(), progress)
            .await
            .map(|_| ())
    }
}

impl<R: ReadAt + Send + Sync + 'static> ArchiveReader for ZipArchive<R> {
    fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        entry.compressed_size
    }

    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<u64, CoreError> {
        ZipArchive::extract(self, entry, sink, progress).await
    }

    async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        ZipArchive::test(self, entry).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use zip::CompressionMethod as ZipCrateMethod;
    use zip::write::SimpleFileOptions;

    use crate::codec::CompressionMethod;
    use crate::io::LocalReadAt;

    use super::*;

    /// Helper: create a real ZIP file in a temp file, return a `LocalReadAt`.
    fn create_test_zip(
        files: &[(&str, &[u8], ZipCrateMethod)],
    ) -> (tempfile::NamedTempFile, LocalReadAt) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data, method) in files {
                let options = SimpleFileOptions::default().compression_method(*method);
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            let buf = writer.finish().unwrap().into_inner();
            std::fs::write(tmp.path(), &buf).unwrap();
        }
        let reader = LocalReadAt::new(tmp.path());
        (tmp, reader)
    }

    /// Helper: extract entry to a Vec<u8> (convenience for tests).
    async fn extract_to_vec(zip: &ZipArchive<LocalReadAt>, entry: &ArchiveEntry) -> Vec<u8> {
        let mut buf = Vec::new();
        zip.extract(entry, &mut buf, Arc::new(Mutex::new(())))
            .await
            .unwrap();
        buf
    }

    #[tokio::test]
    async fn list_files_in_zip() {
        let (_tmp, reader) = create_test_zip(&[
            ("hello.txt", b"Hello, world!", ZipCrateMethod::Deflated),
            (
                "data/numbers.bin",
                b"\x01\x02\x03\x04",
                ZipCrateMethod::Stored,
            ),
        ]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let entries = zip.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].uncompressed_size, 13);
        assert!(!entries[0].is_directory);

        assert_eq!(entries[1].name, "data/numbers.bin");
        assert_eq!(entries[1].uncompressed_size, 4);
    }

    #[tokio::test]
    async fn extract_deflated_file() {
        let content = b"Archive Sniper is awesome! Compressing this text with Deflate.";
        let (_tmp, reader) =
            create_test_zip(&[("compressed.txt", content, ZipCrateMethod::Deflated)]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(data.as_slice(), content);
    }

    #[tokio::test]
    async fn extract_stored_file() {
        let content = b"\x00\x01\x02\x03\x04\x05\x06\x07";
        let (_tmp, reader) = create_test_zip(&[("raw.bin", content, ZipCrateMethod::Stored)]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(data.as_slice(), content);
    }

    #[tokio::test]
    async fn handles_empty_zip() {
        let (_tmp, reader) = create_test_zip(&[]);

        let zip = ZipArchive::open(reader).await.unwrap();
        assert!(zip.entries().is_empty());
    }

    #[tokio::test]
    async fn compression_method_field_is_set() {
        let (_tmp, reader) = create_test_zip(&[
            ("deflated.txt", b"hello", ZipCrateMethod::Deflated),
            ("stored.bin", b"hello", ZipCrateMethod::Stored),
        ]);
        let zip = ZipArchive::open(reader).await.unwrap();
        let entries = zip.entries();
        assert_eq!(
            entries[0].compression_method,
            Some(CompressionMethod::Deflate)
        );
        assert_eq!(
            entries[1].compression_method,
            Some(CompressionMethod::Store)
        );
    }

    #[tokio::test]
    async fn modified_at_is_parsed() {
        let (_tmp, reader) = create_test_zip(&[("file.txt", b"data", ZipCrateMethod::Stored)]);
        let zip = ZipArchive::open(reader).await.unwrap();
        // The zip crate writes a real timestamp, so modified_at should be Some.
        assert!(zip.entries()[0].modified_at.is_some());
    }

    #[tokio::test]
    async fn test_passes_on_valid_entry() {
        let content = b"integrity check content";
        let (_tmp, reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader).await.unwrap();
        zip.test(&zip.entries()[0]).await.unwrap();
    }

    #[tokio::test]
    async fn extract_returns_bytes_written() {
        let content = b"counting bytes";
        let (_tmp, reader) = create_test_zip(&[("count.txt", content, ZipCrateMethod::Stored)]);
        let zip = ZipArchive::open(reader).await.unwrap();
        let mut sink = Vec::new();
        let bytes_written = zip
            .extract(&zip.entries()[0], &mut sink, Arc::new(Mutex::new(())))
            .await
            .unwrap();
        assert_eq!(bytes_written, content.len() as u64);
        assert_eq!(sink, content);
    }

    #[tokio::test]
    async fn crc_mismatch_detected() {
        // Build a valid ZIP and corrupt one byte in the compressed data stream.
        let content = b"This is some content that will be Deflated and then corrupted.";
        let (_tmp, _reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Deflated)]);

        // Read the raw ZIP bytes, flip a byte deep in the compressed data section.
        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        // The compressed data starts after the Local File Header (at least 30 + name bytes).
        // Flip a byte somewhere in the middle of the file, well past the headers.
        let mid = raw.len() / 2;
        raw[mid] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2).await.unwrap();
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(&zip.entries()[0], &mut sink, Arc::new(Mutex::new(())))
            .await;

        // Should be either CrcMismatch or an InvalidFormat decompression error
        assert!(
            matches!(result, Err(CoreError::CrcMismatch { .. }))
                || matches!(result, Err(CoreError::BitstreamError { .. }))
                || matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected CrcMismatch, BitstreamError or InvalidFormat, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn encrypted_entry_returns_password_required() {
        // Strategy: write a normal stored ZIP, then flip GP bit 0 in the Central
        // Directory to simulate an encrypted entry — no AES dep needed.
        let content = b"very secret data";
        let (_tmp, _reader) = create_test_zip(&[("secret.txt", content, ZipCrateMethod::Stored)]);

        // Find the CD entry signature [50 4b 01 02] and set GP bit 0 (encrypted).
        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        let cd_sig = [0x50u8, 0x4b, 0x01, 0x02];
        if let Some(pos) = raw.windows(4).position(|w| w == cd_sig) {
            raw[pos + 8] |= 0x01; // GP flags offset = +8 from CD signature
        } else {
            panic!("could not locate Central Directory signature in test ZIP");
        }
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2).await.unwrap();

        assert!(
            zip.entries()[0].is_encrypted,
            "parser must flag entry as encrypted when GP bit 0 is set"
        );

        // Must fail immediately with PasswordRequired — before any decompression attempt.
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(&zip.entries()[0], &mut sink, Arc::new(Mutex::new(())))
            .await;
        assert!(
            matches!(result, Err(CoreError::PasswordRequired)),
            "expected PasswordRequired, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn entries_clean_filters_os_junk() {
        let (_tmp, reader) = create_test_zip(&[
            ("real_file.txt", b"content", ZipCrateMethod::Stored),
            (".DS_Store", b"mac junk", ZipCrateMethod::Stored),
            (
                "__MACOSX/._real_file.txt",
                b"mac sidecar",
                ZipCrateMethod::Stored,
            ),
        ]);

        let zip = ZipArchive::open(reader).await.unwrap();
        assert_eq!(zip.entries().len(), 3, "entries() must return all 3");

        let clean = zip.entries_clean();
        assert_eq!(
            clean.len(),
            1,
            "entries_clean() must filter out 2 junk files"
        );
        assert_eq!(clean[0].name, "real_file.txt");
    }

    #[tokio::test]
    async fn unsupported_method_parses_but_errors_on_extract() {
        // Write a valid stored ZIP, then patch the method ID in both the
        // Central Directory (offset +10) and the Local File Header (offset +8)
        // to 12 (BZip2), which we don't have compiled in.
        let content = b"some data";
        let (_tmp, _reader) = create_test_zip(&[("file.txt", content, ZipCrateMethod::Stored)]);

        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();

        // Patch Local File Header method (sig [50 4b 03 04], method at +8).
        let lfh_sig = [0x50u8, 0x4b, 0x03, 0x04];
        if let Some(pos) = raw.windows(4).position(|w| w == lfh_sig) {
            raw[pos + 8] = 12; // low byte of method ID
            raw[pos + 9] = 0;
        }

        // Patch Central Directory method (sig [50 4b 01 02], method at +10).
        let cd_sig = [0x50u8, 0x4b, 0x01, 0x02];
        if let Some(pos) = raw.windows(4).position(|w| w == cd_sig) {
            raw[pos + 10] = 12;
            raw[pos + 11] = 0;
        }

        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let zip = ZipArchive::open(reader2).await.unwrap();

        // open() must succeed — unknown methods are deferred (not an error at parse time).
        assert_eq!(zip.entries()[0].name, "file.txt");
        assert_eq!(
            zip.entries()[0].compression_method,
            Some(CompressionMethod::BZip2),
            "method should be recognised in the entry metadata"
        );

        // extract() must fail with UnsupportedCompression — not a panic or InvalidFormat.
        let mut sink = tokio::io::sink();
        let result = zip
            .extract(&zip.entries()[0], &mut sink, Arc::new(Mutex::new(())))
            .await;
        assert!(
            matches!(result, Err(CoreError::UnsupportedCompression { .. })),
            "expected UnsupportedCompression, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn chunked_extraction_produces_identical_output() {
        // ~200 KB of repeating data — compresses well with Deflate, and the
        // decompressed output spans at least three 64 KB chunks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();

        let (_tmp, reader) =
            create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader).await.unwrap();
        let data = extract_to_vec(&zip, &zip.entries()[0]).await;

        assert_eq!(data, content, "chunked extraction must be byte-for-byte identical");
    }

    #[tokio::test]
    async fn cancellation_stops_extraction() {
        // Shared state accessed both by the reporter (inside spawn_blocking) and the test.
        #[derive(Default)]
        struct State {
            chunks_seen: u32,
            entry_start_called: bool,
            entry_complete_called: bool,
            complete_success: Option<bool>,
        }

        struct CancelAfterFirstChunk {
            state: Arc<Mutex<State>>,
        }

        impl ProgressReporter for CancelAfterFirstChunk {
            fn on_entry_start(&mut self, _name: &str, _size: u64) {
                self.state.lock().unwrap().entry_start_called = true;
            }
            fn on_bytes_progress(&mut self, _bytes_so_far: u64) {
                self.state.lock().unwrap().chunks_seen += 1;
            }
            fn on_entry_complete(&mut self, _name: &str, success: bool) {
                let mut s = self.state.lock().unwrap();
                s.entry_complete_called = true;
                s.complete_success = Some(success);
            }
            fn should_cancel(&self) -> bool {
                self.state.lock().unwrap().chunks_seen >= 1
            }
        }

        // ~200 KB — enough to span multiple 64 KB chunks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();

        let (_tmp, reader) =
            create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader).await.unwrap();

        let state = Arc::new(Mutex::new(State::default()));
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(
            CancelAfterFirstChunk { state: Arc::clone(&state) },
        ));

        let mut sink = tokio::io::sink();
        let result = zip
            .extract(&zip.entries()[0], &mut sink, progress)
            .await;

        assert!(
            matches!(result, Err(CoreError::Cancelled)),
            "expected Cancelled, got: {result:?}"
        );

        let s = state.lock().unwrap();
        assert!(s.entry_start_called, "on_entry_start must be called");
        assert!(s.entry_complete_called, "on_entry_complete must be called on cancellation");
        assert_eq!(
            s.complete_success,
            Some(false),
            "on_entry_complete must report success=false on cancellation"
        );
    }

    #[tokio::test]
    async fn progress_reports_bytes() {
        #[derive(Default)]
        struct State {
            reported: Vec<u64>,
        }

        struct ByteRecorder {
            state: Arc<Mutex<State>>,
        }

        impl ProgressReporter for ByteRecorder {
            fn on_bytes_progress(&mut self, bytes_so_far: u64) {
                self.state.lock().unwrap().reported.push(bytes_so_far);
            }
        }

        // ~200 KB — produces multiple progress callbacks.
        let content: Vec<u8> = (0u32..)
            .flat_map(|i| i.to_le_bytes())
            .take(200 * 1024)
            .collect();
        let uncompressed_size = content.len() as u64;

        let (_tmp, reader) =
            create_test_zip(&[("large.bin", &content, ZipCrateMethod::Deflated)]);
        let zip = ZipArchive::open(reader).await.unwrap();

        let state = Arc::new(Mutex::new(State::default()));
        let progress: Arc<Mutex<dyn ProgressReporter>> =
            Arc::new(Mutex::new(ByteRecorder { state: Arc::clone(&state) }));

        let mut sink = tokio::io::sink();
        zip.extract(&zip.entries()[0], &mut sink, progress)
            .await
            .unwrap();

        let s = state.lock().unwrap();
        assert!(
            s.reported.len() > 1,
            "expected multiple progress callbacks, got {}",
            s.reported.len()
        );
        assert_eq!(
            *s.reported.last().unwrap(),
            uncompressed_size,
            "final progress value must equal uncompressed_size"
        );
        // Values must be strictly increasing.
        for w in s.reported.windows(2) {
            assert!(w[1] > w[0], "progress values must be strictly increasing");
        }
    }

    #[tokio::test]
    async fn truncated_zip_returns_invalid_format() {
        // Write a real ZIP then truncate it to just 10 bytes — far too short
        // to contain any valid EOCD record. open() must return an error, not panic.
        let (_tmp, _reader) = create_test_zip(&[("file.txt", b"hello", ZipCrateMethod::Stored)]);

        let path = _tmp.path().to_owned();
        let mut raw = std::fs::read(&path).unwrap();
        raw.truncate(10);
        std::fs::write(&path, &raw).unwrap();

        let reader2 = LocalReadAt::new(&path);
        let result = ZipArchive::open(reader2).await;

        assert!(
            matches!(result, Err(CoreError::InvalidFormat { .. })),
            "expected InvalidFormat for truncated ZIP, got: {:?}",
            result.err()
        );
    }
}
