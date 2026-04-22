use std::sync::{Arc, Mutex};

use archive_core::{
    ArchiveEntry, ArchiveReader, CoreError, LocalPath, ProgressReporter, ReadAt, ScanInfo,
};
use tracing::{debug, info};

use crate::archive::{Archive, ArchiveInner};
use crate::cache::{ArchiveFingerprint, cache_path_for};
use crate::error::{ArchiveError, translate_core_error};

/// Result of opening an archive. Either ready to use or requires a metadata scan.
pub enum Opened<R: ReadAt + LocalPath + 'static> {
    /// Archive metadata is available. Use `Archive` immediately.
    Ready(Archive<R>),
    /// Archive requires a metadata scan before entries are available.
    /// Call `scan_info()` to learn the cost, then `scan()` to execute.
    NeedsScan(PendingArchive<R>),
}

/// An archive that needs a metadata scan before it can be used.
///
/// Created by `Archive::open()` when the format requires an expensive
/// scan phase (e.g., RAR over HTTP without Quick Open).
pub struct PendingArchive<R: ReadAt + LocalPath + 'static> {
    pub(crate) inner: ArchiveInner<R>,
    info: ScanInfo,
    pub(crate) fingerprint: ArchiveFingerprint,
    pub(crate) source: String,
    pub(crate) config: crate::config::ArchiveConfig,
}

impl<R: ReadAt + LocalPath + 'static> Opened<R> {
    /// Convenience: resolve to a ready `Archive`.
    ///
    /// If the archive needs scanning, scans with no-op progress (no UI feedback).
    /// Use this in tests or non-interactive contexts.
    pub async fn ready(self) -> Result<Archive<R>, ArchiveError> {
        match self {
            Opened::Ready(a) => Ok(a),
            Opened::NeedsScan(p) => p.scan(Arc::new(Mutex::new(()))).await,
        }
    }
}

impl<R: ReadAt + LocalPath + 'static> PendingArchive<R> {
    pub(crate) fn new(
        inner: ArchiveInner<R>,
        info: ScanInfo,
        fingerprint: ArchiveFingerprint,
        source: String,
        config: crate::config::ArchiveConfig,
    ) -> Self {
        Self {
            inner,
            info,
            fingerprint,
            source,
            config,
        }
    }

    /// What the scan will cost and why it's needed.
    pub fn scan_info(&self) -> &ScanInfo {
        &self.info
    }

    /// Run the metadata scan with progress reporting.
    /// Consumes self, returns a ready-to-use Archive.
    pub async fn scan(
        mut self,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<Archive<R>, ArchiveError> {
        info!(scan_info = ?self.info, "PendingArchive::scan starting");
        match &mut self.inner {
            ArchiveInner::Zip(_) => {
                debug!("ZIP scan no-op (should not happen)");
            }
            ArchiveInner::Rar(r) => {
                r.scan_metadata(progress, &self.config.limits)
                    .await
                    .map_err(translate_core_error)?;
            }
            ArchiveInner::Rar4(r) => {
                r.scan_metadata(progress, &self.config.limits)
                    .await
                    .map_err(translate_core_error)?;
            }
        }

        let archive = Archive {
            inner: self.inner,
            config: self.config,
        };
        info!(
            entries = archive.entries().len(),
            "PendingArchive::scan complete"
        );

        // ── Implicit cache save (failure silently ignored) ────────────────
        if let Some(cache_path) = cache_path_for(&self.source) {
            debug!(cache_path = %cache_path.display(), "saving scan result to cache");
            let _ = archive.save_cache(&self.fingerprint, &cache_path);
        }

        Ok(archive)
    }

    /// Scan and extract in a single streaming pass.
    ///
    /// Equivalent to calling `scan()` followed by extraction, but done in one
    /// HTTP stream for RAR5/RAR4 cloud archives.
    pub async fn scan_and_extract<Filter, SinkFn>(
        mut self,
        filter: Filter,
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<(Archive<R>, u64), ArchiveError>
    where
        Filter: Fn(&ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>> + Send + 'static,
    {
        let total = match &mut self.inner {
            ArchiveInner::Zip(_) => {
                return Err(translate_core_error(CoreError::UnsupportedFeature {
                    detail: "streaming extraction not supported for ZIP".into(),
                }));
            }
            ArchiveInner::Rar(r) => r
                .extract_all_streaming(filter, sink_factory, progress.clone(), password, &self.config.limits)
                .await
                .map_err(translate_core_error)?,
            ArchiveInner::Rar4(r) => r
                .extract_all_streaming(filter, sink_factory, progress.clone(), password, &self.config.limits)
                .await
                .map_err(translate_core_error)?,
        };

        let archive = Archive {
            inner: self.inner,
            config: self.config,
        };

        // Save cache (same pattern as PendingArchive::scan).
        if let Some(cache_path) = cache_path_for(&self.source) {
            debug!(cache_path = %cache_path.display(), "saving scan+extract result to cache");
            let _ = archive.save_cache(&self.fingerprint, &cache_path);
        }

        Ok((archive, total))
    }
}

impl<R: ReadAt + LocalPath + 'static> std::fmt::Debug for Opened<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(a) => f.debug_tuple("Ready").field(a).finish(),
            Self::NeedsScan(p) => f.debug_tuple("NeedsScan").field(p).finish(),
        }
    }
}

impl<R: ReadAt + LocalPath + 'static> std::fmt::Debug for PendingArchive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingArchive")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}
