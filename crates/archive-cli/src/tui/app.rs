#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use archive::{Archive, ArchiveEntry, ArchiveError, Opened, ProgressReporter, ScanInfo};
use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

/// Messages sent from background tasks to the main loop.
pub enum AppMessage {
    /// Archive::open() completed.
    ArchiveOpened(Result<Opened, ArchiveError>),
    /// Scan progress update (from ProgressReporter).
    ScanProgress { bytes_scanned: u64, entries_found: usize },
    /// PendingArchive::scan() completed.
    ScanComplete(Result<Archive, ArchiveError>),
    /// Extraction byte progress (from ProgressReporter).
    ExtractionProgress { bytes_so_far: u64 },
    /// Archive::extract() completed.
    ExtractionComplete(Result<u64, ArchiveError>),
}

/// Current phase of the application.
pub enum AppPhase {
    Opening,
    Scanning {
        total_bytes: u64,
        bytes_scanned: u64,
        entries_found: usize,
    },
    Browsing,
    Extracting {
        entry_name: String,
        total_bytes: u64,
        bytes_extracted: u64,
    },
    Error(String),
}

pub struct App {
    pub phase: AppPhase,
    pub source: String,
    pub archive: Option<Arc<Archive>>,
    pub entries: Vec<ArchiveEntry>,
    pub selected: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
}

impl App {
    pub fn new(source: String) -> Self {
        Self {
            phase: AppPhase::Opening,
            source,
            archive: None,
            entries: Vec::new(),
            selected: 0,
            should_quit: false,
            status_message: None,
        }
    }

    pub fn handle_message(
        &mut self,
        msg: AppMessage,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        match msg {
            AppMessage::ArchiveOpened(Ok(Opened::Ready(archive))) => {
                self.entries = archive.entries_clean().into_iter().cloned().collect();
                self.archive = Some(Arc::new(archive));
                self.phase = AppPhase::Browsing;
            }
            AppMessage::ArchiveOpened(Ok(Opened::NeedsScan(pending))) => {
                let total_bytes = match pending.scan_info() {
                    ScanInfo::Required { estimated_bytes, .. } => *estimated_bytes,
                    ScanInfo::Ready => 0,
                };
                self.phase = AppPhase::Scanning { total_bytes, bytes_scanned: 0, entries_found: 0 };
                let tx_scan = tx.clone();
                tokio::spawn(async move {
                    let progress = Arc::new(Mutex::new(TuiProgressReporter::new(tx_scan.clone())));
                    let result = pending.scan(progress).await;
                    let _ = tx_scan.send(AppMessage::ScanComplete(result));
                });
            }
            AppMessage::ArchiveOpened(Err(e)) => {
                self.phase = AppPhase::Error(e.to_string());
            }
            AppMessage::ScanProgress { bytes_scanned, entries_found } => {
                if let AppPhase::Scanning { bytes_scanned: ref mut b, entries_found: ref mut e, .. } =
                    self.phase
                {
                    *b = bytes_scanned;
                    *e = entries_found;
                }
            }
            AppMessage::ScanComplete(Ok(archive)) => {
                self.entries = archive.entries_clean().into_iter().cloned().collect();
                self.archive = Some(Arc::new(archive));
                self.phase = AppPhase::Browsing;
            }
            AppMessage::ScanComplete(Err(e)) => {
                self.phase = AppPhase::Error(e.to_string());
            }
            AppMessage::ExtractionProgress { bytes_so_far } => {
                if let AppPhase::Extracting { ref mut bytes_extracted, .. } = self.phase {
                    *bytes_extracted = bytes_so_far;
                }
            }
            AppMessage::ExtractionComplete(Ok(bytes)) => {
                self.phase = AppPhase::Browsing;
                self.status_message =
                    Some(format!("Extracted ({} bytes)", bytes));
            }
            AppMessage::ExtractionComplete(Err(e)) => {
                self.phase = AppPhase::Browsing;
                self.status_message = Some(format!("Extraction failed: {e}"));
            }
        }
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        _tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        // Any keypress clears status message.
        self.status_message = None;

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    if !self.entries.is_empty() {
                        self.selected = (self.selected + 1).min(self.entries.len() - 1);
                    }
                }
            }
            KeyCode::Enter => {
                // No-op for now — extraction comes in Step 6.
            }
            _ => {}
        }
    }
}

/// Progress reporter that forwards updates to the TUI via an mpsc channel.
pub struct TuiProgressReporter {
    tx: mpsc::UnboundedSender<AppMessage>,
}

impl TuiProgressReporter {
    pub fn new(tx: mpsc::UnboundedSender<AppMessage>) -> Self {
        Self { tx }
    }
}

impl ProgressReporter for TuiProgressReporter {
    fn on_scan_progress(&mut self, bytes_scanned: u64, entries_found: usize) {
        let _ = self.tx.send(AppMessage::ScanProgress { bytes_scanned, entries_found });
    }

    fn on_bytes_progress(&mut self, bytes_so_far: u64) {
        let _ = self.tx.send(AppMessage::ExtractionProgress { bytes_so_far });
    }
}
