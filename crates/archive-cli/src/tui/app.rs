#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use archive::{Archive, ArchiveEntry, ArchiveError, BatchResult, Opened, ProgressReporter, ScanInfo};
use archive_source::ResolvedSource;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Messages sent from background tasks to the main loop.
///
/// Only discrete state-change events use the channel. High-frequency progress
/// updates are read from shared atomics on each tick instead (see `SharedProgress`).
pub enum AppMessage {
    /// Archive::open() completed.
    ArchiveOpened(Result<Opened<ResolvedSource>, ArchiveError>),
    /// PendingArchive::scan() completed.
    ScanComplete(Result<Archive<ResolvedSource>, ArchiveError>),
    /// Archive::extract() completed.
    ExtractionComplete(Result<u64, ArchiveError>),
    /// Archive::extract_selected() completed.
    /// Ok contains the BatchResult. Err is only for unrecoverable errors (cancellation).
    BatchExtractionComplete(Result<BatchResult, ArchiveError>),
}

/// Shared atomics for high-frequency progress updates.
///
/// Written by `TuiProgressReporter` on background threads, read by the main
/// loop on each 50 ms tick. This avoids flooding the message channel.
#[derive(Default)]
pub struct SharedProgress {
    pub bytes_scanned: AtomicU64,
    pub entries_found: AtomicUsize,
    pub bytes_extracted: AtomicU64,
    /// Number of entries completed (success or failure) in the current batch.
    pub files_done: AtomicUsize,
    /// Name of the entry currently being extracted (batch mode).
    pub current_entry_name: Mutex<String>,
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
    ConfirmExtract {
        entry_index: usize,
        entry_name: String,
        dict_size: u64,
    },
    Extracting {
        entry_name: String,
        total_bytes: u64,
        bytes_extracted: u64,
    },
    ExtractingBatch {
        total_files: usize,
        files_done: usize,
        current_entry: String,
        total_bytes: u64,
        bytes_extracted: u64,
    },
    Error(String),
}

pub struct App {
    pub phase: AppPhase,
    pub source: String,
    pub archive: Option<Arc<Archive<ResolvedSource>>>,
    pub entries: Vec<ArchiveEntry>,
    pub selected: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub table_state: TableState,
    /// Entry indices toggled for batch extraction (by index into `self.entries`).
    pub toggled: HashSet<usize>,
    /// Shared cancel flag — set by Esc during extraction, read by ProgressReporter.
    pub cancel_flag: Arc<AtomicBool>,
    /// Shared progress counters — written by ProgressReporter, read on tick.
    pub progress: Arc<SharedProgress>,
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
            toggled: HashSet::new(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(SharedProgress::default()),
        }
    }

    pub fn handle_message(
        &mut self,
        msg: AppMessage,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        // Sync latest progress from shared atomics before processing state transitions.
        self.poll_progress();

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
                // Reset shared progress counters for this scan.
                self.progress.bytes_scanned.store(0, Ordering::Relaxed);
                self.progress.entries_found.store(0, Ordering::Relaxed);
                self.phase = AppPhase::Scanning { total_bytes, bytes_scanned: 0, entries_found: 0 };
                debug!("spawning scan task");
                let tx_scan = tx.clone();
                let shared = self.progress.clone();
                tokio::spawn(async move {
                    let progress = Arc::new(Mutex::new(TuiProgressReporter::new(
                        shared,
                        Arc::new(AtomicBool::new(false)),
                    )));
                    let result = pending.scan(progress).await;
                    let _ = tx_scan.send(AppMessage::ScanComplete(result));
                });
            }
            AppMessage::ArchiveOpened(Err(e)) => {
                error!(error = %e, "archive open failed → Error");
                self.phase = AppPhase::Error(e.to_string());
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
                if matches!(e, ArchiveError::Cancelled) {
                    info!("extraction cancelled → Browsing");
                    self.status_message = Some("Extraction cancelled.".to_string());
                } else {
                    warn!(error = %e, "extraction failed → Browsing");
                    self.status_message = Some(format!("Extraction failed: {e}"));
                }
                self.phase = AppPhase::Browsing;
            }
            AppMessage::BatchExtractionComplete(Ok(batch)) => {
                let n = batch.succeeded;
                let bytes = batch.bytes_written;
                let failed = batch.failed.len();
                info!(succeeded = n, failed, bytes, "batch extraction complete → Browsing");
                self.toggled.clear();
                self.phase = AppPhase::Browsing;
                self.status_message = Some(if failed == 0 {
                    format!(
                        "Extracted {} file{} ({})",
                        n,
                        if n == 1 { "" } else { "s" },
                        crate::format_size(bytes),
                    )
                } else {
                    format!(
                        "Extracted {} file{} ({}) — {} failed",
                        n,
                        if n == 1 { "" } else { "s" },
                        crate::format_size(bytes),
                        failed,
                    )
                });
            }
            AppMessage::BatchExtractionComplete(Err(e)) => {
                self.toggled.clear();
                self.phase = AppPhase::Browsing;
                if matches!(e, ArchiveError::Cancelled) {
                    info!("batch extraction cancelled → Browsing");
                    self.status_message = Some("Batch extraction cancelled.".to_string());
                } else {
                    warn!(error = %e, "batch extraction failed → Browsing");
                    self.status_message = Some(format!("Extraction failed: {e}"));
                }
            }
        }
    }

    /// Read shared progress atomics and update the current phase.
    /// Called on each tick (50 ms) — no channel messages needed.
    pub fn poll_progress(&mut self) {
        match self.phase {
            AppPhase::Scanning { ref mut bytes_scanned, ref mut entries_found, .. } => {
                *bytes_scanned = self.progress.bytes_scanned.load(Ordering::Relaxed);
                *entries_found = self.progress.entries_found.load(Ordering::Relaxed);
            }
            AppPhase::Extracting { ref mut bytes_extracted, .. } => {
                *bytes_extracted = self.progress.bytes_extracted.load(Ordering::Relaxed);
            }
            AppPhase::ExtractingBatch {
                ref mut bytes_extracted,
                ref mut files_done,
                ref mut current_entry,
                ..
            } => {
                *bytes_extracted = self.progress.bytes_extracted.load(Ordering::Relaxed);
                *files_done = self.progress.files_done.load(Ordering::Relaxed);
                *current_entry = self.progress.current_entry_name.lock().unwrap().clone();
            }
            _ => {}
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
            KeyCode::Char('q') => {
                if matches!(
                    self.phase,
                    AppPhase::Extracting { .. } | AppPhase::ExtractingBatch { .. }
                ) {
                    // Signal cancel so the background task cleans up the partial file(s).
                    self.cancel_flag.store(true, Ordering::SeqCst);
                }
                self.should_quit = true;
            }
            KeyCode::Esc => {
                if matches!(
                    self.phase,
                    AppPhase::Extracting { .. } | AppPhase::ExtractingBatch { .. }
                ) {
                    debug!("Esc during Extracting/ExtractingBatch → cancelling");
                    self.cancel_flag.store(true, Ordering::SeqCst);
                    self.status_message = Some("Cancelling...".to_string());
                } else if matches!(self.phase, AppPhase::ConfirmExtract { .. }) {
                    debug!("Esc during ConfirmExtract → Browsing");
                    self.phase = AppPhase::Browsing;
                } else {
                    debug!("Esc outside Extracting → quit");
                    self.should_quit = true;
                }
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
            KeyCode::Char(' ') => {
                if matches!(self.phase, AppPhase::Browsing) && !self.entries.is_empty() {
                    if let Some(entry) = self.entries.get(self.selected)
                        && !entry.is_directory
                    {
                        let idx = self.selected;
                        if self.toggled.contains(&idx) {
                            self.toggled.remove(&idx);
                        } else {
                            self.toggled.insert(idx);
                        }
                    }
                    // Advance cursor down (like most file managers).
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Char('a') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    let non_dir_indices: Vec<usize> = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| !e.is_directory)
                        .map(|(i, _)| i)
                        .collect();
                    let all_selected = non_dir_indices.len() == self.toggled.len()
                        && non_dir_indices.iter().all(|i| self.toggled.contains(i));
                    if all_selected {
                        self.toggled.clear();
                    } else {
                        self.toggled = non_dir_indices.into_iter().collect();
                    }
                }
            }
            KeyCode::Enter => {
                if matches!(self.phase, AppPhase::ConfirmExtract { .. }) {
                    // User confirmed — proceed with extraction.
                    let idx = if let AppPhase::ConfirmExtract { entry_index, .. } = &self.phase {
                        *entry_index
                    } else {
                        unreachable!()
                    };
                    self.start_extraction(idx, tx);
                } else if matches!(self.phase, AppPhase::Browsing) {
                    if !self.toggled.is_empty() {
                        self.start_batch_extraction(tx);
                    } else if let Some(entry) = self.entries.get(self.selected) {
                        if entry.is_directory {
                            return;
                        }
                        // Check dictionary size — warn if > 4 GB.
                        const DICT_WARN_THRESHOLD: u64 = 4 * 1024 * 1024 * 1024;
                        let dict = self.archive.as_ref().unwrap().dict_size(entry);
                        if let Some(ds) = dict
                            && ds > DICT_WARN_THRESHOLD
                        {
                            info!(
                                name = %entry.name,
                                dict_size = ds,
                                "large dictionary → ConfirmExtract"
                            );
                            self.phase = AppPhase::ConfirmExtract {
                                entry_index: self.selected,
                                entry_name: entry.name.clone(),
                                dict_size: ds,
                            };
                            return;
                        }
                        self.start_extraction(self.selected, tx);
                    }
                }
            }
            _ => {}
        }
    }

    /// Spawn the extraction task for the entry at `idx`.
    fn start_extraction(
        &mut self,
        idx: usize,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        let entry = &self.entries[idx];
        let entry_name = entry.name.clone();
        let total_bytes = entry.uncompressed_size;

        info!(name = %entry_name, total_bytes, "starting extraction → Extracting");

        self.phase = AppPhase::Extracting {
            entry_name: entry_name.clone(),
            total_bytes,
            bytes_extracted: 0,
        };

        let output_dir = crate::default_output_folder(&self.source)
            .unwrap_or_else(|| std::path::PathBuf::from("extracted"));
        let output_path = output_dir.join(&entry_name);

        let archive = self.archive.as_ref().unwrap().clone();
        let entry = self.entries[idx].clone();
        let tx_extract = tx.clone();
        self.cancel_flag.store(false, Ordering::SeqCst);
        self.progress.bytes_extracted.store(0, Ordering::Relaxed);
        let progress = Arc::new(Mutex::new(TuiProgressReporter::new(
            self.progress.clone(),
            self.cancel_flag.clone(),
        )));

        debug!(output = %output_path.display(), "spawning extraction task");
        tokio::spawn(async move {
            if let Some(parent) = output_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::File::create(&output_path).await {
                Ok(mut file) => {
                    let result = archive.extract(&entry, &mut file, progress, None).await;
                    if matches!(&result, Err(ArchiveError::Cancelled)) {
                        drop(file);
                        let _ = tokio::fs::remove_file(&output_path).await;
                    }
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

    /// Spawn a batch extraction task for all toggled entries.
    fn start_batch_extraction(&mut self, tx: &mpsc::UnboundedSender<AppMessage>) {
        let selected_entries: Vec<ArchiveEntry> = self
            .toggled
            .iter()
            .filter_map(|&i| self.entries.get(i).cloned())
            .filter(|e| !e.is_directory)
            .collect();

        if selected_entries.is_empty() {
            self.status_message = Some("No files selected.".to_string());
            return;
        }

        let total_bytes: u64 = selected_entries.iter().map(|e| e.uncompressed_size).sum();
        let total_files = selected_entries.len();

        info!(total_files, total_bytes, "starting batch extraction → ExtractingBatch");

        self.phase = AppPhase::ExtractingBatch {
            total_files,
            files_done: 0,
            current_entry: String::new(),
            total_bytes,
            bytes_extracted: 0,
        };

        let output_dir = crate::default_output_folder(&self.source)
            .unwrap_or_else(|| std::path::PathBuf::from("extracted"));

        let archive = self.archive.as_ref().unwrap().clone();
        let tx_extract = tx.clone();
        self.cancel_flag.store(false, Ordering::SeqCst);
        self.progress.bytes_extracted.store(0, Ordering::Relaxed);
        self.progress.files_done.store(0, Ordering::Relaxed);
        *self.progress.current_entry_name.lock().unwrap() = String::new();

        let progress = Arc::new(Mutex::new(TuiProgressReporter::new(
            self.progress.clone(),
            self.cancel_flag.clone(),
        )));

        tokio::spawn(async move {
            tokio::fs::create_dir_all(&output_dir).await.ok();

            let entry_refs: Vec<&ArchiveEntry> = selected_entries.iter().collect();

            let sink_factory = {
                let output_dir = output_dir.clone();
                move |entry: &ArchiveEntry| -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> {
                    // Path traversal guard: reject absolute paths and `..` components.
                    let entry_path = std::path::Path::new(&entry.name);
                    if entry_path.is_absolute()
                        || entry_path
                            .components()
                            .any(|c| c == std::path::Component::ParentDir)
                    {
                        return None;
                    }
                    let dest = output_dir.join(&entry.name);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let file = std::fs::File::create(&dest).ok()?;
                    Some(Box::new(tokio::fs::File::from_std(file)))
                }
            };

            let result =
                archive.extract_selected(&entry_refs, sink_factory, progress, None).await;

            // Clean up output files on cancel or per-entry failures.
            match &result {
                Err(ArchiveError::Cancelled) => {
                    for entry in &selected_entries {
                        let dest = output_dir.join(&entry.name);
                        let _ = tokio::fs::remove_file(&dest).await;
                    }
                }
                Ok(batch) => {
                    for (name, _) in &batch.failed {
                        let dest = output_dir.join(name);
                        let _ = tokio::fs::remove_file(&dest).await;
                    }
                }
                Err(_) => {}
            }

            let _ = tx_extract.send(AppMessage::BatchExtractionComplete(result));
        });
    }
}

/// Progress reporter that writes to shared atomics.
///
/// The main loop reads these on each tick, so no channel messages are needed
/// for high-frequency progress updates.
pub struct TuiProgressReporter {
    shared: Arc<SharedProgress>,
    cancel: Arc<AtomicBool>,
    /// Bytes from all previously completed entries (used for cumulative progress in batch mode).
    cumulative_bytes: u64,
    /// Bytes written for the current entry so far (updated by on_bytes_progress).
    current_entry_bytes: u64,
}

impl TuiProgressReporter {
    pub fn new(shared: Arc<SharedProgress>, cancel: Arc<AtomicBool>) -> Self {
        Self { shared, cancel, cumulative_bytes: 0, current_entry_bytes: 0 }
    }
}

impl ProgressReporter for TuiProgressReporter {
    fn on_scan_progress(&mut self, bytes_scanned: u64, entries_found: usize) {
        self.shared.bytes_scanned.store(bytes_scanned, Ordering::Relaxed);
        self.shared.entries_found.store(entries_found, Ordering::Relaxed);
    }

    fn on_entry_start(&mut self, name: &str, _size: u64) {
        self.current_entry_bytes = 0;
        *self.shared.current_entry_name.lock().unwrap() = name.to_string();
    }

    fn on_bytes_progress(&mut self, bytes_so_far: u64) {
        self.current_entry_bytes = bytes_so_far;
        self.shared
            .bytes_extracted
            .store(self.cumulative_bytes + bytes_so_far, Ordering::Relaxed);
    }

    fn on_entry_complete(&mut self, _name: &str, _success: bool) {
        // Accumulate bytes from this entry into the running total for batch progress.
        self.cumulative_bytes += self.current_entry_bytes;
        self.current_entry_bytes = 0;
        // Increment regardless of success — the progress bar tracks completions, not successes.
        self.shared.files_done.fetch_add(1, Ordering::Relaxed);
    }

    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}
