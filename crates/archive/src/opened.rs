use std::sync::{Arc, Mutex};

use archive_core::{ArchiveReader, ProgressReporter, ScanInfo};

use crate::archive::{Archive, ArchiveInner};
use crate::error::{translate_core_error, ArchiveError};

/// Result of opening an archive. Either ready to use or requires a metadata scan.
pub enum Opened {
    /// Archive metadata is available. Use `Archive` immediately.
    Ready(Archive),
    /// Archive requires a metadata scan before entries are available.
    /// Call `scan_info()` to learn the cost, then `scan()` to execute.
    NeedsScan(PendingArchive),
}

/// An archive that needs a metadata scan before it can be used.
///
/// Created by `Archive::open()` when the format requires an expensive
/// scan phase (e.g., RAR over HTTP without Quick Open).
pub struct PendingArchive {
    pub(crate) inner: ArchiveInner,
    info: ScanInfo,
}

impl Opened {
    /// Convenience: resolve to a ready `Archive`.
    ///
    /// If the archive needs scanning, scans with no-op progress (no UI feedback).
    /// Use this in tests or non-interactive contexts.
    pub async fn ready(self) -> Result<Archive, ArchiveError> {
        match self {
            Opened::Ready(a) => Ok(a),
            Opened::NeedsScan(p) => p.scan(Arc::new(Mutex::new(()))).await,
        }
    }
}

impl PendingArchive {
    pub(crate) fn new(inner: ArchiveInner, info: ScanInfo) -> Self {
        Self { inner, info }
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
    ) -> Result<Archive, ArchiveError> {
        match &mut self.inner {
            ArchiveInner::Zip(_) => {} // no-op, should not happen
            ArchiveInner::Rar(r) => {
                r.scan_metadata(progress)
                    .await
                    .map_err(translate_core_error)?;
            }
        }
        Ok(Archive { inner: self.inner })
    }
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(a) => f.debug_tuple("Ready").field(a).finish(),
            Self::NeedsScan(p) => f.debug_tuple("NeedsScan").field(p).finish(),
        }
    }
}

impl std::fmt::Debug for PendingArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingArchive")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}
