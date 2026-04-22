use tracing::warn;

/// Callbacks for tracking extraction progress and requesting cancellation.
///
/// Default implementations are no-ops — callers that don't need progress
/// can pass `&mut ()`.
pub trait ProgressReporter: Send {
    fn on_entry_start(&mut self, _name: &str, _size: u64) {}
    fn on_bytes_progress(&mut self, _bytes_so_far: u64) {}
    fn on_entry_complete(&mut self, _name: &str, _success: bool) {}
    fn should_cancel(&self) -> bool {
        false
    }

    /// Called when a metadata scan begins. `total_bytes` is the archive size to stream through.
    fn on_scan_start(&mut self, _total_bytes: u64) {}

    /// Called periodically during scan. `bytes_scanned` = bytes consumed so far.
    fn on_scan_progress(&mut self, _bytes_scanned: u64, _entries_found: usize) {}

    /// Called when scan finishes.
    fn on_scan_complete(&mut self, _entries_found: usize) {}

    /// A non-fatal safety warning was raised. Typical source:
    /// `ResourceLimits` check in `LimitMode::Warn` (e.g. "suspicious
    /// 800:1 ratio"). The operation continues.
    fn on_warning(&mut self, message: &str) {
        warn!("Archive warning: {}", message);
    }
}

/// No-op implementation — use when progress reporting is not needed.
impl ProgressReporter for () {}
