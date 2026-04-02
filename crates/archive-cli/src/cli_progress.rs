use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use indicatif::{ProgressBar, ProgressStyle};

/// CLI progress reporter using indicatif for stderr output.
///
/// Handles two phases:
/// - **Scan:** spinner with byte + entry count updates
/// - **Extraction/Test:** per-entry progress with optional byte-level bar
pub struct CliProgressReporter {
    /// Active progress bar (scan spinner or entry bar).
    bar: Option<ProgressBar>,
    /// Cancellation flag (shared with signal handler).
    cancel: Arc<AtomicBool>,
    /// Tracks how many entries have been processed (for multi-entry ops).
    completed: u64,
    /// Total number of entries (set externally before extraction loop).
    total_entries: u64,
}

impl CliProgressReporter {
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            bar: None,
            cancel,
            completed: 0,
            total_entries: 0,
        }
    }

    /// Set total entry count for multi-entry operations (ExtractAll, Test).
    pub fn set_total_entries(&mut self, total: u64) {
        self.total_entries = total;
    }
}

impl archive::ProgressReporter for CliProgressReporter {
    fn on_scan_start(&mut self, total_bytes: u64) {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("  {spinner:.cyan} Scanning... {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("=> "),
        );
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        self.bar = Some(bar);
    }

    fn on_scan_progress(&mut self, bytes_scanned: u64, entries_found: usize) {
        if let Some(bar) = &self.bar {
            bar.set_position(bytes_scanned);
            bar.set_message(format!("{entries_found} entries"));
        }
    }

    fn on_scan_complete(&mut self, entries_found: usize) {
        if let Some(bar) = self.bar.take() {
            bar.finish_with_message(format!("Scan complete — {entries_found} entries"));
        }
    }

    fn on_entry_start(&mut self, name: &str, size: u64) {
        let prefix = if self.total_entries > 0 {
            format!("[{}/{}] ", self.completed + 1, self.total_entries)
        } else {
            String::new()
        };

        let bar = if size > 0 {
            let b = ProgressBar::new(size);
            b.set_style(
                ProgressStyle::default_bar()
                    .template("  {prefix}{msg} {bar:30.cyan/dim} {bytes}/{total_bytes}")
                    .unwrap()
                    .progress_chars("=> "),
            );
            b.set_prefix(prefix);
            b.set_message(short_name(name));
            b
        } else {
            let b = ProgressBar::new_spinner();
            b.set_style(
                ProgressStyle::default_spinner()
                    .template("  {prefix}{spinner:.cyan} {msg}")
                    .unwrap(),
            );
            b.set_prefix(prefix);
            b.set_message(short_name(name));
            b.enable_steady_tick(std::time::Duration::from_millis(100));
            b
        };
        self.bar = Some(bar);
    }

    fn on_bytes_progress(&mut self, bytes_so_far: u64) {
        if let Some(bar) = &self.bar {
            bar.set_position(bytes_so_far);
        }
    }

    fn on_entry_complete(&mut self, name: &str, success: bool) {
        if let Some(bar) = self.bar.take() {
            if success {
                bar.finish_and_clear();
            } else {
                bar.abandon_with_message(format!("FAIL {}", short_name(name)));
            }
        }
        self.completed += 1;
    }

    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Shorten long entry names for display (show last path component).
fn short_name(name: &str) -> String {
    match name.rsplit('/').next() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => name.to_string(),
    }
}
