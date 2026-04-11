#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use archive::{Archive, ArchiveEntry, ArchiveError, BatchResult, EntryTree, Opened, ProgressReporter, ScanInfo};
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

/// Tracks byte progress over time to compute rolling speed and ETA.
pub struct SpeedTracker {
    /// Ring buffer of (timestamp, cumulative_bytes) samples.
    samples: VecDeque<(Instant, u64)>,
    /// Window size for rolling average (in seconds).
    window_secs: f64,
}

impl SpeedTracker {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(128),
            window_secs: 3.0, // 3-second rolling window
        }
    }

    /// Record a new progress sample. Call on each tick.
    pub fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));

        // Evict samples older than the window.
        let cutoff = now - std::time::Duration::from_secs_f64(self.window_secs);
        while self.samples.len() > 2 {
            if self.samples[0].0 < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Current speed in bytes per second, or None if insufficient data.
    pub fn speed_bps(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let first = self.samples.front().unwrap();
        let last = self.samples.back().unwrap();
        let elapsed = last.0.duration_since(first.0).as_secs_f64();
        if elapsed < 0.1 {
            return None; // Not enough time elapsed
        }
        let bytes_delta = last.1.saturating_sub(first.1);
        Some(bytes_delta as f64 / elapsed)
    }

    /// Estimated seconds remaining, given total bytes expected.
    pub fn eta_secs(&self, total_bytes: u64, current_bytes: u64) -> Option<f64> {
        let speed = self.speed_bps()?;
        if speed < 1.0 {
            return None; // Too slow to estimate
        }
        let remaining = total_bytes.saturating_sub(current_bytes);
        Some(remaining as f64 / speed)
    }

    /// Human-readable speed string, or None if insufficient data.
    pub fn format_speed(&self) -> Option<String> {
        let bps = self.speed_bps()?;
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * KB;
        const GB: f64 = 1024.0 * MB;

        Some(if bps >= GB {
            format!("{:.1} GB/s", bps / GB)
        } else if bps >= MB {
            format!("{:.1} MB/s", bps / MB)
        } else if bps >= KB {
            format!("{:.0} KB/s", bps / KB)
        } else {
            format!("{:.0} B/s", bps)
        })
    }

    /// Human-readable ETA string ("M:SS" or "H:MM:SS"), or None if unavailable.
    pub fn format_eta(&self, total_bytes: u64, current_bytes: u64) -> Option<String> {
        let secs = self.eta_secs(total_bytes, current_bytes)?;
        let total_secs = secs.ceil() as u64;
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        let seconds = total_secs % 60;

        Some(if hours > 0 {
            format!("ETA {}:{:02}:{:02}", hours, minutes, seconds)
        } else {
            format!("ETA {}:{:02}", minutes, seconds)
        })
    }

    /// Reset for a new extraction.
    pub fn reset(&mut self) {
        self.samples.clear();
    }
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
    /// Prompting user for a password before extraction.
    PasswordPrompt {
        /// What to retry after password entry.
        /// `false` = single file extraction (uses `entry_index`).
        /// `true` = batch extraction.
        is_batch: bool,
        /// For single-file extraction, the entry index to retry.
        entry_index: Option<usize>,
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

/// A single item visible in the current directory view.
pub struct DisplayItem {
    /// Segment name for display (e.g. "cat.jpg" or "photos").
    pub name: String,
    /// Full archive path for selection tracking (e.g. "photos/cat.jpg").
    pub full_path: String,
    pub is_directory: bool,
    /// Index into `App::entries`, if this is a real archive entry.
    pub entry_index: Option<usize>,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
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
    /// Full archive paths toggled for batch extraction.
    pub toggled: HashSet<String>,
    /// Shared cancel flag — set by Esc during extraction, read by ProgressReporter.
    pub cancel_flag: Arc<AtomicBool>,
    /// Shared progress counters — written by ProgressReporter, read on tick.
    pub progress: Arc<SharedProgress>,
    /// Virtual folder tree built once from entries.
    pub entry_tree: Option<EntryTree>,
    /// Breadcrumb path segments (e.g. ["photos", "vacation"]).
    pub current_path: Vec<String>,
    /// Items visible in the current directory.
    pub visible_items: Vec<DisplayItem>,
    /// Last selected index for each visited path (e.g. "" -> 2, "photos" -> 5).
    pub selection_history: std::collections::HashMap<String, usize>,
    /// Whether to show the help overlay.
    pub show_help: bool,
    /// The current search filter query.
    pub filter_query: String,
    /// Whether the user is currently typing into the filter.
    pub is_filtering: bool,
    /// Whether the Tab Review mode is active (shows only selected files).
    pub in_review_mode: bool,
    /// Index of the highlighted item within the review list.
    pub review_selected: usize,
    /// Tracks extraction speed and ETA.
    pub speed_tracker: SpeedTracker,
    /// Session password — once entered, reused for all subsequent extractions.
    pub password: Option<Vec<u8>>,
    /// Text buffer for the password input field.
    pub password_input: String,
    /// Error message to show above the password prompt (e.g., "Wrong password").
    pub password_error: Option<String>,
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
            entry_tree: None,
            current_path: Vec::new(),
            visible_items: Vec::new(),
            selection_history: std::collections::HashMap::new(),
            show_help: false,
            filter_query: String::new(),
            is_filtering: false,
            in_review_mode: false,
            review_selected: 0,
            speed_tracker: SpeedTracker::new(),
            password: None,
            password_input: String::new(),
            password_error: None,
        }
    }

    /// Rebuild the visible items list from the entry tree at the current path.
    /// Called after navigation (dive in / back out) and after initial tree build.
    /// If `target` is provided, attempts to select the item with that name.
    pub fn rebuild_visible_items(&mut self, target: Option<&str>) {
        // Phase 1: collect lightweight node data from the tree (borrows self.entry_tree).
        let node_data: Vec<(String, String, bool, Option<usize>)> = {
            let Some(tree) = &self.entry_tree else { return };
            let path_str = self.current_path.join("/");
            let children = tree.children_at(&path_str);
            let query = self.filter_query.to_lowercase();

            children
                .iter()
                .filter_map(|node| {
                    if !query.is_empty() && !node.name.to_lowercase().contains(&query) {
                        return None;
                    }

                    let full_path = if path_str.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{}/{}", path_str, node.name)
                    };
                    let is_directory = !node.children.is_empty();
                    Some((node.name.clone(), full_path, is_directory, node.entry_index))
                })
                .collect()
        };
        // Tree borrow dropped here.

        // Phase 2: enrich with entry data (borrows self.entries).
        let mut items: Vec<DisplayItem> = node_data
            .into_iter()
            .map(|(name, full_path, is_directory, entry_index)| {
                let (uncompressed_size, compressed_size) = if !is_directory {
                    entry_index
                        .map(|idx| {
                            let e = &self.entries[idx];
                            (e.uncompressed_size, e.compressed_size)
                        })
                        .unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                DisplayItem {
                    name,
                    full_path,
                    is_directory,
                    entry_index,
                    uncompressed_size,
                    compressed_size,
                }
            })
            .collect();

        // Sort: directories first (alphabetical), then files (alphabetical).
        items.sort_by(|a, b| match (a.is_directory, b.is_directory) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        // If a target was requested (backing out), find its index.
        // Else, check the history for the current folder.
        // Otherwise, default to 0.
        let new_selected = if let Some(t) = target {
            items.iter().position(|item| item.name == t).unwrap_or(0)
        } else {
            let path_str = self.current_path.join("/");
            self.selection_history.get(&path_str).cloned().unwrap_or(0)
        };

        self.visible_items = items;
        self.selected = new_selected;
        self.table_state.select(
            if self.visible_items.is_empty() {
                None
            } else {
                Some(new_selected)
            },
        );
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
                self.entry_tree = Some(EntryTree::build(&self.entries));
                self.current_path.clear();
                self.rebuild_visible_items(None);
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
                self.entry_tree = Some(EntryTree::build(&self.entries));
                self.current_path.clear();
                self.rebuild_visible_items(None);
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
                if matches!(e, ArchiveError::PasswordNeeded) {
                    // Password was wrong — clear it and re-prompt.
                    self.password = None;
                    self.password_input.clear();

                    // Figure out the entry index from the current phase.
                    let entry_index = if let AppPhase::Extracting { ref entry_name, .. } = self.phase
                    {
                        self.entries.iter().position(|e| e.name == *entry_name)
                    } else {
                        None
                    };

                    self.password_error = Some("Wrong password. Try again.".into());
                    self.phase = AppPhase::PasswordPrompt { is_batch: false, entry_index };
                    return;
                }
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
                // Check if failures are all password-related.
                let password_failures: Vec<&String> = batch
                    .failed
                    .iter()
                    .filter(|(_, err)| err.to_lowercase().contains("password"))
                    .map(|(name, _)| name)
                    .collect();

                if !password_failures.is_empty() {
                    // Some entries failed due to wrong password.
                    // Remove successfully extracted entries from toggled so they won't be re-attempted.
                    let succeeded_names: HashSet<String> = self
                        .entries
                        .iter()
                        .filter(|e| self.toggled.contains(&e.name))
                        .filter(|e| !batch.failed.iter().any(|(name, _)| name == &e.name))
                        .map(|e| e.name.clone())
                        .collect();
                    for name in &succeeded_names {
                        self.toggled.remove(name);
                    }

                    self.password = None;

                    let msg = if batch.succeeded > 0 {
                        format!(
                            "Extracted {} file{} — {} failed (wrong password). Try again.",
                            batch.succeeded,
                            if batch.succeeded == 1 { "" } else { "s" },
                            password_failures.len(),
                        )
                    } else {
                        "Wrong password. Try extracting again.".into()
                    };
                    self.status_message = Some(msg);
                    self.phase = AppPhase::Browsing;
                    return;
                }

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
                if matches!(e, ArchiveError::PasswordNeeded) {
                    // Batch aborted early due to password — no files succeeded.
                    self.password = None;
                    self.status_message = Some("Wrong password. Try extracting again.".into());
                    self.phase = AppPhase::Browsing;
                    return;
                }
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
                self.speed_tracker.record(*bytes_extracted);
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
                self.speed_tracker.record(*bytes_extracted);
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

        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_help = false;
                }
                _ => {}
            }
            return;
        }

        if self.is_filtering {
            match key.code {
                KeyCode::Char(c) => {
                    self.filter_query.push(c);
                    self.rebuild_visible_items(None);
                }
                KeyCode::Backspace => {
                    self.filter_query.pop();
                    self.rebuild_visible_items(None);
                }
                KeyCode::Enter | KeyCode::Esc => {
                    self.is_filtering = false;
                }
                _ => {}
            }
            return;
        }

        if self.in_review_mode {
            match key.code {
                KeyCode::Tab | KeyCode::Esc => {
                    self.in_review_mode = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.review_selected = self.review_selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = self.toggled.len();
                    if count > 0 {
                        self.review_selected = (self.review_selected + 1).min(count - 1);
                    }
                }
                KeyCode::Char(' ') => {
                    // Remove the highlighted item from the selection.
                    // Items are sorted alphabetically by full path in the UI.
                    let mut review_items: Vec<String> = self
                        .entries
                        .iter()
                        .filter(|e| self.toggled.contains(&e.name))
                        .map(|e| e.name.clone())
                        .collect();
                    review_items.sort_by_key(|a| a.to_lowercase());

                    if let Some(name) = review_items.get(self.review_selected) {
                        self.toggled.remove(name);
                    }
                    // If selection is now empty, exit review mode.
                    if self.toggled.is_empty() {
                        self.in_review_mode = false;
                    } else {
                        // Clamp review_selected to the new count.
                        let new_count = self.toggled.len();
                        if self.review_selected >= new_count {
                            self.review_selected = new_count.saturating_sub(1);
                        }
                    }
                }
                KeyCode::Enter => {
                    // Start batch extraction from review mode.
                    self.in_review_mode = false;
                    self.start_batch_extraction(tx);
                }
                KeyCode::Char('q') => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        if matches!(self.phase, AppPhase::PasswordPrompt { .. }) {
            match key.code {
                KeyCode::Char(c) => {
                    self.password_input.push(c);
                }
                KeyCode::Backspace => {
                    self.password_input.pop();
                }
                KeyCode::Enter => {
                    if self.password_input.is_empty() {
                        return; // Don't accept empty password
                    }
                    // Store the password for the session.
                    let pw = self.password_input.as_bytes().to_vec();
                    self.password = Some(pw);
                    self.password_input.clear();

                    // Extract the phase data before replacing it.
                    let (is_batch, entry_index) = if let AppPhase::PasswordPrompt {
                        is_batch,
                        entry_index,
                    } = &self.phase
                    {
                        (*is_batch, *entry_index)
                    } else {
                        unreachable!()
                    };

                    // Resume the extraction that was interrupted.
                    if is_batch {
                        self.start_batch_extraction(tx);
                    } else if let Some(idx) = entry_index {
                        const DICT_WARN_THRESHOLD: u64 = 4 * 1024 * 1024 * 1024;
                        let dict = self.archive.as_ref().unwrap().dict_size(&self.entries[idx]);
                        if let Some(ds) = dict
                            && ds > DICT_WARN_THRESHOLD
                        {
                            self.phase = AppPhase::ConfirmExtract {
                                entry_index: idx,
                                entry_name: self.entries[idx].name.clone(),
                                dict_size: ds,
                            };
                        } else {
                            self.start_extraction(idx, tx);
                        }
                    }
                }
                KeyCode::Esc => {
                    // Cancel password entry, return to browsing.
                    self.password_input.clear();
                    self.password_error = None;
                    self.phase = AppPhase::Browsing;
                }
                _ => {}
            }
            return;
        }

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
                    debug!("Esc during extraction → cancelling");
                    self.cancel_flag.store(true, Ordering::SeqCst);
                    self.status_message = Some("Cancelling...".to_string());
                } else if matches!(self.phase, AppPhase::ConfirmExtract { .. }) {
                    debug!("Esc during ConfirmExtract → Browsing");
                    self.phase = AppPhase::Browsing;
                } else if matches!(self.phase, AppPhase::Browsing) && !self.current_path.is_empty() {
                    debug!("Esc in subdirectory → parent");
                    let old_path = self.current_path.join("/");
                    self.selection_history.insert(old_path, self.selected);
                    let target = self.current_path.pop();
                    self.filter_query.clear();
                    self.is_filtering = false;
                    self.rebuild_visible_items(target.as_deref());
                } else {
                    debug!("Esc at root → quit");
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
                if matches!(self.phase, AppPhase::Browsing) && !self.visible_items.is_empty() {
                    self.selected = (self.selected + 1).min(self.visible_items.len() - 1);
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
                if matches!(self.phase, AppPhase::Browsing) && !self.visible_items.is_empty() {
                    self.selected = self.visible_items.len() - 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Char(' ') => {
                if matches!(self.phase, AppPhase::Browsing) && !self.visible_items.is_empty() {
                    let item = &self.visible_items[self.selected];
                    if !item.is_directory {
                        let path = item.full_path.clone();
                        if self.toggled.contains(&path) {
                            self.toggled.remove(&path);
                        } else {
                            self.toggled.insert(path);
                        }
                    }
                    // Advance cursor down (like most file managers).
                    self.selected = (self.selected + 1).min(self.visible_items.len() - 1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if matches!(self.phase, AppPhase::Browsing) && !self.visible_items.is_empty() {
                    let item = &self.visible_items[self.selected];
                    if item.is_directory {
                        let old_path = self.current_path.join("/");
                        self.selection_history.insert(old_path, self.selected);
                        self.current_path.push(item.name.clone());
                        self.filter_query.clear();
                        self.is_filtering = false;
                        self.rebuild_visible_items(None);
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                if matches!(self.phase, AppPhase::Browsing) && !self.current_path.is_empty() {
                    let old_path = self.current_path.join("/");
                    self.selection_history.insert(old_path, self.selected);
                    let target = self.current_path.pop();
                    self.filter_query.clear();
                    self.is_filtering = false;
                    self.rebuild_visible_items(target.as_deref());
                }
            }
            KeyCode::Char('a') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    let file_paths: Vec<String> = self
                        .visible_items
                        .iter()
                        .filter(|item| !item.is_directory)
                        .map(|item| item.full_path.clone())
                        .collect();
                    let all_selected = !file_paths.is_empty()
                        && file_paths.iter().all(|p| self.toggled.contains(p));
                    if all_selected {
                        for p in &file_paths {
                            self.toggled.remove(p);
                        }
                    } else {
                        for p in file_paths {
                            self.toggled.insert(p);
                        }
                    }
                }
            }
            KeyCode::Char('?') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    self.show_help = !self.show_help;
                }
            }
            KeyCode::Tab => {
                if matches!(self.phase, AppPhase::Browsing) && !self.toggled.is_empty() {
                    self.in_review_mode = true;
                    self.review_selected = 0;
                }
            }
            KeyCode::Char('/') => {
                if matches!(self.phase, AppPhase::Browsing) {
                    self.is_filtering = true;
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
                        // Check if ANY toggled entry is encrypted.
                        let has_encrypted = self.entries.iter().any(|e| {
                            self.toggled.contains(&e.name) && e.is_encrypted && self.password.is_none()
                        });
                        if has_encrypted {
                            self.password_input.clear();
                            self.password_error = None;
                            self.phase = AppPhase::PasswordPrompt {
                                is_batch: true,
                                entry_index: None,
                            };
                            return;
                        }
                        self.start_batch_extraction(tx);
                    } else if let Some(item) = self.visible_items.get(self.selected) {
                        if item.is_directory {
                            let old_path = self.current_path.join("/");
                            self.selection_history.insert(old_path, self.selected);
                            self.current_path.push(item.name.clone());
                            self.filter_query.clear();
                            self.is_filtering = false;
                            self.rebuild_visible_items(None);
                            return;
                        }
                        if let Some(idx) = item.entry_index {
                            let entry = &self.entries[idx];
                            if entry.is_encrypted && self.password.is_none() {
                                self.password_input.clear();
                                self.password_error = None;
                                self.phase = AppPhase::PasswordPrompt {
                                    is_batch: false,
                                    entry_index: Some(idx),
                                };
                                return;
                            }

                            const DICT_WARN_THRESHOLD: u64 = 4 * 1024 * 1024 * 1024;
                            let dict =
                                self.archive.as_ref().unwrap().dict_size(&self.entries[idx]);
                            if let Some(ds) = dict
                                && ds > DICT_WARN_THRESHOLD
                            {
                                info!(
                                    name = %self.entries[idx].name,
                                    dict_size = ds,
                                    "large dictionary → ConfirmExtract"
                                );
                                self.phase = AppPhase::ConfirmExtract {
                                    entry_index: idx,
                                    entry_name: self.entries[idx].name.clone(),
                                    dict_size: ds,
                                };
                                return;
                            }
                            self.start_extraction(idx, tx);
                        }
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
        self.speed_tracker.reset();
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
        let password = self.password.clone();

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
                    let result =
                        archive.extract(&entry, &mut file, progress, password.as_deref()).await;
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
        self.speed_tracker.reset();
        let selected_entries: Vec<ArchiveEntry> = self
            .entries
            .iter()
            .filter(|e| !e.is_directory && self.toggled.contains(&e.name))
            .cloned()
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
        let password = self.password.clone();

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
                archive.extract_selected(&entry_refs, sink_factory, progress, password.as_deref()).await;

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
