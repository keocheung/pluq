#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use archive::{Archive, ArchiveEntry, ArchiveError, Opened, ProgressReporter, ScanInfo};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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
    pub table_state: TableState,
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
            table_state: TableState::default().with_selected(Some(0)),
        }
    }

    pub fn handle_message(
        &mut self,
        msg: AppMessage,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        match msg {
            AppMessage::ArchiveOpened(Ok(Opened::Ready(archive))) => {
                let count = archive.entries_clean().len();
                info!(entries = count, "archive opened → Browsing");
                self.entries = archive.entries_clean().into_iter().cloned().collect();
                self.archive = Some(Arc::new(archive));
                self.phase = AppPhase::Browsing;
            }
            AppMessage::ArchiveOpened(Ok(Opened::NeedsScan(pending))) => {
                let total_bytes = match pending.scan_info() {
                    ScanInfo::Required { estimated_bytes, .. } => *estimated_bytes,
                    ScanInfo::Ready => 0,
                };
                info!(total_bytes, "archive needs scan → Scanning");
                self.phase = AppPhase::Scanning { total_bytes, bytes_scanned: 0, entries_found: 0 };
                debug!("spawning scan task");
                let tx_scan = tx.clone();
                tokio::spawn(async move {
                    let progress = Arc::new(Mutex::new(TuiProgressReporter::new(tx_scan.clone())));
                    let result = pending.scan(progress).await;
                    let _ = tx_scan.send(AppMessage::ScanComplete(result));
                });
            }
            AppMessage::ArchiveOpened(Err(e)) => {
                error!(error = %e, "archive open failed → Error");
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
                let count = archive.entries_clean().len();
                info!(entries = count, "scan complete → Browsing");
                self.entries = archive.entries_clean().into_iter().cloned().collect();
                self.archive = Some(Arc::new(archive));
                self.phase = AppPhase::Browsing;
            }
            AppMessage::ScanComplete(Err(e)) => {
                error!(error = %e, "scan failed → Error");
                self.phase = AppPhase::Error(e.to_string());
            }
            AppMessage::ExtractionProgress { bytes_so_far } => {
                if let AppPhase::Extracting { ref mut bytes_extracted, .. } = self.phase {
                    *bytes_extracted = bytes_so_far;
                }
            }
            AppMessage::ExtractionComplete(Ok(bytes)) => {
                let name = if let AppPhase::Extracting { ref entry_name, .. } = self.phase {
                    entry_name.clone()
                } else {
                    String::new()
                };
                info!(name = %name, bytes, "extraction complete → Browsing");
                self.phase = AppPhase::Browsing;
                self.status_message =
                    Some(format!("Extracted '{}' ({})", name, crate::format_size(bytes)));
            }
            AppMessage::ExtractionComplete(Err(e)) => {
                warn!(error = %e, "extraction failed → Browsing");
                self.phase = AppPhase::Browsing;
                self.status_message = Some(format!("Extraction failed: {e}"));
            }
        }
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<AppMessage>,
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
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if matches!(self.phase, AppPhase::Browsing) && !self.entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    self.selected = 0;
                    self.table_state.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if matches!(self.phase, AppPhase::Browsing) && !self.entries.is_empty() {
                    self.selected = self.entries.len() - 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Enter => {
                if matches!(self.phase, AppPhase::Browsing)
                    && let Some(entry) = self.entries.get(self.selected)
                {
                    if entry.is_directory {
                        return;
                    }
                    let entry_name = entry.name.clone();
                    let total_bytes = entry.uncompressed_size;

                    info!(
                        name = %entry_name,
                        total_bytes,
                        "user requested extraction → Extracting"
                    );

                    self.phase = AppPhase::Extracting {
                        entry_name: entry_name.clone(),
                        total_bytes,
                        bytes_extracted: 0,
                    };

                    let output_dir = crate::default_output_folder(&self.source)
                        .unwrap_or_else(|| std::path::PathBuf::from("extracted"));
                    let output_path = output_dir.join(&entry_name);

                    let archive = self.archive.as_ref().unwrap().clone();
                    let entry = self.entries[self.selected].clone();
                    let tx_extract = tx.clone();
                    let progress =
                        Arc::new(Mutex::new(TuiProgressReporter::new(tx_extract.clone())));

                    debug!(output = %output_path.display(), "spawning extraction task");
                    tokio::spawn(async move {
                        if let Some(parent) = output_path.parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        match tokio::fs::File::create(&output_path).await {
                            Ok(mut file) => {
                                let result = archive.extract(&entry, &mut file, progress, None).await;
                                let _ = tx_extract.send(AppMessage::ExtractionComplete(result));
                            }
                            Err(e) => {
                                let _ = tx_extract.send(AppMessage::ExtractionComplete(Err(
                                    archive::ArchiveError::Internal {
                                        detail: format!(
                                            "Failed to create {}: {e}",
                                            output_path.display()
                                        ),
                                    },
                                )));
                            }
                        }
                    });
                }
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
