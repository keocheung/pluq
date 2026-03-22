use std::sync::{Arc, Mutex};

use archive_core::{
    ArchiveEntry, ArchiveReader, CoreError, ProgressReporter, RarArchive, ReadAt, ScanInfo, Source,
    ZipArchive, RAR4_SIGNATURE, RAR5_SIGNATURE,
};
use bytes::Bytes;
use tokio::io::AsyncWrite;

use crate::entry_tree::EntryTree;
use crate::error::{translate_core_error, ArchiveError};
use crate::opened::{Opened, PendingArchive};

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `Archive`.
pub(crate) enum ArchiveInner {
    Zip(ZipArchive<Source>),
    Rar(RarArchive<Source>),
}

/// Product-facing handle to an open archive.
///
/// Created via [`Archive::open`], which auto-detects the source type
/// (local file or HTTP URL) and the archive format (ZIP, RAR, ...).
///
/// `open()` returns [`Opened`] — either immediately usable ([`Opened::Ready`])
/// or requiring a metadata scan first ([`Opened::NeedsScan`]).
pub struct Archive {
    pub(crate) inner: ArchiveInner,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let format = match &self.inner {
            ArchiveInner::Zip(_) => "Zip",
            ArchiveInner::Rar(_) => "Rar",
        };
        f.debug_struct("Archive")
            .field("format", &format)
            .finish()
    }
}

impl Archive {
    /// Open an archive from a local path or URL.
    ///
    /// Returns [`Opened::Ready`] if the archive is immediately usable, or
    /// [`Opened::NeedsScan`] if a metadata scan is required first (e.g. RAR over HTTP
    /// without a Quick Open block).
    pub async fn open(source: &str) -> Result<Opened, ArchiveError> {
        let reader = Source::detect(source);

        // Sniff the first 8 bytes to select the right parser.
        let magic = reader
            .read_range(0, 8)
            .await
            .map_err(|e| translate_core_error(CoreError::from(e)))?;

        // RAR5?
        if magic.len() >= 8 && magic[..8] == RAR5_SIGNATURE {
            let rar = RarArchive::open(reader).await.map_err(translate_core_error)?;
            let info = rar.scan_info();
            return if matches!(info, ScanInfo::Ready) {
                Ok(Opened::Ready(Self { inner: ArchiveInner::Rar(rar) }))
            } else {
                Ok(Opened::NeedsScan(PendingArchive::new(ArchiveInner::Rar(rar), info)))
            };
        }

        // RAR4? — detect and reject with a clear error.
        if magic.len() >= 7 && magic[..7] == RAR4_SIGNATURE {
            return Err(translate_core_error(CoreError::UnsupportedFormat {
                format: "RAR4 archives are not yet supported (RAR5 only)".into(),
            }));
        }

        // Fall back to ZIP (reads EOCD from the end of the file).
        let zip = ZipArchive::open(reader).await.map_err(translate_core_error)?;
        Ok(Opened::Ready(Self { inner: ArchiveInner::Zip(zip) }))
    }

    /// All entries in the archive including OS-generated junk.
    pub fn entries(&self) -> &[ArchiveEntry] {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries(),
            ArchiveInner::Rar(r) => r.entries(),
        }
    }

    /// Entries with OS-generated junk files filtered out.
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries_clean(),
            ArchiveInner::Rar(r) => r.entries_clean(),
        }
    }

    /// Build a virtual folder tree from the flat entry list.
    pub fn entry_tree(&self) -> EntryTree {
        EntryTree::build(self.entries())
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written.
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, ArchiveError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry, sink, progress, password).await,
            ArchiveInner::Rar(r) => r.extract(entry, sink, progress, password).await,
        }
        .map_err(translate_core_error)
    }

    /// Extract a single entry into memory.
    ///
    /// Convenience wrapper around [`extract`](Self::extract) for small entries.
    /// For large entries, use `extract()` with a file sink instead.
    pub async fn extract_to_bytes(
        &self,
        entry: &ArchiveEntry,
        password: Option<&[u8]>,
    ) -> Result<Bytes, ArchiveError> {
        let mut buf = Vec::with_capacity(entry.uncompressed_size as usize);
        let progress = Arc::new(Mutex::new(()));
        self.extract(entry, &mut buf, progress, password).await?;
        Ok(Bytes::from(buf))
    }

    /// Bytes that must be fetched and decompressed to produce this entry.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extraction_cost(entry),
            ArchiveInner::Rar(r) => r.extraction_cost(entry),
        }
    }

    /// Decompress and verify CRC without retaining output.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry).await,
            ArchiveInner::Rar(r) => r.test(entry).await,
        }
        .map_err(translate_core_error)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use archive_core::RAR4_SIGNATURE;

    use super::*;
    use crate::error::ArchiveError;

    fn write_temp(bytes: &[u8]) -> (tempfile::NamedTempFile, String) {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        let path = f.path().to_str().unwrap().to_owned();
        (f, path)
    }

    fn rar_fixture(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("archive-core/tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (name, data) in entries {
            zw.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf
    }

    // ── Moved from facade.rs ───────────────────────────────────────────────

    #[tokio::test]
    async fn open_rar5_detects_rar_format() {
        let archive = Archive::open(&rar_fixture("rar5_store.rar"))
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();
        assert!(!archive.entries().is_empty());
        assert_eq!(archive.entries()[0].name, "file1.txt");
    }

    #[tokio::test]
    async fn open_rar4_returns_unsupported_format() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&RAR4_SIGNATURE);
        bytes.extend_from_slice(&[0u8; 64]);
        let (_f, path) = write_temp(&bytes);
        let result = Archive::open(&path).await;
        assert!(
            matches!(result, Err(ArchiveError::UnsupportedFormat { .. })),
            "expected UnsupportedFormat"
        );
    }

    #[tokio::test]
    async fn open_zip_still_works_after_rar_added() {
        let buf = build_zip(&[("hello.txt", b"hello")]);
        let (_f, path) = write_temp(&buf);
        let archive = Archive::open(&path).await.unwrap().ready().await.unwrap();
        assert!(!archive.entries().is_empty());
        assert_eq!(archive.entries()[0].name, "hello.txt");
    }

    #[tokio::test]
    async fn open_unknown_bytes_produces_error() {
        // Zero bytes are not a valid archive — must fail without panicking.
        let bytes = vec![0u8; 64];
        let (_f, path) = write_temp(&bytes);
        let result = Archive::open(&path).await;
        assert!(result.is_err(), "unknown bytes should produce an error");
    }

    // ── New: Opened variant tests ──────────────────────────────────────────

    #[tokio::test]
    async fn open_zip_returns_ready() {
        let buf = build_zip(&[("hello.txt", b"hello")]);
        let (_f, path) = write_temp(&buf);
        let opened = Archive::open(&path).await.unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    #[tokio::test]
    async fn open_rar5_local_returns_ready() {
        // Local RAR5: sequential scan runs during open → always Ready.
        let opened = Archive::open(&rar_fixture("rar5_store.rar")).await.unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    #[tokio::test]
    async fn opened_ready_convenience_works() {
        let archive = Archive::open(&rar_fixture("rar5_store.rar"))
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();
        assert!(!archive.entries().is_empty());
    }
}
