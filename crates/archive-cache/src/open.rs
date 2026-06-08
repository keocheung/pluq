use archive::{Archive, ArchiveConfig, ArchiveError};
use archive_core::ProgressReporter;
use archive_core::{ArchiveFingerprint, ReadAt};
use archive_source::{ResolvedSource, SourceConfig};
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

pub(crate) fn from_cache(
    cache: crate::ArchiveCache,
    reader: ResolvedSource,
    config: ArchiveConfig,
) -> Archive<ResolvedSource> {
    Archive::from_cache(cache.format_state, cache.entries, reader, config)
}

pub(crate) fn save_cache(
    archive: &Archive<ResolvedSource>,
    fingerprint: &ArchiveFingerprint,
    path: &std::path::Path,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let cache = crate::ArchiveCache {
        schema_version: crate::SCHEMA_VERSION,
        fingerprint: fingerprint.clone(),
        entries: archive.entries().to_vec(),
        format_state: archive.format_cache_state(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    cache.save(path)
}

pub enum Opened {
    Ready(Archive<ResolvedSource>),
    NeedsScan(PendingArchive),
}

impl Opened {
    pub async fn ready(self) -> Result<Archive<ResolvedSource>, ArchiveError> {
        match self {
            Opened::Ready(a) => Ok(a),
            Opened::NeedsScan(p) => p.scan(Arc::new(Mutex::new(()))).await,
        }
    }
}

pub struct PendingArchive {
    inner: archive::PendingArchive<ResolvedSource>,
    fingerprint: ArchiveFingerprint,
    source: String,
}

impl PendingArchive {
    pub fn scan_info(&self) -> &archive_core::ScanInfo {
        self.inner.scan_info()
    }

    pub async fn scan(
        self,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<Archive<ResolvedSource>, ArchiveError> {
        let archive = self.inner.scan(progress).await?;
        if let Some(cache_path) = crate::cache_path_for(&self.source) {
            let _ = save_cache(&archive, &self.fingerprint, &cache_path);
        }
        Ok(archive)
    }

    pub async fn scan_and_extract<Filter, SinkFn>(
        self,
        filter: Filter,
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<(Archive<ResolvedSource>, archive_core::BatchResult), ArchiveError>
    where
        Filter: Fn(&archive_core::ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&archive_core::ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>>
            + Send
            + 'static,
    {
        let (archive, batch) = self
            .inner
            .scan_and_extract(filter, sink_factory, progress, password)
            .await?;
        if let Some(cache_path) = crate::cache_path_for(&self.source) {
            let _ = save_cache(&archive, &self.fingerprint, &cache_path);
        }
        Ok((archive, batch))
    }
}

pub async fn open(
    source: &str,
    config: &SourceConfig,
    archive_config: ArchiveConfig,
) -> Result<Opened, ArchiveError> {
    let resolved =
        archive_source::resolve(source, config).map_err(|e| ArchiveError::ConnectionFailed {
            detail: format!("source resolution: {e}"),
        })?;

    let _ = resolved
        .size()
        .await
        .map_err(|e| archive::translate_core_error(archive_core::CoreError::from(e)))?;

    let fingerprint = resolved.fingerprint();
    debug!(
        source,
        size = fingerprint.source_size,
        etag = ?fingerprint.etag,
        last_modified = ?fingerprint.last_modified,
        "resolved source fingerprint"
    );

    // Disk cache (Layer 3 state): a valid hit returns the stored entry list with
    // NO archive parse — i.e. no central-directory/EOCD reads from the source.
    // `--clean-cache` wipes this dir, forcing the full parse below. Logging the
    // hit/miss/invalid decision here makes that difference visible in the log.
    if let Some(cache_path) = crate::cache_path_for(source) {
        match crate::ArchiveCache::load(&cache_path) {
            Ok(cache) => match cache.invalidity_reason(&fingerprint) {
                None => {
                    info!(
                        source,
                        cache_path = %cache_path.display(),
                        entries = cache.entries.len(),
                        "archive cache HIT — using stored entries, skipping source parse"
                    );
                    let archive = from_cache(cache, resolved, archive_config);
                    return Ok(Opened::Ready(archive));
                }
                Some(reason) => {
                    info!(
                        source,
                        cache_path = %cache_path.display(),
                        reason,
                        "archive cache INVALID — re-parsing from source (fingerprint mismatch)"
                    );
                }
            },
            Err(e) => {
                debug!(
                    source,
                    cache_path = %cache_path.display(),
                    error = %e,
                    "archive cache MISS — absent/unreadable, parsing from source"
                );
            }
        }
    } else {
        debug!(source, "no cache path for source — parsing from source");
    }

    info!(
        source,
        "opening archive via uncached parse (reading central directory from the source)"
    );
    let opened = archive::Archive::open_reader_uncached(resolved, &archive_config).await?;

    match opened {
        archive::Opened::Ready(archive) => {
            if let Some(cache_path) = crate::cache_path_for(source) {
                let _ = save_cache(&archive, &fingerprint, &cache_path);
            }
            Ok(Opened::Ready(archive))
        }
        archive::Opened::NeedsScan(inner) => Ok(Opened::NeedsScan(PendingArchive {
            inner,
            fingerprint,
            source: source.to_string(),
        })),
    }
}
