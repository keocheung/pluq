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
}

/// No-op implementation — use when progress reporting is not needed.
impl ProgressReporter for () {}
