use std::sync::{Arc, Mutex};

use archive_core::{
    ArchiveEntry, ArchiveReader, CoreError, ProgressReporter, Rar4Archive, RarArchive, ReadAt,
    ScanInfo, Source, ZipArchive, RAR4_SIGNATURE, RAR5_SIGNATURE,
};
use bytes::Bytes;
use tokio::io::AsyncWrite;
use tracing::{debug, info};

use crate::cache::{
    cache_path_for, ArchiveCache, ArchiveFingerprint, FormatCacheState,
};
use crate::entry_tree::EntryTree;
use crate::error::{translate_core_error, ArchiveError};
use crate::opened::{Opened, PendingArchive};

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `Archive`.
pub(crate) enum ArchiveInner {
    Zip(ZipArchive<Source>),
    Rar(RarArchive<Source>),
    Rar4(Rar4Archive<Source>),
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
            ArchiveInner::Rar4(_) => "Rar4",
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
    ///
    /// Transparently checks the cache before invoking any format parser. On a valid
    /// cache hit, returns [`Opened::Ready`] immediately without any I/O scan.
    pub async fn open(source: &str) -> Result<Opened, ArchiveError> {
        info!(source = %source, "Archive::open");
        let reader = Source::detect(source);

        // Sniff the first 8 bytes to select the right parser.
        let magic = reader
            .read_range(0, 8)
            .await
            .map_err(|e| translate_core_error(CoreError::from(e)))?;
        debug!(magic = ?&magic[..magic.len().min(8)], "magic bytes read");

        // ── Implicit cache check ──────────────────────────────────────────────
        let source_size = reader
            .size()
            .await
            .map_err(|e| translate_core_error(CoreError::from(e)))?;

        let mtime = std::fs::metadata(source)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        let fingerprint = ArchiveFingerprint {
            source: source.to_string(),
            source_size,
            etag: None,
            last_modified: None,
            mtime,
        };

        if let Some(cache_path) = cache_path_for(source)
            && let Ok(cache) = ArchiveCache::load(&cache_path)
            && cache.is_valid(&fingerprint)
        {
            info!(entries = cache.entries.len(), "cache hit — skipping parse");
            let archive = Self::from_cache(cache, reader);
            return Ok(Opened::Ready(archive));
        }
        debug!("cache miss — parsing from source");
        // ── End cache check ───────────────────────────────────────────────────

        // RAR5?
        if magic.len() >= 8 && magic[..8] == RAR5_SIGNATURE {
            info!("format detected: RAR5");
            let rar = RarArchive::open(reader).await.map_err(translate_core_error)?;
            let info = rar.scan_info();
            return if matches!(info, ScanInfo::Ready) {
                let archive = Self { inner: ArchiveInner::Rar(rar) };
                // Save cache for local files (already scanned during open).
                if let Some(cache_path) = cache_path_for(source) {
                    let _ = archive.save_cache(&fingerprint, &cache_path);
                }
                Ok(Opened::Ready(archive))
            } else {
                Ok(Opened::NeedsScan(PendingArchive::new(
                    ArchiveInner::Rar(rar),
                    info,
                    fingerprint,
                    source.to_string(),
                )))
            };
        }

        // RAR4?
        if magic.len() >= 7 && magic[..7] == RAR4_SIGNATURE {
            info!("format detected: RAR4");
            let rar4 = Rar4Archive::open(reader).await.map_err(translate_core_error)?;
            let info = rar4.scan_info();
            return if matches!(info, ScanInfo::Ready) {
                let archive = Self { inner: ArchiveInner::Rar4(rar4) };
                // Save cache for local files (already scanned during open).
                if let Some(cache_path) = cache_path_for(source) {
                    let _ = archive.save_cache(&fingerprint, &cache_path);
                }
                Ok(Opened::Ready(archive))
            } else {
                Ok(Opened::NeedsScan(PendingArchive::new(
                    ArchiveInner::Rar4(rar4),
                    info,
                    fingerprint,
                    source.to_string(),
                )))
            };
        }

        // Fall back to ZIP (reads EOCD from the end of the file).
        info!("format detected: ZIP (fallback)");
        let zip = ZipArchive::open(reader).await.map_err(translate_core_error)?;
        let archive = Self { inner: ArchiveInner::Zip(zip) };
        // Save cache for ZIP (listing is fast but caching saves 2 range requests on reopen).
        if let Some(cache_path) = cache_path_for(source) {
            let _ = archive.save_cache(&fingerprint, &cache_path);
        }
        Ok(Opened::Ready(archive))
    }

    /// Reconstruct an archive from a cache hit without any I/O scan.
    fn from_cache(cache: ArchiveCache, reader: Source) -> Self {
        let local_path = reader.local_path().map(|p| p.to_path_buf());
        match cache.format_state {
            FormatCacheState::Zip { locations } => {
                let zip = ZipArchive::from_cached(reader, cache.entries, locations);
                Self { inner: ArchiveInner::Zip(zip) }
            }
            FormatCacheState::Rar { locations, archive_preamble, end_block, archive_size } => {
                let rar = RarArchive::from_cached(
                    reader,
                    cache.entries,
                    locations,
                    Bytes::from(archive_preamble),
                    Bytes::from(end_block),
                    local_path,
                    archive_size,
                );
                Self { inner: ArchiveInner::Rar(rar) }
            }
            FormatCacheState::Rar4 { locations, archive_preamble, end_block, archive_size } => {
                let rar4 = Rar4Archive::from_cached(
                    reader,
                    cache.entries,
                    locations,
                    Bytes::from(archive_preamble),
                    Bytes::from(end_block),
                    local_path,
                    archive_size,
                );
                Self { inner: ArchiveInner::Rar4(rar4) }
            }
        }
    }

    /// Serialize current state to a cache file. Failure is silently ignored by callers.
    pub(crate) fn save_cache(
        &self,
        fingerprint: &ArchiveFingerprint,
        path: &std::path::Path,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let format_state = match &self.inner {
            ArchiveInner::Zip(z) => FormatCacheState::Zip {
                locations: z.locations().to_vec(),
            },
            ArchiveInner::Rar(r) => FormatCacheState::Rar {
                locations: r.locations().to_vec(),
                archive_preamble: r.archive_preamble_bytes().to_vec(),
                end_block: r.end_block_bytes().to_vec(),
                archive_size: r.archive_size(),
            },
            ArchiveInner::Rar4(r) => FormatCacheState::Rar4 {
                locations: r.locations().to_vec(),
                archive_preamble: r.archive_preamble_bytes().to_vec(),
                end_block: r.end_block_bytes().to_vec(),
                archive_size: r.archive_size(),
            },
        };

        let cache = ArchiveCache {
            fingerprint: fingerprint.clone(),
            entries: self.entries().to_vec(),
            format_state,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };

        cache.save(path)
    }

    /// All entries in the archive including OS-generated junk.
    pub fn entries(&self) -> &[ArchiveEntry] {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries(),
            ArchiveInner::Rar(r) => r.entries(),
            ArchiveInner::Rar4(r) => r.entries(),
        }
    }

    /// Entries with OS-generated junk files filtered out.
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries_clean(),
            ArchiveInner::Rar(r) => r.entries_clean(),
            ArchiveInner::Rar4(r) => r.entries_clean(),
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
        debug!(name = %entry.name, "Archive::extract delegating to format parser");
        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry, sink, progress, password).await,
            ArchiveInner::Rar(r) => r.extract(entry, sink, progress, password).await,
            ArchiveInner::Rar4(r) => r.extract(entry, sink, progress, password).await,
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
            ArchiveInner::Rar4(r) => r.extraction_cost(entry),
        }
    }

    /// Decompress and verify CRC without retaining output.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry).await,
            ArchiveInner::Rar(r) => r.test(entry).await,
            ArchiveInner::Rar4(r) => r.test(entry).await,
        }
        .map_err(translate_core_error)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use super::*;

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
    async fn open_rar4_detects_rar4_format() {
        let archive = Archive::open(&rar_fixture("rar4_sample.rar"))
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();
        assert!(!archive.entries().is_empty());
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

    // ── RAR4 facade tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn open_rar4_returns_ready() {
        // RAR4 always scans during open — never NeedsScan.
        let opened = Archive::open(&rar_fixture("rar4_sample.rar")).await.unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    // ── Cache integration tests ────────────────────────────────────────────

    #[tokio::test]
    async fn zip_cache_round_trip() {
        use crate::cache::ArchiveCache;

        let buf = build_zip(&[("hello.txt", b"world data")]);
        let (_f, path) = write_temp(&buf);

        let cache_file = tempfile::NamedTempFile::new().unwrap();

        // Open and get the archive.
        let archive = Archive::open(&path).await.unwrap().ready().await.unwrap();
        let entry_count = archive.entries().len();
        assert!(entry_count > 0);

        // Manually save cache to our temp file.
        let fingerprint = crate::cache::ArchiveFingerprint {
            source: path.clone(),
            source_size: std::fs::metadata(&path).unwrap().len(),
            etag: None,
            last_modified: None,
            mtime: None,
        };
        archive.save_cache(&fingerprint, cache_file.path()).unwrap();

        // Load and verify round-trip.
        let cached = ArchiveCache::load(cache_file.path()).unwrap();
        assert_eq!(cached.entries.len(), entry_count);
        assert!(matches!(cached.format_state, crate::cache::FormatCacheState::Zip { .. }));
    }

    #[tokio::test]
    async fn rar5_implicit_cache_open_reopen() {
        let cache_dir = tempfile::TempDir::new().unwrap();
        let fixture_path = rar_fixture("rar5_store.rar");

        // First open: scans and saves cache.
        let archive = Archive::open(&fixture_path).await.unwrap().ready().await.unwrap();
        let expected_entries: Vec<String> = archive.entries().iter().map(|e| e.name.clone()).collect();
        assert!(!expected_entries.is_empty());

        // Manually save to our isolated temp dir.
        let fingerprint = crate::cache::ArchiveFingerprint {
            source: fixture_path.clone(),
            source_size: std::fs::metadata(&fixture_path).unwrap().len(),
            etag: None,
            last_modified: None,
            mtime: std::fs::metadata(&fixture_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        };
        let cache_path = cache_dir.path().join("test.json");
        archive.save_cache(&fingerprint, &cache_path).unwrap();

        // Load cache and reconstruct archive via from_cache.
        let cached = crate::cache::ArchiveCache::load(&cache_path).unwrap();
        assert!(cached.is_valid(&fingerprint));

        let reader = Source::detect(&fixture_path);
        let restored = Archive::from_cache(cached, reader);

        let restored_entries: Vec<String> = restored.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(restored_entries, expected_entries);
    }

    #[tokio::test]
    async fn rar4_implicit_cache_open_reopen() {
        let fixture_path = rar_fixture("rar4_sample.rar");

        let archive = Archive::open(&fixture_path).await.unwrap().ready().await.unwrap();
        let expected_entries: Vec<String> = archive.entries().iter().map(|e| e.name.clone()).collect();
        assert!(!expected_entries.is_empty());

        let fingerprint = crate::cache::ArchiveFingerprint {
            source: fixture_path.clone(),
            source_size: std::fs::metadata(&fixture_path).unwrap().len(),
            etag: None,
            last_modified: None,
            mtime: std::fs::metadata(&fixture_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        };
        let cache_file = tempfile::NamedTempFile::new().unwrap();
        archive.save_cache(&fingerprint, cache_file.path()).unwrap();

        let cached = crate::cache::ArchiveCache::load(cache_file.path()).unwrap();
        assert!(cached.is_valid(&fingerprint));

        let reader = Source::detect(&fixture_path);
        let restored = Archive::from_cache(cached, reader);

        let restored_entries: Vec<String> = restored.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(restored_entries, expected_entries);
    }

    #[tokio::test]
    async fn corrupted_cache_falls_through_to_normal_open() {
        let cache_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(cache_file.path(), b"{{not valid json}}}").unwrap();

        // ArchiveCache::load should fail on corrupted JSON.
        let result = crate::cache::ArchiveCache::load(cache_file.path());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cache_wrong_size_is_invalid() {
        use crate::cache::{ArchiveCache, ArchiveFingerprint, FormatCacheState};

        let fp = ArchiveFingerprint {
            source: "test.rar".into(),
            source_size: 1000,
            etag: None,
            last_modified: None,
            mtime: None,
        };
        let cache = ArchiveCache {
            fingerprint: fp,
            entries: vec![],
            format_state: FormatCacheState::Zip { locations: vec![] },
            created_at: 0,
        };

        let different_fp = ArchiveFingerprint {
            source: "test.rar".into(),
            source_size: 9999, // different size
            etag: None,
            last_modified: None,
            mtime: None,
        };
        assert!(!cache.is_valid(&different_fp));
    }
}
