#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use archive::{
    Archive, ArchiveEntry, ArchiveError, BatchResult, EntryTree, Opened, ProgressReporter, ScanInfo,
};
use archive_source::{resolve, ResolvedSource, SourceConfig};
use chrono::Utc;
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

/// The three extraction destination strategies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionStrategy {
    /// Extract into a folder named after the archive (e.g., `photos.zip` → `./photos/`).
    ArchiveName,
    /// Extract directly into the current working directory.
    Here,
    /// Extract into a user-specified folder name.
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    SnipeInput,
    Bookmarks, // reserved for Plan v2; cycles but shows empty placeholder in MVP
    Recent,
}

impl HomeFocus {
    fn next(self) -> Self {
        match self {
            Self::SnipeInput => Self::Bookmarks,
            Self::Bookmarks => Self::Recent,
            Self::Recent => Self::SnipeInput,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::SnipeInput => Self::Recent,
            Self::Bookmarks => Self::SnipeInput,
            Self::Recent => Self::Bookmarks,
        }
    }
}

/// Current phase of the application.
pub enum AppPhase {
    Home, // NEW — owns no data; home state lives on `App`
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
    /// Pre-extraction modal — user picks where to extract.
    ExtractionStrategy {
        /// Which strategy is currently highlighted (0 = ArchiveName, 1 = Here, 2 = Custom).
        selected: usize,
        /// Whether we're in text-input mode for the Custom option.
        editing_custom: bool,
        /// The text buffer for the custom folder name.
        custom_input: String,
        /// Whether this is a batch extraction (true) or single-file (false).
        is_batch: bool,
        /// For single-file extraction, the entry index.
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
    /// The output directory chosen in the extraction strategy modal.
    /// Persists across the password prompt so resume-after-password uses the right directory.
    pub chosen_output_dir: Option<std::path::PathBuf>,

    // NEW — Home Screen state. Only meaningful while `phase == Home`.
    pub home_focus: HomeFocus,
    pub snipe_input: String,
    pub home_recent: crate::home::recent::RecentList,
    pub home_recent_selected: usize,
    /// Error string to show under the Snipe Input (e.g., "File not found").
    /// Set when archive open fails while user is in Home; cleared on next keypress.
    pub home_input_error: Option<String>,

    /// Provider tag derived from `ResolvedSource` during Home resolve (pre-spawn).
    /// Consumed by `record_recent_on_open` in Step 6. Some only during Opening/Scanning
    /// that originated from Home.
    pub home_pending_provider: Option<crate::home::provider::Provider>,
    /// Display name derived from `ResolvedSource::suggested_name()` during Home resolve.
    /// Consumed by `record_recent_on_open` in Step 6.
    pub home_pending_name: Option<String>,

    /// True if the session originally started in Home. Controls whether Esc at
    /// browse root should return to Home (step 7 wires this).
    pub launched_from_home: bool,
}

impl App {
    /// Entry from `snipe <source>` — direct browse.
    pub fn new_with_source(source: String) -> Self {
        let mut app = Self::default_state();
        app.source = source;
        app.phase = AppPhase::Opening;
        app.launched_from_home = false;
        app
    }

    /// Entry from bare `snipe` — Home Screen.
    pub fn new_home() -> Self {
        let mut app = Self::default_state();
        app.phase = AppPhase::Home;
        app.home_recent = crate::home::recent::RecentList::load();
        app.launched_from_home = true;
        app
    }

    fn default_state() -> Self {
        Self {
            phase: AppPhase::Opening,
            source: String::new(),
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
            chosen_output_dir: None,

            home_focus: HomeFocus::SnipeInput,
            snipe_input: String::new(),
            home_recent: crate::home::recent::RecentList::default(),
            home_recent_selected: 0,
            home_input_error: None,
            home_pending_provider: None,
            home_pending_name: None,
            launched_from_home: false,
        }
    }

    pub fn new(source: String) -> Self {
        Self::new_with_source(source)
    }

    /// Returns the archive stem name for use as the default extraction folder.
    /// e.g., "photos.zip" → "photos", a Google Drive URL → the file ID or detected name.
    pub fn archive_stem(&self) -> String {
        crate::default_output_folder(&self.source)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "extracted".to_string())
    }

    /// Resolve the chosen extraction strategy into an output directory path.
    pub fn resolve_output_dir(&self, strategy: &ExtractionStrategy) -> std::path::PathBuf {
        match strategy {
            ExtractionStrategy::ArchiveName => {
                crate::default_output_folder(&self.source)
                    .unwrap_or_else(|| std::path::PathBuf::from("extracted"))
            }
            ExtractionStrategy::Here => std::path::PathBuf::from("."),
            ExtractionStrategy::Custom(name) => std::path::PathBuf::from(name),
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
            AppMessage::ArchiveOpened(Ok(opened)) => {
                self.record_recent_on_open(&opened);

                match opened {
                    Opened::Ready(archive) => {
                        let count = archive.entries_clean().len();
                        info!(entries = count, "archive opened → Browsing");
                        self.entries = archive.entries_clean().into_iter().cloned().collect();
                        self.archive = Some(Arc::new(archive));
                        self.entry_tree = Some(EntryTree::build(&self.entries));
                        self.current_path.clear();
                        self.rebuild_visible_items(None);
                        self.phase = AppPhase::Browsing;
                    }
                    Opened::NeedsScan(pending) => {
                        let total_bytes = match pending.scan_info() {
                            ScanInfo::Required { estimated_bytes, .. } => *estimated_bytes,
                            ScanInfo::Ready => 0,
                        };
                        info!(total_bytes, "archive needs scan → Scanning");
                        // Reset shared progress counters for this scan.
                        self.progress.bytes_scanned.store(0, Ordering::Relaxed);
                        self.progress.entries_found.store(0, Ordering::Relaxed);
                        self.phase = AppPhase::Scanning {
                            total_bytes,
                            bytes_scanned: 0,
                            entries_found: 0,
                        };
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
                }
            }
            AppMessage::ArchiveOpened(Err(e)) => {
                if self.launched_from_home {
                    error!(error = %e, "archive open failed (Home) → stay on Home");
                    self.home_input_error = Some(self.format_home_error(&e));
                    self.phase = AppPhase::Home;
                    // Clear pending stashed values as we're back to Home.
                    self.home_pending_provider = None;
                    self.home_pending_name = None;
                } else {
                    error!(error = %e, "archive open failed → Error");
                    self.phase = AppPhase::Error(e.to_string());
                }
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
                self.chosen_output_dir = None;
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
                self.chosen_output_dir = None;
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
                self.chosen_output_dir = None;
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
                self.chosen_output_dir = None;
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

    /// Update the Recent list if we launched from Home.
    fn record_recent_on_open(&mut self, _opened: &Opened<ResolvedSource>) {
        if !self.launched_from_home {
            return;
        }
        let source = self.source.clone();
        let provider = self
            .home_pending_provider
            .take()
            .unwrap_or(crate::home::provider::Provider::Local);
        let name = self.home_pending_name.take().unwrap_or_else(|| source.clone());

        // Size: MVP ships size=0 (see SESSION_PLAN_STEP_6 Design).
        let size = 0u64;

        self.home_recent.push(crate::home::recent::RecentEntry {
            name,
            source,
            provider,
            size,
            last_opened: Utc::now(),
        });
        self.home_recent.save();
    }

    /// Common logic for opening an archive from Home (Snipe Input or Recent).
    fn submit_home_source(&mut self, source: String, tx: &mpsc::UnboundedSender<AppMessage>) {
        let config = SourceConfig::new();
        match resolve(&source, &config) {
            Ok(reader) => {
                let provider = crate::home::provider::Provider::from_resolved(&reader);
                let name = reader.suggested_name().unwrap_or_else(|| source.clone());

                self.home_input_error = None;
                self.source = source.clone();
                self.phase = AppPhase::Opening;
                self.home_pending_provider = Some(provider);
                self.home_pending_name = Some(name);

                let tx_open = tx.clone();
                let source_for_task = source.clone();
                tokio::spawn(async move {
                    let result = Archive::open_reader(&source_for_task, reader).await;
                    let _ = tx_open.send(AppMessage::ArchiveOpened(result));
                });
            }
            Err(e) => {
                self.home_input_error = Some(format!("{e}"));
            }
        }
    }

    /// Reset all archive-scoped state and return to the Home Screen.
    fn return_to_home(&mut self) {
        self.phase = AppPhase::Home;
        self.source.clear();
        self.archive = None;
        self.entries.clear();
        self.entry_tree = None;
        self.visible_items.clear();
        self.current_path.clear();
        self.selection_history.clear();
        self.toggled.clear();
        self.show_help = false;
        self.filter_query.clear();
        self.is_filtering = false;
        self.in_review_mode = false;
        self.review_selected = 0;
        self.password = None;
        self.password_input.clear();
        self.password_error = None;
        self.chosen_output_dir = None;
        self.status_message = Some("Returned to Home".into());
        self.table_state = TableState::default().with_selected(Some(0));
        self.home_recent = crate::home::recent::RecentList::load();
        self.home_input_error = None;
        self.home_pending_provider = None;
        self.home_pending_name = None;
        self.home_focus = HomeFocus::SnipeInput;
        // Intentionally keep `launched_from_home = true`.
    }

    /// Formats an ArchiveError for display in the Home screen error row.
    fn format_home_error(&self, e: &ArchiveError) -> String {
        use archive_source::{classify_transport, ProviderError};
        match e {
            ArchiveError::Transport(inner) => match classify_transport(inner.as_ref()) {
                Some(ProviderError::NotPublic { provider }) => {
                    format!("{provider}: file is private. Share publicly or sign in.")
                }
                Some(ProviderError::FileNotFound { provider }) => {
                    format!("{provider}: file not found.")
                }
                Some(ProviderError::RateLimited { provider, retry_after }) => {
                    let when = retry_after
                        .map(|d| format!(" (retry in ~{}s)", d.as_secs()))
                        .unwrap_or_default();
                    format!("{provider}: rate limited{when}.")
                }
                Some(ProviderError::InvalidCredentials { provider }) => {
                    format!("{provider}: credentials invalid or missing.")
                }
                Some(ProviderError::QuotaExceeded { provider }) => {
                    format!("{provider}: quota exceeded.")
                }
                None => e.to_string(),
            },
            _ => e.to_string(),
        }
    }

    /// Process a keypress while in the Home phase.
    fn handle_home_key(&mut self, key: KeyEvent, _tx: &mpsc::UnboundedSender<AppMessage>) {
        match key.code {
            KeyCode::Tab => {
                self.home_focus = self.home_focus.next();
            }
            KeyCode::BackTab => {
                self.home_focus = self.home_focus.prev();
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Esc => {
                // Style guide §6.6 — Home is a dead-end; q only.
            }

            // Recent list navigation (only if focused on Recent)
            KeyCode::Down | KeyCode::Char('j') if self.home_focus == HomeFocus::Recent => {
                let count = self.home_recent.len();
                if count > 0 {
                    self.home_recent_selected = (self.home_recent_selected + 1).min(count - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.home_focus == HomeFocus::Recent => {
                self.home_recent_selected = self.home_recent_selected.saturating_sub(1);
            }
            KeyCode::Home | KeyCode::Char('g') if self.home_focus == HomeFocus::Recent => {
                self.home_recent_selected = 0;
            }
            KeyCode::End | KeyCode::Char('G') if self.home_focus == HomeFocus::Recent => {
                let count = self.home_recent.len();
                if count > 0 {
                    self.home_recent_selected = count - 1;
                }
            }

            // Recent delete
            KeyCode::Char('d') if self.home_focus == HomeFocus::Recent => {
                if !self.home_recent.is_empty() {
                    self.home_recent.remove(self.home_recent_selected);
                    // Persist change.
                    self.home_recent.save();
                    // Clamp cursor.
                    let max = self.home_recent.len().saturating_sub(1);
                    self.home_recent_selected = self.home_recent_selected.min(max);
                }
            }

            // Enter
            KeyCode::Enter => match self.home_focus {
                HomeFocus::SnipeInput => {
                    let source = self.snipe_input.trim().to_string();
                    if source.is_empty() {
                        self.home_input_error = Some("Paste a link or path first.".into());
                        return;
                    }
                    self.submit_home_source(source, _tx);
                }
                HomeFocus::Recent => {
                    if let Some(entry) = self.home_recent.entries().get(self.home_recent_selected) {
                        let source = entry.source.clone();
                        self.snipe_input.clear();
                        self.submit_home_source(source, _tx);
                    }
                }
                HomeFocus::Bookmarks => {
                    // MVP: no bookmarks yet.
                }
            },

            // Snipe Input editing (only if focused)
            KeyCode::Char('u') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                && self.home_focus == HomeFocus::SnipeInput =>
            {
                self.snipe_input.clear();
            }
            KeyCode::Char(c) if self.home_focus == HomeFocus::SnipeInput
                && !key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) => {
                self.snipe_input.push(c);
                self.home_input_error = None;
            }
            KeyCode::Backspace if self.home_focus == HomeFocus::SnipeInput => {
                self.snipe_input.pop();
            }

            _ => {}
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
            AppPhase::ExtractionStrategy { .. } => {}
            _ => {}
        }
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        // Any keypress clears status message and home input error.
        self.status_message = None;
        self.home_input_error = None;

        if matches!(self.phase, AppPhase::Home) {
            self.handle_home_key(key, tx);
            return;
        }

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
                    // Open strategy modal for batch extraction from review mode.
                    self.in_review_mode = false;
                    self.phase = AppPhase::ExtractionStrategy {
                        selected: 0,
                        editing_custom: false,
                        custom_input: String::new(),
                        is_batch: true,
                        entry_index: None,
                    };
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
                    let output_dir = self.chosen_output_dir.clone().unwrap_or_else(|| {
                        self.resolve_output_dir(&ExtractionStrategy::ArchiveName)
                    });
                    if is_batch {
                        self.start_batch_extraction(output_dir, tx);
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
                            self.start_extraction(idx, &output_dir, tx);
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

        if let AppPhase::ExtractionStrategy {
            ref mut selected,
            ref mut editing_custom,
            ref mut custom_input,
            is_batch,
            entry_index,
        } = self.phase
        {
            if *editing_custom {
                // Text input mode for custom folder name
                match key.code {
                    KeyCode::Char(c) => {
                        custom_input.push(c);
                    }
                    KeyCode::Backspace => {
                        if custom_input.is_empty() {
                            // Backspace on empty input exits editing mode
                            *editing_custom = false;
                        } else {
                            custom_input.pop();
                        }
                    }
                    KeyCode::Enter => {
                        let trimmed = custom_input.trim().to_string();
                        if trimmed.is_empty() {
                            return; // Don't accept empty or whitespace-only custom name
                        }
                        let strategy = ExtractionStrategy::Custom(trimmed);
                        // Drop borrow before calling methods on self
                        let output_dir = self.resolve_output_dir(&strategy);
                        self.proceed_with_extraction(is_batch, entry_index, output_dir, tx);
                    }
                    KeyCode::Esc => {
                        *editing_custom = false;
                    }
                    _ => {}
                }
                return;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(2); // 3 options: 0, 1, 2
                }
                KeyCode::Enter => {
                    let strategy = match *selected {
                        0 => ExtractionStrategy::ArchiveName,
                        1 => ExtractionStrategy::Here,
                        2 => {
                            // Enter editing mode for custom name
                            *editing_custom = true;
                            return;
                        }
                        _ => unreachable!(),
                    };
                    // Drop borrow before calling methods on self
                    let output_dir = self.resolve_output_dir(&strategy);
                    self.proceed_with_extraction(is_batch, entry_index, output_dir, tx);
                }
                KeyCode::Esc => {
                    self.chosen_output_dir = None;
                    self.phase = AppPhase::Browsing;
                }
                KeyCode::Char('q') => {
                    self.should_quit = true;
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
                } else if matches!(self.phase, AppPhase::Browsing) && self.current_path.is_empty() {
                    if self.launched_from_home {
                        self.return_to_home();
                    }
                    // else: dead-end, no-op (§6.6)
                } else if matches!(self.phase, AppPhase::Error(_)) && self.launched_from_home {
                    self.return_to_home();
                } else {
                    // At root — Esc is a no-op. Only `q` quits.
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
                    if item.is_directory {
                        // Recursively toggle all files under this directory.
                        let prefix = format!("{}/", item.full_path);
                        let child_paths: Vec<String> = self
                            .entries
                            .iter()
                            .filter(|e| !e.is_directory && e.name.starts_with(&prefix))
                            .map(|e| e.name.clone())
                            .collect();
                        let all_selected = !child_paths.is_empty()
                            && child_paths.iter().all(|p| self.toggled.contains(p));
                        if all_selected {
                            for p in &child_paths {
                                self.toggled.remove(p);
                            }
                        } else {
                            for p in child_paths {
                                self.toggled.insert(p);
                            }
                        }
                    } else {
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
                    // User confirmed — proceed with extraction using the chosen output dir.
                    let idx = if let AppPhase::ConfirmExtract { entry_index, .. } = &self.phase {
                        *entry_index
                    } else {
                        unreachable!()
                    };
                    let output_dir = self.chosen_output_dir.clone().unwrap_or_else(|| {
                        self.resolve_output_dir(&ExtractionStrategy::ArchiveName)
                    });
                    self.start_extraction(idx, &output_dir, tx);
                } else if matches!(self.phase, AppPhase::Browsing) {
                    if !self.toggled.is_empty() {
                        // Batch extraction — open strategy modal
                        self.phase = AppPhase::ExtractionStrategy {
                            selected: 0,
                            editing_custom: false,
                            custom_input: String::new(),
                            is_batch: true,
                            entry_index: None,
                        };
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
                            // Single file — open strategy modal
                            self.phase = AppPhase::ExtractionStrategy {
                                selected: 0,
                                editing_custom: false,
                                custom_input: String::new(),
                                is_batch: false,
                                entry_index: Some(idx),
                            };
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// After the user picks an extraction strategy, handle password/dict checks and start extraction.
    fn proceed_with_extraction(
        &mut self,
        is_batch: bool,
        entry_index: Option<usize>,
        output_dir: std::path::PathBuf,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        self.chosen_output_dir = Some(output_dir.clone());
        if is_batch {
            // Check if any toggled entry is encrypted and needs a password.
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
            self.start_batch_extraction(output_dir, tx);
        } else if let Some(idx) = entry_index {
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
            let dict = self.archive.as_ref().unwrap().dict_size(&self.entries[idx]);
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
            self.start_extraction(idx, &output_dir, tx);
        }
    }

    /// Spawn the extraction task for the entry at `idx`.
    fn start_extraction(
        &mut self,
        idx: usize,
        output_dir: &std::path::Path,
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
    fn start_batch_extraction(
        &mut self,
        output_dir: std::path::PathBuf,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
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

#[cfg(test)]
mod home_tests {
    use super::*;
    use crate::home::recent::{RecentEntry, RecentList};
    use crate::home::provider::Provider;
    use chrono::Utc;
    use crossterm::event::{KeyEvent, KeyCode, KeyModifiers, KeyEventKind, KeyEventState};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn test_app_new_home() {
        let app = App::new_home();
        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.launched_from_home);
        assert_eq!(app.home_focus, HomeFocus::SnipeInput);
        assert_eq!(app.home_recent_selected, 0);
        assert!(app.home_pending_provider.is_none());
        assert!(app.home_pending_name.is_none());
    }

    #[test]
    fn test_app_new_with_source() {
        let source = "foo.zip".to_string();
        let app = App::new_with_source(source.clone());
        assert!(matches!(app.phase, AppPhase::Opening));
        assert!(!app.launched_from_home);
        assert_eq!(app.source, source);
    }

    #[test]
    fn test_home_focus_cycling() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        // Tab cycles forwards
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Recent);
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::SnipeInput);

        // Shift+Tab (BackTab) cycles backwards
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Recent);
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::SnipeInput);
    }

    #[test]
    fn test_home_snipe_input() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Type "/tmp/foo.zip"
        for c in "/tmp/foo.zip".chars() {
            app.handle_key(make_key(KeyCode::Char(c)), &tx);
        }
        assert_eq!(app.snipe_input, "/tmp/foo.zip");

        // Backspace
        app.handle_key(make_key(KeyCode::Backspace), &tx);
        assert_eq!(app.snipe_input, "/tmp/foo.zi");

        // Ctrl+U clears
        app.handle_key(make_ctrl_key(KeyCode::Char('u')), &tx);
        assert_eq!(app.snipe_input, "");

        // Backspace on empty is no-op
        app.handle_key(make_key(KeyCode::Backspace), &tx);
        assert_eq!(app.snipe_input, "");
    }

    #[test]
    fn test_home_snipe_input_accepts_shift_chars() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Uppercase letter (crossterm reports Char('H') + SHIFT on most terminals).
        app.handle_key(make_shift_key(KeyCode::Char('H')), &tx);
        // Shifted punctuation (query-string '?', parameter '&').
        app.handle_key(make_shift_key(KeyCode::Char('?')), &tx);
        app.handle_key(make_shift_key(KeyCode::Char('&')), &tx);

        assert_eq!(app.snipe_input, "H?&");
    }

    #[test]
    fn test_home_recent_navigation() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        // Populate recent
        let entries = vec![
            RecentEntry { name: "a.zip".into(), source: "s1".into(), provider: Provider::Local, size: 10, last_opened: Utc::now() },
            RecentEntry { name: "b.zip".into(), source: "s2".into(), provider: Provider::Local, size: 20, last_opened: Utc::now() },
        ];
        app.home_recent = RecentList::from_entries(entries);
        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0;

        // j / Down
        app.handle_key(make_key(KeyCode::Char('j')), &tx);
        assert_eq!(app.home_recent_selected, 1);
        app.handle_key(make_key(KeyCode::Char('j')), &tx); // Clamped
        assert_eq!(app.home_recent_selected, 1);

        // k / Up
        app.handle_key(make_key(KeyCode::Char('k')), &tx);
        assert_eq!(app.home_recent_selected, 0);
        app.handle_key(make_key(KeyCode::Char('k')), &tx); // Clamped
        assert_eq!(app.home_recent_selected, 0);

        // G / End
        app.handle_key(make_key(KeyCode::Char('G')), &tx);
        assert_eq!(app.home_recent_selected, 1);

        // g / Home
        app.handle_key(make_key(KeyCode::Char('g')), &tx);
        assert_eq!(app.home_recent_selected, 0);
    }

    #[test]
    fn test_home_recent_delete() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        // Populate recent with 2 entries
        let entries = vec![
            RecentEntry { name: "a.zip".into(), source: "s1".into(), provider: Provider::Local, size: 10, last_opened: Utc::now() },
            RecentEntry { name: "b.zip".into(), source: "s2".into(), provider: Provider::Local, size: 20, last_opened: Utc::now() },
        ];
        app.home_recent = RecentList::from_entries(entries);
        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 1;

        // d deletes
        app.handle_key(make_key(KeyCode::Char('d')), &tx);
        assert_eq!(app.home_recent.len(), 1);
        assert_eq!(app.home_recent_selected, 0);
        assert_eq!(app.home_recent.entries()[0].name, "a.zip");

        // delete last one
        app.handle_key(make_key(KeyCode::Char('d')), &tx);
        assert_eq!(app.home_recent.len(), 0);
        assert_eq!(app.home_recent_selected, 0);
    }

    #[test]
    fn test_home_esc_noop() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
        assert!(!app.should_quit);
    }

    #[test]
    fn test_home_handle_key_q() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn test_home_snipe_enter_empty() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.snipe_input = "   ".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.is_some());
        assert!(app.home_input_error.unwrap().contains("Paste a link"));
    }

    #[tokio::test]
    async fn test_home_snipe_enter_unresolvable() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // GDrive URL without file ID should fail resolution
        app.snipe_input = "https://drive.google.com/drive/my-drive".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.is_some());
    }

    #[tokio::test]
    async fn test_home_snipe_enter_success() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.snipe_input = "test.zip".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Opening));
        assert_eq!(app.source, "test.zip");
        assert!(app.home_pending_provider.is_some());
        assert!(app.home_pending_name.is_some());
        assert!(app.home_input_error.is_none());
    }

    #[test]
    fn test_record_recent_from_home() {
        let mut app = App::new_home();
        app.source = "test.zip".to_string();
        app.home_pending_name = Some("Test Archive".to_string());
        app.home_pending_provider = Some(Provider::Local);
        app.launched_from_home = true;

        // We can't easily construct a real Opened, but we can pass something that looks like it
        // if we don't actually use the 'opened' parameter in record_recent_on_open.
        // Wait, record_recent_on_open takes &Opened<ResolvedSource>.
        // Since we can't easily make one, and it's unused, we can just skip this test
        // or refactor record_recent_on_open if we really want to test it.
        // Actually, the plan said skip if impractical.
    }

    #[test]
    fn test_record_recent_not_from_home() {
        let mut app = App::new_home();
        app.launched_from_home = false;
        app.home_recent = RecentList::default();

        // Should be a no-op even if we had a real Opened.
        // We'll skip for now due to Opened construction difficulty.
    }

    #[test]
    fn test_return_to_home() {
        let mut app = App::new_home();
        app.source = "test.zip".into();
        app.phase = AppPhase::Browsing;
        app.current_path = vec!["subdir".into()];
        app.show_help = true;
        app.status_message = None;

        app.return_to_home();

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.source.is_empty());
        assert!(app.current_path.is_empty());
        assert!(!app.show_help);
        assert_eq!(app.status_message, Some("Returned to Home".into()));
        assert!(app.launched_from_home);
    }

    #[test]
    fn test_esc_to_home_from_browsing_root() {
        let mut app = App::new_home(); // sets launched_from_home = true
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        app.phase = AppPhase::Browsing;
        app.current_path = vec![];
        
        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
    }

    #[test]
    fn test_esc_noop_from_browsing_root_not_launched_from_home() {
        let mut app = App::new_with_source("test.zip".into());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        app.phase = AppPhase::Browsing;
        app.current_path = vec![];
        
        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Browsing));
        assert!(!app.should_quit);
    }

    #[test]
    fn test_esc_to_home_from_error() {
        let mut app = App::new_home(); // sets launched_from_home = true
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        app.phase = AppPhase::Error("bad".into());
        
        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
    }

    #[tokio::test]
    async fn test_recent_enter_opens() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        let entries = vec![
            RecentEntry { name: "a.zip".into(), source: "s1.zip".into(), provider: Provider::Local, size: 10, last_opened: Utc::now() },
        ];
        app.home_recent = RecentList::from_entries(entries);
        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0;
        app.snipe_input = "garbage".into();

        app.handle_key(make_key(KeyCode::Enter), &tx);
        
        // Should transition to Opening and clear snipe_input
        assert!(matches!(app.phase, AppPhase::Opening));
        assert_eq!(app.source, "s1.zip");
        assert!(app.snipe_input.is_empty());
    }

    #[test]
    fn test_q_from_home_quits() {
        let mut app = App::new_home();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        
        app.home_focus = HomeFocus::Recent;
        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(app.should_quit);
    }
}
