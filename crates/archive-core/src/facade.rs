//! High-level entry point for opening and working with archives.
//!
//! ```ignore
//! let archive = ArchiveCore::open("https://example.com/data.zip").await?;
//! for entry in archive.entries() {
//!     println!("{}", entry.name);
//! }
//! let data = archive.extract_to_bytes(&archive.entries()[0]).await?;
//! ```

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::AsyncWrite;

use crate::io::{ReadAt, Source};
use crate::junk;
use crate::progress::ProgressReporter;
use crate::rar::parse::{RAR4_SIGNATURE, RAR5_SIGNATURE};
use crate::rar::RarArchive;
use crate::zip::ZipArchive;
use crate::{ArchiveEntry, CoreError};

/// A format-agnostic archive handle.
///
/// Created via [`ArchiveCore::open`], which auto-detects the source type
/// (local file or HTTP URL) and the archive format (ZIP, RAR, 7z, ...)./
///
/// This is the primary public API for the core layer. The state layer
/// (`archive`) wraps this type — products should interact with
/// `archive::Archive`, not this type directly.
pub struct ArchiveCore {
    inner: ArchiveInner,
}

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `ArchiveCore`.
enum ArchiveInner {
    Zip(ZipArchive<Source>),
    Rar(RarArchive<Source>),
    // Future: SevenZip(...), Tar(...)
}

impl ArchiveCore {
    /// Open an archive from a local path or URL.
    ///
    /// Auto-detects:
    /// - **Source:** local file (any path) or HTTP URL (`http://` / `https://`)
    /// - **Format:** currently ZIP only (by attempting to parse); more formats coming
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Local file
    /// let archive = ArchiveCore::open("/path/to/file.zip").await?;
    ///
    /// // Cloud file (Dropbox, Drive, etc.)
    /// let archive = ArchiveCore::open("https://dropbox.com/...?dl=1").await?;
    /// ```
    pub async fn open(source: &str) -> Result<Self, CoreError> {
        let reader = Source::detect(source);

        // Sniff the first 8 bytes to select the right parser.
        let magic = reader.read_range(0, 8).await?;

        // RAR5?
        if magic.len() >= 8 && magic[..8] == RAR5_SIGNATURE {
            let rar = RarArchive::open(reader).await?;
            return Ok(Self { inner: ArchiveInner::Rar(rar) });
        }

        // RAR4? — detect and reject with a clear error.
        if magic.len() >= 7 && magic[..7] == RAR4_SIGNATURE {
            return Err(CoreError::UnsupportedFormat {
                format: "RAR4 archives are not yet supported (RAR5 only)".into(),
            });
        }

        // Fall back to ZIP (reads EOCD from the end of the file).
        let zip = ZipArchive::open(reader).await?;
        Ok(Self { inner: ArchiveInner::Zip(zip) })
    }

    /// Return all entries (files and directories) in the archive, including
    /// OS-generated junk (`.DS_Store`, `__MACOSX/`, `Thumbs.db`, etc.).
    ///
    /// Prefer [`entries_clean`](Self::entries_clean) for user-facing output.
    pub fn entries(&self) -> &[ArchiveEntry] {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries(),
            ArchiveInner::Rar(r) => r.entries(),
        }
    }

    /// Return entries with OS-generated junk files filtered out.
    ///
    /// This is the **default** for user-facing output (CLI, UI). Filters:
    /// - macOS: `__MACOSX/`, `._*` sidecars, `.DS_Store`
    /// - Windows: `Thumbs.db`, `desktop.ini`, `ehthumbs.db`  
    /// - Linux/KDE: `.directory`
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries()
            .iter()
            .filter(|e| !junk::is_junk(e))
            .collect()
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written. CRC verification is mandatory.
    ///
    /// For convenience, see [`extract_to_bytes`](Self::extract_to_bytes).
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, CoreError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry, sink, progress, password).await,
            // RAR extraction implemented in Step 5-6.
            ArchiveInner::Rar(_) => Err(CoreError::UnsupportedFeature {
                detail: "RAR extraction not yet implemented".into(),
            }),
        }
    }

    /// Extract a single entry into memory, returning decompressed bytes.
    ///
    /// Convenience wrapper around [`extract`](Self::extract) for small entries.
    /// For large entries (multi-GB), use `extract()` with a file sink instead.
    pub async fn extract_to_bytes(
        &self,
        entry: &ArchiveEntry,
        password: Option<&[u8]>,
    ) -> Result<Bytes, CoreError> {
        let mut buf = Vec::with_capacity(entry.uncompressed_size as usize);
        let progress = Arc::new(Mutex::new(()));
        self.extract(entry, &mut buf, progress, password).await?;
        Ok(Bytes::from(buf))
    }

    /// Bytes that must be downloaded and decompressed to produce this entry.
    ///
    /// Used by the "extraction tax" UX for cloud sniping cost warnings.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extraction_cost(entry),
            // RAR extraction_cost implemented in Step 5-6.
            ArchiveInner::Rar(_) => entry.compressed_size,
        }
    }

    /// Decompress and verify CRC without retaining output.
    ///
    /// Used for archive integrity checking.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry).await,
            // RAR test implemented in Step 5-6.
            ArchiveInner::Rar(_) => Err(CoreError::UnsupportedFeature {
                detail: "RAR test not yet implemented".into(),
            }),
        }
    }

}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::rar::parse::RAR4_SIGNATURE;
    use crate::{ArchiveCore, CoreError};

    /// Write bytes to a temp file and return the file + path string.
    fn write_temp(bytes: &[u8]) -> (tempfile::NamedTempFile, String) {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        let path = f.path().to_str().unwrap().to_owned();
        (f, path)
    }

    #[tokio::test]
    async fn open_rar5_detects_rar_format() {
        let bytes = crate::rar::build_minimal_rar5_for_test("hello.txt", b"hello", false);
        let (_f, path) = write_temp(&bytes);
        let archive = ArchiveCore::open(&path).await.unwrap();
        assert_eq!(archive.entries().len(), 1);
        assert_eq!(archive.entries()[0].name, "hello.txt");
    }

    #[tokio::test]
    async fn open_rar4_returns_unsupported_format() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&RAR4_SIGNATURE);
        bytes.extend_from_slice(&[0u8; 64]);
        let (_f, path) = write_temp(&bytes);
        let result = ArchiveCore::open(&path).await;
        assert!(
            matches!(result, Err(CoreError::UnsupportedFormat { .. })),
            "expected UnsupportedFormat"
        );
    }

    #[tokio::test]
    async fn open_zip_still_works_after_rar_added() {
        use std::io::Write as _;
        // Build a minimal valid ZIP in memory.
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file("hello.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"hello").unwrap();
            zw.finish().unwrap();
        }
        let (_f, path) = write_temp(&buf);
        let archive = ArchiveCore::open(&path).await.unwrap();
        assert!(!archive.entries().is_empty());
        assert_eq!(archive.entries()[0].name, "hello.txt");
    }

    #[tokio::test]
    async fn source_local_path_returns_path_for_local() {
        use crate::io::Source;
        let source = Source::detect("/tmp/test.rar");
        assert!(source.local_path().is_some());
    }

    #[tokio::test]
    async fn source_local_path_returns_none_for_http() {
        use crate::io::Source;
        let source = Source::detect("https://example.com/test.rar");
        assert!(source.local_path().is_none());
    }

    #[tokio::test]
    async fn open_unknown_bytes_falls_through_to_zip_error() {
        let bytes = vec![0u8; 64];
        let (_f, path) = write_temp(&bytes);
        let result = ArchiveCore::open(&path).await;
        // Zero bytes should NOT trigger the RAR4 detection path.
        assert!(
            !matches!(result, Err(CoreError::UnsupportedFormat { .. })),
            "zero bytes should not trigger RAR4 detection"
        );
        assert!(result.is_err(), "unknown bytes should produce an error");
    }
}
