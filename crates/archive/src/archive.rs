use std::sync::{Arc, Mutex};

use archive_core::{
    ArchiveEntry, ArchiveReader, BatchResult, CoreError, LocalPath, ProgressReporter,
    RAR4_SIGNATURE, RAR5_SIGNATURE, Rar4Archive, RarArchive, ReadAt, ScanInfo, ZipArchive,
};
use bytes::Bytes;
use tokio::io::AsyncWrite;
use tracing::{debug, info};

use crate::config::ArchiveConfig;
use crate::entry_tree::EntryTree;
use crate::error::{ArchiveError, translate_core_error};
use crate::opened::{Opened, PendingArchive};

use std::pin::Pin;
use std::task::{Context, Poll};

struct LimitedBuf<'a> {
    inner: &'a mut Vec<u8>,
    cap: usize,
}

impl<'a> AsyncWrite for LimitedBuf<'a> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if this.inner.len() + buf.len() > this.cap {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Symlink target too large (exceeds 4 KiB)",
            )));
        }
        this.inner.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `Archive`.
pub(crate) enum ArchiveInner<R: ReadAt + LocalPath + 'static> {
    Zip(ZipArchive<R>),
    Rar(RarArchive<R>),
    Rar4(Rar4Archive<R>),
}

/// Product-facing handle to an open archive.
///
/// Created via [`Archive::open`], which auto-detects the source type
/// (local file or HTTP URL) and the archive format (ZIP, RAR, ...).
///
/// `open()` returns [`Opened`] — either immediately usable ([`Opened::Ready`])
/// or requiring a metadata scan first ([`Opened::NeedsScan`]).
pub struct Archive<R: ReadAt + LocalPath + 'static> {
    pub(crate) inner: ArchiveInner<R>,
    pub(crate) config: ArchiveConfig,
}

impl<R: ReadAt + LocalPath + 'static> std::fmt::Debug for Archive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let format = match &self.inner {
            ArchiveInner::Zip(_) => "Zip",
            ArchiveInner::Rar(_) => "Rar",
            ArchiveInner::Rar4(_) => "Rar4",
        };
        f.debug_struct("Archive").field("format", &format).finish()
    }
}

impl<R: ReadAt + LocalPath + 'static> Archive<R> {
    /// Open an archive from an externally-built reader.
    ///
    /// Use this when a product layer needs to inject a decorated reader (e.g. a
    /// progress-reporting wrapper, Google Drive, Dropbox).
    ///
    /// `source_label` is the user-facing string used for cache keying and tracing —
    /// it is **not** resolved as a filesystem path.
    pub async fn open_reader_uncached(
        reader: R,
        config: &ArchiveConfig,
    ) -> Result<Opened<R>, ArchiveError> {
        info!("Archive::open_reader_uncached");

        // Sniff the first 8 bytes to select the right parser.
        let magic = reader
            .read_range(0, 8)
            .await
            .map_err(|e| translate_core_error(CoreError::from(e)))?;
        debug!(magic = ?&magic[..magic.len().min(8)], "magic bytes read");

        // RAR5?
        if magic.len() >= 8 && magic[..8] == RAR5_SIGNATURE {
            info!("format detected: RAR5");
            let rar = RarArchive::open(reader, &config.limits)
                .await
                .map_err(translate_core_error)?;
            let info = rar.scan_info();
            return if matches!(info, ScanInfo::Ready) {
                let archive = Self {
                    inner: ArchiveInner::Rar(rar),
                    config: config.clone(),
                };
                Ok(Opened::Ready(archive))
            } else {
                Ok(Opened::NeedsScan(PendingArchive::new(
                    ArchiveInner::Rar(rar),
                    info,
                    config.clone(),
                )))
            };
        }

        // RAR4?
        if magic.len() >= 7 && magic[..7] == RAR4_SIGNATURE {
            info!("format detected: RAR4");
            let rar4 = Rar4Archive::open(reader, &config.limits)
                .await
                .map_err(translate_core_error)?;
            let info = rar4.scan_info();
            return if matches!(info, ScanInfo::Ready) {
                let archive = Self {
                    inner: ArchiveInner::Rar4(rar4),
                    config: config.clone(),
                };
                Ok(Opened::Ready(archive))
            } else {
                Ok(Opened::NeedsScan(PendingArchive::new(
                    ArchiveInner::Rar4(rar4),
                    info,
                    config.clone(),
                )))
            };
        }

        // Fall back to ZIP (reads EOCD from the end of the file).
        info!("format detected: ZIP (fallback)");
        let zip = ZipArchive::open(reader, &config.limits)
            .await
            .map_err(translate_core_error)?;
        let archive = Self {
            inner: ArchiveInner::Zip(zip),
            config: config.clone(),
        };
        Ok(Opened::Ready(archive))
    }

    pub fn format_cache_state(&self) -> archive_core::FormatCacheState {
        match &self.inner {
            ArchiveInner::Zip(z) => archive_core::FormatCacheState::Zip {
                locations: z.locations().to_vec(),
            },
            ArchiveInner::Rar(r) => archive_core::FormatCacheState::Rar {
                locations: r.locations().to_vec(),
                archive_preamble: r.archive_preamble_bytes().to_vec(),
                end_block: r.end_block_bytes().to_vec(),
                archive_size: r.archive_size(),
            },
            ArchiveInner::Rar4(r) => archive_core::FormatCacheState::Rar4 {
                locations: r.locations().to_vec(),
                archive_preamble: r.archive_preamble_bytes().to_vec(),
                end_block: r.end_block_bytes().to_vec(),
                archive_size: r.archive_size(),
            },
        }
    }

    pub fn from_cache(
        format_state: archive_core::FormatCacheState,
        entries: Vec<ArchiveEntry>,
        reader: R,
        config: ArchiveConfig,
    ) -> Self {
        let local_path = reader.local_path().map(|p| p.to_path_buf());
        let inner = match format_state {
            archive_core::FormatCacheState::Zip { locations } => {
                ArchiveInner::Zip(ZipArchive::from_cached(reader, entries, locations))
            }
            archive_core::FormatCacheState::Rar {
                locations,
                archive_preamble,
                end_block,
                archive_size,
            } => ArchiveInner::Rar(RarArchive::from_cached(
                reader,
                entries,
                locations,
                Bytes::from(archive_preamble),
                Bytes::from(end_block),
                local_path,
                archive_size,
            )),
            archive_core::FormatCacheState::Rar4 {
                locations,
                archive_preamble,
                end_block,
                archive_size,
            } => ArchiveInner::Rar4(Rar4Archive::from_cached(
                reader,
                entries,
                locations,
                Bytes::from(archive_preamble),
                Bytes::from(end_block),
                local_path,
                archive_size,
            )),
        };

        Self { inner, config }
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
        let limits = &self.config.limits;

        match archive_core::evaluate_path(&entry.name, self.config.path_policy) {
            archive_core::PathDecision::Reject { path } => {
                return Err(translate_core_error(CoreError::PathTraversal { path }));
            }
            archive_core::PathDecision::Rewrite {
                sanitized: _,
                original,
            } => {
                if self.config.path_policy == archive_core::PathPolicy::Strict {
                    return Err(translate_core_error(CoreError::PathTraversal {
                        path: original,
                    }));
                } else {
                    progress
                        .lock()
                        .unwrap()
                        .on_warning(&format!("Extracted potentially unsafe path: {}", original));
                }
            }
            archive_core::PathDecision::Allow => {}
        }

        if entry.is_symlink {
            let mut buf = Vec::with_capacity(256);
            let mut bounded = LimitedBuf {
                inner: &mut buf,
                cap: 4096,
            };
            let bytes = match &self.inner {
                ArchiveInner::Zip(z) => {
                    z.extract(entry, &mut bounded, progress.clone(), password, limits)
                        .await
                }
                ArchiveInner::Rar(r) => {
                    r.extract(entry, &mut bounded, progress.clone(), password, limits)
                        .await
                }
                ArchiveInner::Rar4(r) => {
                    r.extract(entry, &mut bounded, progress.clone(), password, limits)
                        .await
                }
            }
            .map_err(translate_core_error)?;

            let target = String::from_utf8_lossy(&buf);
            match archive_core::evaluate_symlink_target(&entry.name, &target) {
                archive_core::SymlinkDecision::Reject => {
                    return Err(translate_core_error(CoreError::SymlinkTargetEscape {
                        path: entry.name.clone(),
                        target: target.into_owned(),
                    }));
                }
                archive_core::SymlinkDecision::Allow => {
                    use tokio::io::AsyncWriteExt;
                    sink.write_all(&buf)
                        .await
                        .map_err(|e| ArchiveError::Internal {
                            detail: format!("I/O error: {}", e),
                        })?;
                    return Ok(bytes);
                }
            }
        }

        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry, sink, progress, password, limits).await,
            ArchiveInner::Rar(r) => r.extract(entry, sink, progress, password, limits).await,
            ArchiveInner::Rar4(r) => r.extract(entry, sink, progress, password, limits).await,
        }
        .map_err(translate_core_error)
    }

    /// Extract multiple entries in a single batch operation.
    ///
    /// Per-entry failures are collected in `BatchResult.failed` — extraction
    /// continues past CRC/decompression errors. Returns `Err` only for
    /// unrecoverable errors (cancellation).
    pub async fn extract_selected<SinkFn>(
        &self,
        entries: &[&ArchiveEntry],
        mut sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<BatchResult, ArchiveError>
    where
        SinkFn:
            FnMut(&ArchiveEntry) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> + Send,
    {
        let limits = &self.config.limits;

        if self.config.path_policy == archive_core::PathPolicy::Strict {
            for entry in entries {
                if let archive_core::PathDecision::Reject { path }
                | archive_core::PathDecision::Rewrite { original: path, .. } =
                    archive_core::evaluate_path(&entry.name, self.config.path_policy)
                {
                    return Err(translate_core_error(CoreError::PathTraversal { path }));
                }
            }
        }

        let mut symlinks = Vec::new();
        let mut non_symlinks = Vec::new();
        for &e in entries {
            if e.is_symlink {
                symlinks.push(e);
            } else {
                non_symlinks.push(e);
            }
        }

        let non_symlink_result = {
            let mut wrapped_factory = |entry: &ArchiveEntry| match archive_core::evaluate_path(
                &entry.name,
                self.config.path_policy,
            ) {
                archive_core::PathDecision::Reject { path } => {
                    progress
                        .lock()
                        .unwrap()
                        .on_warning(&format!("Skipping unsafe path: {}", path));
                    None
                }
                archive_core::PathDecision::Rewrite {
                    sanitized,
                    original,
                } => {
                    progress.lock().unwrap().on_warning(&format!(
                        "Sanitized unsafe path: {} -> {}",
                        original, sanitized
                    ));
                    let mut safe_entry = entry.clone();
                    safe_entry.name = sanitized;
                    sink_factory(&safe_entry)
                }
                archive_core::PathDecision::Allow => sink_factory(entry),
            };

            match &self.inner {
                ArchiveInner::Zip(z) => {
                    z.extract_selected(
                        &non_symlinks,
                        &mut wrapped_factory,
                        progress.clone(),
                        password,
                        limits,
                    )
                    .await
                }
                ArchiveInner::Rar(r) => {
                    r.extract_selected(
                        &non_symlinks,
                        &mut wrapped_factory,
                        progress.clone(),
                        password,
                        limits,
                    )
                    .await
                }
                ArchiveInner::Rar4(r) => {
                    r.extract_selected(
                        &non_symlinks,
                        &mut wrapped_factory,
                        progress.clone(),
                        password,
                        limits,
                    )
                    .await
                }
            }
            .map_err(translate_core_error)?
        };

        let mut batch_result = non_symlink_result;

        for entry in symlinks {
            let safe_entry = match archive_core::evaluate_path(&entry.name, self.config.path_policy)
            {
                archive_core::PathDecision::Reject { path } => {
                    progress
                        .lock()
                        .unwrap()
                        .on_warning(&format!("Skipping unsafe path: {}", path));
                    continue;
                }
                archive_core::PathDecision::Rewrite {
                    sanitized,
                    original,
                } => {
                    progress.lock().unwrap().on_warning(&format!(
                        "Sanitized unsafe path: {} -> {}",
                        original, sanitized
                    ));
                    let mut safe = entry.clone();
                    safe.name = sanitized;
                    safe
                }
                archive_core::PathDecision::Allow => entry.clone(),
            };

            if let Some(mut sink) = sink_factory(&safe_entry) {
                let mut buf = Vec::with_capacity(256);
                let mut bounded = LimitedBuf {
                    inner: &mut buf,
                    cap: 4096,
                };
                let bytes_res = match &self.inner {
                    ArchiveInner::Zip(z) => {
                        z.extract(entry, &mut bounded, progress.clone(), password, limits)
                            .await
                    }
                    ArchiveInner::Rar(r) => {
                        r.extract(entry, &mut bounded, progress.clone(), password, limits)
                            .await
                    }
                    ArchiveInner::Rar4(r) => {
                        r.extract(entry, &mut bounded, progress.clone(), password, limits)
                            .await
                    }
                };

                match bytes_res {
                    Ok(bytes) => {
                        let target = String::from_utf8_lossy(&buf);
                        match archive_core::evaluate_symlink_target(&entry.name, &target) {
                            archive_core::SymlinkDecision::Reject => {
                                let err = translate_core_error(CoreError::SymlinkTargetEscape {
                                    path: entry.name.clone(),
                                    target: target.into_owned(),
                                });
                                batch_result
                                    .failed
                                    .push((entry.name.clone(), err.to_string()));
                            }
                            archive_core::SymlinkDecision::Allow => {
                                use tokio::io::AsyncWriteExt;
                                match sink.write_all(&buf).await {
                                    Ok(_) => {
                                        batch_result.succeeded += 1;
                                        batch_result.bytes_written += bytes;
                                    }
                                    Err(e) => {
                                        batch_result.failed.push((
                                            entry.name.clone(),
                                            format!("I/O Error: {}", e),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if matches!(e, CoreError::Cancelled) {
                            return Err(translate_core_error(e));
                        }
                        batch_result
                            .failed
                            .push((entry.name.clone(), e.to_string()));
                    }
                }
            }
        }

        Ok(batch_result)
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

    /// Compression dictionary size in bytes for this entry, if known.
    ///
    /// Returns `Some(bytes)` for RAR5, `None` for ZIP/RAR4.
    pub fn dict_size(&self, entry: &ArchiveEntry) -> Option<u64> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.dict_size(entry),
            ArchiveInner::Rar(r) => r.dict_size(entry),
            ArchiveInner::Rar4(r) => r.dict_size(entry),
        }
    }

    /// Decompress and verify CRC without retaining output.
    pub async fn test(
        &self,
        entry: &ArchiveEntry,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<(), ArchiveError> {
        let limits = &self.config.limits;
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry, progress, limits).await,
            ArchiveInner::Rar(r) => r.test(entry, progress, limits).await,
            ArchiveInner::Rar4(r) => r.test(entry, progress, limits).await,
        }
        .map_err(translate_core_error)
    }

    /// Extract entries in a single streaming pass with pipelined I/O.
    ///
    /// For RAR5/RAR4 cloud archives, this replaces the "download everything
    /// to a temp file" approach with a one-pass streaming splitter that uses
    /// 1x disk space (one snippet at a time).
    ///
    /// Populates entries as a side effect (like `scan`).
    /// Returns total uncompressed bytes written.
    // TODO(symlink): symlink verification not wired into the streaming
    // path. Single-extract + extract_selected paths verify; streaming
    // entries flagged is_symlink will write unverified target as plain
    // content. Tracking for follow-up.
    pub async fn extract_all_streaming<Filter, SinkFn>(
        &mut self,
        filter: Filter,
        mut sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<u64, ArchiveError>
    where
        Filter: Fn(&ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        let limits = &self.config.limits;
        let strict_error = Arc::new(Mutex::new(None));

        let wrapped_factory = {
            let strict_error = strict_error.clone();
            let config_policy = self.config.path_policy;
            let progress = progress.clone();
            move |entry: &ArchiveEntry| {
                if strict_error.lock().unwrap().is_some() {
                    return None;
                }
                match archive_core::evaluate_path(&entry.name, config_policy) {
                    archive_core::PathDecision::Reject { path } => {
                        if config_policy == archive_core::PathPolicy::Strict {
                            *strict_error.lock().unwrap() = Some(path);
                            None
                        } else {
                            progress
                                .lock()
                                .unwrap()
                                .on_warning(&format!("Skipping unsafe path: {}", path));
                            None
                        }
                    }
                    archive_core::PathDecision::Rewrite {
                        sanitized,
                        original,
                    } => {
                        if config_policy == archive_core::PathPolicy::Strict {
                            *strict_error.lock().unwrap() = Some(original);
                            None
                        } else {
                            progress.lock().unwrap().on_warning(&format!(
                                "Sanitized unsafe path: {} -> {}",
                                original, sanitized
                            ));
                            let mut safe_entry = entry.clone();
                            safe_entry.name = sanitized;
                            sink_factory(&safe_entry)
                        }
                    }
                    archive_core::PathDecision::Allow => sink_factory(entry),
                }
            }
        };

        let res = match &mut self.inner {
            ArchiveInner::Zip(z) => {
                z.extract_all_streaming(filter, wrapped_factory, progress, password, limits)
                    .await
            }
            ArchiveInner::Rar(r) => {
                r.extract_all_streaming(filter, wrapped_factory, progress, password, limits)
                    .await
            }
            ArchiveInner::Rar4(r) => {
                r.extract_all_streaming(filter, wrapped_factory, progress, password, limits)
                    .await
            }
        };

        let strict_err_opt = strict_error.lock().unwrap().take();
        if let Some(path) = strict_err_opt {
            return Err(translate_core_error(CoreError::PathTraversal { path }));
        }

        res.map_err(translate_core_error)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use archive_core::LocalReadAt;

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
            zw.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf
    }

    // ── Moved from facade.rs ───────────────────────────────────────────────

    #[tokio::test]
    async fn open_rar5_detects_rar_format() {
        let path = rar_fixture("rar5_store.rar");
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
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
        let path = rar_fixture("rar4_sample.rar");
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
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
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();
        assert!(!archive.entries().is_empty());
        assert_eq!(archive.entries()[0].name, "hello.txt");
    }

    #[tokio::test]
    async fn open_unknown_bytes_produces_error() {
        // Zero bytes are not a valid archive — must fail without panicking.
        let bytes = vec![0u8; 64];
        let (_f, path) = write_temp(&bytes);
        let result =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default()).await;
        assert!(result.is_err(), "unknown bytes should produce an error");
    }

    // ── New: Opened variant tests ──────────────────────────────────────────

    #[tokio::test]
    async fn open_zip_returns_ready() {
        let buf = build_zip(&[("hello.txt", b"hello")]);
        let (_f, path) = write_temp(&buf);
        let opened =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    #[tokio::test]
    async fn open_rar5_local_returns_ready() {
        // Local RAR5: sequential scan runs during open → always Ready.
        let path = rar_fixture("rar5_store.rar");
        let opened =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    #[tokio::test]
    async fn opened_ready_convenience_works() {
        let path = rar_fixture("rar5_store.rar");
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
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
        let path = rar_fixture("rar4_sample.rar");
        let opened =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap();
        assert!(matches!(opened, Opened::Ready(_)));
    }

    #[tokio::test]
    async fn extract_selected_extracts_subset() {
        let buf = build_zip(&[
            ("a.txt", b"hello world"),
            ("b.txt", b"foo bar baz"),
            ("c.txt", b"skip me please"),
        ]);
        let (_f, path) = write_temp(&buf);
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();

        let all_entries = archive.entries_clean();
        assert_eq!(all_entries.len(), 3);

        // Select only the first two entries.
        let selected: Vec<&ArchiveEntry> = all_entries[..2].to_vec();

        let progress = Arc::new(Mutex::new(()));
        let result = archive
            .extract_selected(
                &selected,
                |_entry| Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>),
                progress,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.succeeded, 2);
        assert_eq!(
            result.bytes_written,
            b"hello world".len() as u64 + b"foo bar baz".len() as u64
        );
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn extract_selected_skips_when_sink_factory_returns_none() {
        let buf = build_zip(&[("x.txt", b"data"), ("y.txt", b"more data")]);
        let (_f, path) = write_temp(&buf);
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();

        let all_entries = archive.entries_clean();
        #[allow(clippy::iter_cloned_collect)]
        let selected: Vec<&ArchiveEntry> = all_entries.iter().copied().collect();

        let progress = Arc::new(Mutex::new(()));
        // Skip all entries by returning None.
        let result = archive
            .extract_selected(&selected, |_entry| None, progress, None)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 0);
        assert_eq!(result.bytes_written, 0);
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn path_policy_strict_aborts_extraction() {
        let buf = build_zip(&[("safe.txt", b"safe"), ("../escape.txt", b"danger")]);
        let (_f, path) = write_temp(&buf);
        let config = ArchiveConfig {
            path_policy: archive_core::PathPolicy::Strict,
            ..ArchiveConfig::default()
        };
        let archive = Archive::open_reader_uncached(LocalReadAt::new(&path), &config)
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();

        let entries = archive.entries_clean();
        let progress = Arc::new(Mutex::new(()));
        let result = archive
            .extract_selected(
                &entries,
                |_| Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>),
                progress,
                None,
            )
            .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ArchiveError::SecurityRisk { .. }
        ));
    }

    #[tokio::test]
    async fn path_policy_sanitize_rewrites_and_extracts() {
        let buf = build_zip(&[("safe.txt", b"safe"), ("../escape.txt", b"danger")]);
        let (_f, path) = write_temp(&buf);
        let config = ArchiveConfig {
            path_policy: archive_core::PathPolicy::Sanitize,
            ..ArchiveConfig::default()
        };
        let archive = Archive::open_reader_uncached(LocalReadAt::new(&path), &config)
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();

        let entries = archive.entries_clean();
        let progress = Arc::new(Mutex::new(()));

        let extracted_paths = Arc::new(Mutex::new(Vec::new()));
        let extracted_paths_clone = extracted_paths.clone();

        let sink_factory = move |entry: &ArchiveEntry| {
            extracted_paths_clone
                .lock()
                .unwrap()
                .push(entry.name.clone());
            Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>)
        };

        let result = archive
            .extract_selected(&entries, sink_factory, progress, None)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 2);

        let paths = extracted_paths.lock().unwrap();
        assert!(paths.contains(&"safe.txt".to_string()));
        assert!(paths.contains(&"escape.txt".to_string()));
    }

    #[tokio::test]
    async fn extract_symlink_safe_target_writes_plain_text() {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            // Add a regular file
            zw.start_file("target.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"hello world").unwrap();

            // Add a symlink entry pointing to target.txt
            let mut options = zip::write::SimpleFileOptions::default();
            // 0xA000 is the Unix file attribute for symlink
            options = options.unix_permissions(0xA000 | 0o777);
            zw.start_file("link.txt", options).unwrap();
            zw.write_all(b"target.txt").unwrap();
            zw.finish().unwrap();
        }

        let (_f, path) = write_temp(&buf);
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();

        let mut link_entry = archive
            .entries_clean()
            .into_iter()
            .find(|e| e.name == "link.txt")
            .unwrap()
            .clone();
        link_entry.is_symlink = true;

        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let written = archive
            .extract(&link_entry, &mut out, progress, None)
            .await
            .unwrap();

        assert_eq!(written, b"target.txt".len() as u64);
        assert_eq!(out, b"target.txt");
    }

    #[tokio::test]
    async fn extract_symlink_unsafe_target_returns_error() {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let mut options = zip::write::SimpleFileOptions::default();
            options = options.unix_permissions(0xA000 | 0o777);
            zw.start_file("link.txt", options).unwrap();
            zw.write_all(b"../../../etc/passwd").unwrap();
            zw.finish().unwrap();
        }

        let (_f, path) = write_temp(&buf);
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();

        let mut link_entry = archive
            .entries_clean()
            .into_iter()
            .find(|e| e.name == "link.txt")
            .unwrap()
            .clone();
        link_entry.is_symlink = true;

        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let err = archive
            .extract(&link_entry, &mut out, progress.clone(), None)
            .await
            .unwrap_err();

        assert!(matches!(err, ArchiveError::SecurityRisk { .. }));

        // Test in extract_selected
        let result = archive
            .extract_selected(
                &[&link_entry],
                |_| Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>),
                progress,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].0, "link.txt");
        assert!(result.failed[0].1.contains("dangerous symlink"));
    }

    #[tokio::test]
    async fn extract_symlink_oversized_target_rejected() {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let mut options = zip::write::SimpleFileOptions::default();
            options = options.unix_permissions(0xA000 | 0o777);
            zw.start_file("link.txt", options).unwrap();
            let large_target = vec![b'a'; 5000]; // > 4096 cap
            zw.write_all(&large_target).unwrap();
            zw.finish().unwrap();
        }

        let (_f, path) = write_temp(&buf);
        let archive =
            Archive::open_reader_uncached(LocalReadAt::new(&path), &ArchiveConfig::default())
                .await
                .unwrap()
                .ready()
                .await
                .unwrap();

        let mut link_entry = archive
            .entries_clean()
            .into_iter()
            .find(|e| e.name == "link.txt")
            .unwrap()
            .clone();
        link_entry.is_symlink = true;

        let mut out = Vec::new();
        let progress = Arc::new(Mutex::new(()));
        let err = archive
            .extract(&link_entry, &mut out, progress, None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ArchiveError::FileCorrupted { .. } | ArchiveError::Internal { .. }
        ));
    }
}
