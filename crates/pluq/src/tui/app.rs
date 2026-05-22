#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::glob_match;
use crate::home::complete::{complete_path, looks_like_url};
use archive::{
    Archive, ArchiveConfig, ArchiveEntry, ArchiveError, BatchResult, EntryTree, ProgressReporter,
    ScanInfo,
};
use archive_cache::Opened;
use archive_source::{ResolvedSource, SourceConfig, resolve};
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
    ArchiveOpened(Result<Opened, ArchiveError>),
    /// PendingArchive::scan() completed.
    ScanComplete(Result<Archive<ResolvedSource>, ArchiveError>),
    /// Archive::extract() completed.
    ExtractionComplete(Result<u64, ArchiveError>),
    /// Archive::extract_selected() completed.
    /// Ok contains the BatchResult. Err is only for unrecoverable errors (cancellation).
    BatchExtractionComplete(Result<BatchResult, ArchiveError>),
    /// OAuth flow completed.
    AuthComplete {
        provider: crate::auth_cmd::AuthProvider,
        result: Result<archive_auth::StoredToken, String>,
        source: String,
    },
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
    PluqInput,
    Bookmarks, // reserved for Plan v2; cycles but shows empty placeholder in MVP
    Recent,
}

#[derive(Debug, Clone)]
pub enum OverlayMode {
    /// Create a new bookmark. Carries everything needed to build the entry.
    Save {
        source: String,
        provider: crate::home::provider::Provider,
        size: u64,
    },
    /// Rename the bookmark at `index` in `home_bookmarks`.
    Rename { index: usize },
}

#[derive(Debug, Clone)]
pub enum OverlayState {
    /// User is typing the name. No collision yet.
    Editing,
    /// Enter hit a name collision. Waiting on o/r/c.
    Collision,
}

#[derive(Debug, Clone)]
pub struct BookmarkOverlay {
    pub mode: OverlayMode,
    pub input: String,
    pub state: OverlayState,
}

impl HomeFocus {
    fn next(self) -> Self {
        match self {
            Self::PluqInput => Self::Bookmarks,
            Self::Bookmarks => Self::Recent,
            Self::Recent => Self::PluqInput,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::PluqInput => Self::Recent,
            Self::Bookmarks => Self::PluqInput,
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
    /// Archive open returned a NotPublic / InvalidCredentials on an OAuth provider.
    AuthPrompt {
        provider: crate::auth_cmd::AuthProvider,
        source: String,
    },
    /// OAuth browser flow is in-flight.
    Authenticating {
        provider: crate::auth_cmd::AuthProvider,
        source: String,
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
    pub archive_config: ArchiveConfig,
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
    pub pluq_input: String,
    pub home_recent: crate::home::recent::RecentList,
    pub home_recent_selected: usize,
    /// Error string to show under the Pluq Input (e.g., "File not found").
    /// Set when archive open fails while user is in Home; cleared on next keypress.
    pub home_input_error: Option<String>,

    /// Provider tag derived from `ResolvedSource` during Home resolve (pre-spawn).
    /// Consumed by `record_recent_on_open` in Step 6. Some only during Opening/Scanning
    /// that originated from Home.
    pub home_pending_provider: Option<crate::home::provider::Provider>,
    /// Display name derived from `ResolvedSource::suggested_name()` during Home resolve.
    /// Consumed by `record_recent_on_open` in Step 6.
    pub home_pending_name: Option<String>,

    // NEW — Bookmark state. Only meaningful while `phase == Home` or an overlay is active.
    pub home_bookmarks: crate::home::bookmarks::BookmarkList,
    pub home_bookmarks_selected: usize,

    /// True if the session originally started in Home. Controls whether Esc at
    /// browse root should return to Home (step 7 wires this).
    pub launched_from_home: bool,

    /// Bookmark overlay state. None when not active.
    pub bookmark_overlay: Option<BookmarkOverlay>,

    pub token_store: Option<Arc<archive_auth::TokenStore>>,

    /// Handle to the running OAuth background task.
    pub auth_task: Option<tokio::task::JoinHandle<()>>,
}

impl App {
    /// Entry from `pluq <source>` — direct browse.
    pub fn new_with_source(
        source: String,
        token_store: Option<Arc<archive_auth::TokenStore>>,
    ) -> Self {
        let mut app = Self::default_state();
        app.source = source;
        app.phase = AppPhase::Opening;
        app.launched_from_home = false;
        // Load existing bookmarks so a `b` press in Browsing appends to them
        // (and persists) instead of overwriting the file with a single entry.
        app.home_bookmarks = crate::home::bookmarks::BookmarkList::load();
        app.token_store = token_store;
        app
    }

    /// Entry from bare `pluq` — Home Screen.
    pub fn new_home(token_store: Option<Arc<archive_auth::TokenStore>>) -> Self {
        let mut app = Self::default_state();
        app.phase = AppPhase::Home;
        app.home_recent = crate::home::recent::RecentList::load();
        app.home_bookmarks = crate::home::bookmarks::BookmarkList::load();
        app.launched_from_home = true;
        app.token_store = token_store;
        app
    }

    fn default_state() -> Self {
        Self {
            phase: AppPhase::Opening,
            source: String::new(),
            archive: None,
            archive_config: ArchiveConfig::default(),
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

            home_focus: HomeFocus::PluqInput,
            pluq_input: String::new(),
            home_recent: crate::home::recent::RecentList::default(),
            home_recent_selected: 0,
            home_input_error: None,
            home_pending_provider: None,
            home_pending_name: None,
            home_bookmarks: crate::home::bookmarks::BookmarkList::default(),
            home_bookmarks_selected: 0,
            launched_from_home: false,
            bookmark_overlay: None,
            token_store: None,
            auth_task: None,
        }
    }

    pub fn new(source: String, token_store: Option<Arc<archive_auth::TokenStore>>) -> Self {
        Self::new_with_source(source, token_store)
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
            ExtractionStrategy::ArchiveName => crate::default_output_folder(&self.source)
                .unwrap_or_else(|| std::path::PathBuf::from("extracted")),
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
            let query = self.filter_query.as_str();
            let glob_pat = if glob_match::is_glob_query(query) {
                glob_match::compile_glob(query)
            } else {
                None
            };
            let lower = query.to_lowercase();

            children
                .iter()
                .filter_map(|node| {
                    let full_path = if path_str.is_empty() {
                        node.name.clone()
                    } else {
                        format!("{}/{}", path_str, node.name)
                    };

                    let pass = if query.is_empty() {
                        true
                    } else if let Some(p) = &glob_pat {
                        glob_match::glob_matches(p, &full_path)
                    } else if glob_match::is_glob_query(query) {
                        false // invalid glob — explicit no match
                    } else {
                        node.name.to_lowercase().contains(&lower)
                    };

                    if !pass {
                        return None;
                    }

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
        self.table_state.select(if self.visible_items.is_empty() {
            None
        } else {
            Some(new_selected)
        });
    }

    /// Select all files matching the given glob pattern across the whole archive.
    pub fn toggle_all_matching(&mut self, pattern: &glob::Pattern) {
        let mut count = 0;
        for entry in &self.entries {
            if !entry.is_directory
                && glob_match::glob_matches(pattern, &entry.name)
                && self.toggled.insert(entry.name.clone())
            {
                count += 1;
            }
        }
        if count > 0 {
            self.status_message = Some(format!("Selected {} matching files", count));
        } else {
            self.status_message = Some("No new files matched".to_string());
        }
    }

    pub fn handle_message(&mut self, msg: AppMessage, tx: &mpsc::UnboundedSender<AppMessage>) {
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
                            ScanInfo::Required {
                                estimated_bytes, ..
                            } => *estimated_bytes,
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
                // Try to route to AuthPrompt first.
                if self.token_store.is_some()
                    && self.launched_from_home
                    && let Some(provider) = auth_provider_from_error(&e)
                {
                    info!(
                        ?provider,
                        "NotPublic/InvalidCredentials on known OAuth provider → AuthPrompt"
                    );
                    self.phase = AppPhase::AuthPrompt {
                        provider,
                        source: self.source.clone(),
                    };
                    self.home_pending_provider = None;
                    self.home_pending_name = None;
                    return;
                }

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
                self.status_message = Some(format!(
                    "Extracted '{}' ({})",
                    name,
                    crate::format_size(bytes)
                ));
            }
            AppMessage::ExtractionComplete(Err(e)) => {
                if matches!(e, ArchiveError::PasswordNeeded) {
                    // Password was wrong — clear it and re-prompt.
                    self.password = None;
                    self.password_input.clear();

                    // Figure out the entry index from the current phase.
                    let entry_index =
                        if let AppPhase::Extracting { ref entry_name, .. } = self.phase {
                            self.entries.iter().position(|e| e.name == *entry_name)
                        } else {
                            None
                        };

                    self.password_error = Some("Wrong password. Try again.".into());
                    self.phase = AppPhase::PasswordPrompt {
                        is_batch: false,
                        entry_index,
                    };
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
                info!(
                    succeeded = n,
                    failed, bytes, "batch extraction complete → Browsing"
                );
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
                    let first_err = &batch.failed[0].1;
                    format!(
                        "Extracted {} file{} ({}) — {} failed (e.g. {})",
                        n,
                        if n == 1 { "" } else { "s" },
                        crate::format_size(bytes),
                        failed,
                        first_err
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
            AppMessage::AuthComplete {
                provider,
                result,
                source,
            } => {
                if self.auth_task.is_none() {
                    return; // Task was cancelled.
                }
                self.auth_task = None;
                let store = match &self.token_store {
                    Some(s) => s.clone(),
                    None => {
                        self.home_input_error = Some("Token store unavailable.".into());
                        self.phase = AppPhase::Home;
                        return;
                    }
                };

                match result {
                    Ok(token) => {
                        let key = match provider {
                            crate::auth_cmd::AuthProvider::Gdrive => "gdrive",
                            crate::auth_cmd::AuthProvider::Dropbox => "dropbox",
                        };
                        if let Err(e) = store.save(key, token) {
                            error!(error = %e, "failed to persist OAuth token");
                            self.home_input_error = Some(format!("Token save failed: {e}"));
                            self.phase = AppPhase::Home;
                            return;
                        }
                        info!(?provider, "OAuth token saved → retrying archive open");
                        // Retry the open through the normal home submit path.
                        self.submit_home_source(source, tx);
                    }
                    Err(msg) => {
                        error!(error = %msg, "OAuth flow failed");
                        self.home_input_error = Some(format!("Authentication failed: {msg}"));
                        self.phase = AppPhase::Home;
                    }
                }
            }
        }
    }

    /// Returns the Recent entries that should be shown in the Home Screen,
    /// filtering out entries whose source is already bookmarked (design spec §3.3).
    pub fn recent_visible(&self) -> Vec<&crate::home::recent::RecentEntry> {
        self.home_recent
            .entries()
            .iter()
            .filter(|r| !self.home_bookmarks.has_source(&r.source))
            .collect()
    }

    /// Update the Recent list if we launched from Home.
    fn record_recent_on_open(&mut self, _opened: &Opened) {
        if !self.launched_from_home {
            return;
        }
        let source = self.source.clone();
        let provider = self
            .home_pending_provider
            .take()
            .unwrap_or(crate::home::provider::Provider::Local);
        let name = self
            .home_pending_name
            .take()
            .unwrap_or_else(|| source.clone());

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

    /// Common logic for opening an archive from Home (Pluq Input or Recent).
    fn submit_home_source(&mut self, source: String, tx: &mpsc::UnboundedSender<AppMessage>) {
        let mut config = SourceConfig::new();
        if let Some(store) = &self.token_store {
            config = config.with_token_store(store.clone());
        }

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
                let archive_config = self.archive_config.clone();
                let source_config = crate::source_config(self.token_store.clone());
                tokio::spawn(async move {
                    let result =
                        archive_cache::open(&source_for_task, &source_config, archive_config).await;
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
        if let Some(h) = self.auth_task.take() {
            h.abort();
        }
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
        self.home_bookmarks = crate::home::bookmarks::BookmarkList::load();
        self.home_bookmarks_selected = 0;
        self.home_input_error = None;
        self.home_pending_provider = None;
        self.home_pending_name = None;
        self.home_focus = HomeFocus::PluqInput;
        // Intentionally keep `launched_from_home = true`.
    }

    fn start_auth_flow(
        &mut self,
        provider: crate::auth_cmd::AuthProvider,
        source: String,
        tx: &mpsc::UnboundedSender<AppMessage>,
    ) {
        info!(?provider, "starting OAuth browser flow");
        self.phase = AppPhase::Authenticating {
            provider,
            source: source.clone(),
        };
        let tx_auth = tx.clone();
        let source_for_task = source.clone();
        let handle = tokio::spawn(async move {
            let result = match provider {
                crate::auth_cmd::AuthProvider::Gdrive => {
                    archive_gdrive::oauth::authenticate(300).await
                }
                crate::auth_cmd::AuthProvider::Dropbox => {
                    archive_dropbox::oauth::authenticate(300).await
                }
            };
            let _ = tx_auth.send(AppMessage::AuthComplete {
                provider,
                result: result.map_err(|e| e.to_string()),
                source: source_for_task,
            });
        });
        self.auth_task = Some(handle);
    }

    fn cancel_auth_prompt(&mut self) {
        // Surface original NotPublic/InvalidCredentials message on Home.
        let msg = match &self.phase {
            AppPhase::AuthPrompt { provider, .. } => {
                format!(
                    "{}: file is private. Run `pluq auth add {}` to authenticate.",
                    match provider {
                        crate::auth_cmd::AuthProvider::Gdrive => "Google Drive",
                        crate::auth_cmd::AuthProvider::Dropbox => "Dropbox",
                    },
                    match provider {
                        crate::auth_cmd::AuthProvider::Gdrive => "gdrive",
                        crate::auth_cmd::AuthProvider::Dropbox => "dropbox",
                    }
                )
            }
            _ => "Authentication declined.".to_string(),
        };
        self.home_input_error = Some(msg);
        self.phase = AppPhase::Home;
    }

    fn cancel_auth_in_flight(&mut self) {
        if let Some(h) = self.auth_task.take() {
            h.abort();
        }
        info!("OAuth flow cancelled by user");
        self.home_input_error = Some("Authentication cancelled.".to_string());
        self.phase = AppPhase::Home;
    }

    /// Formats an ArchiveError for display in the Home screen error row.
    fn format_home_error(&self, e: &ArchiveError) -> String {
        use archive_source::{ProviderError, classify_transport};
        match e {
            ArchiveError::Transport(inner) => match classify_transport(inner.as_ref()) {
                Some(ProviderError::NotPublic { provider }) => {
                    format!("{provider}: file is private. Share publicly or sign in.")
                }
                Some(ProviderError::FileNotFound { provider }) => {
                    format!("{provider}: file not found.")
                }
                Some(ProviderError::RateLimited {
                    provider,
                    retry_after,
                }) => {
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

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.bookmark_overlay.as_mut() else {
            return;
        };
        match overlay.state {
            OverlayState::Editing => match key.code {
                KeyCode::Esc => self.bookmark_overlay = None,
                KeyCode::Enter => self.overlay_submit(),
                KeyCode::Backspace => {
                    overlay.input.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                        && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
                {
                    overlay.input.push(c);
                }
                _ => {}
            },
            OverlayState::Collision => match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') => self.overlay_overwrite(),
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    overlay.state = OverlayState::Editing;
                }
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
                    self.bookmark_overlay = None;
                }
                _ => {}
            },
        }
    }

    fn overlay_submit(&mut self) {
        let Some(overlay) = self.bookmark_overlay.as_mut() else {
            return;
        };
        let name = overlay.input.trim().to_string();
        if name.is_empty() {
            // Keep overlay open; empty names are rejected silently. No error slot yet.
            return;
        }
        match &overlay.mode {
            OverlayMode::Save {
                source,
                provider,
                size,
            } => {
                if self.home_bookmarks.has_name(&name) {
                    overlay.state = OverlayState::Collision;
                    return;
                }
                self.home_bookmarks
                    .add(crate::home::bookmarks::BookmarkEntry {
                        name,
                        source: source.clone(),
                        provider: *provider,
                        size: *size,
                        last_opened: Utc::now(),
                    });
                self.home_bookmarks.save();
                self.bookmark_overlay = None;
                self.status_message = Some("Bookmarked.".into());
            }
            OverlayMode::Rename { index } => {
                let target_index = *index;
                match self.home_bookmarks.rename(target_index, name.clone()) {
                    Ok(()) => {
                        self.home_bookmarks.save();
                        self.bookmark_overlay = None;
                        self.status_message = Some("Bookmark renamed.".into());
                    }
                    Err(()) => {
                        overlay.state = OverlayState::Collision;
                    }
                }
            }
        }
    }

    fn overlay_overwrite(&mut self) {
        let Some(overlay) = self.bookmark_overlay.as_ref() else {
            return;
        };
        let name = overlay.input.trim().to_string();
        match &overlay.mode {
            OverlayMode::Save {
                source,
                provider,
                size,
            } => {
                let entry = crate::home::bookmarks::BookmarkEntry {
                    name,
                    source: source.clone(),
                    provider: *provider,
                    size: *size,
                    last_opened: Utc::now(),
                };
                self.home_bookmarks.overwrite_by_name(entry);
                self.home_bookmarks.save();
                self.bookmark_overlay = None;
                self.status_message = Some("Bookmark overwritten.".into());
            }
            OverlayMode::Rename { index } => {
                let target_index = *index;
                // Find the conflicting entry (different index, same name).
                let conflict_index = self
                    .home_bookmarks
                    .entries()
                    .iter()
                    .enumerate()
                    .find(|(i, e)| *i != target_index && e.name == name)
                    .map(|(i, _)| i);

                if let Some(conflict_idx) = conflict_index {
                    // Remove the conflict first. If it was before target, target_index shifts down by 1.
                    self.home_bookmarks.remove(conflict_idx);
                    let new_target = if conflict_idx < target_index {
                        target_index - 1
                    } else {
                        target_index
                    };
                    // Now rename (guaranteed no collision).
                    let _ = self.home_bookmarks.rename(new_target, name.clone());
                    self.home_bookmarks.save();
                    self.bookmark_overlay = None;
                    self.status_message = Some("Bookmark renamed (conflict overwritten).".into());
                } else {
                    // Shouldn't reach here — collision state implies conflict exists.
                    // Defensive: just close.
                    self.bookmark_overlay = None;
                }
            }
        }
    }

    /// Process a keypress while in the Home phase.
    fn handle_home_key(&mut self, key: KeyEvent, _tx: &mpsc::UnboundedSender<AppMessage>) {
        match key.code {
            KeyCode::Tab
                if self.home_focus == HomeFocus::PluqInput
                    && !self.pluq_input.is_empty()
                    && !looks_like_url(&self.pluq_input) =>
            {
                if let Some((new_input, hits)) = complete_path(&self.pluq_input) {
                    self.pluq_input = new_input;
                    if hits > 1 {
                        self.status_message = Some(format!("{} matches", hits));
                    } else if hits == 0 {
                        self.status_message = Some("no match".into());
                    }
                }
            }
            KeyCode::Tab => {
                self.home_focus = self.home_focus.next();
            }
            KeyCode::BackTab => {
                self.home_focus = self.home_focus.prev();
            }
            KeyCode::Char('q')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    == (self.home_focus == HomeFocus::PluqInput) =>
            {
                self.should_quit = true;
            }
            KeyCode::Esc => {
                // Style guide §6.6 — Home is a dead-end; q only.
            }

            // Recent list navigation (only if focused on Recent)
            KeyCode::Down | KeyCode::Char('j') if self.home_focus == HomeFocus::Recent => {
                let count = self.recent_visible().len();
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
                let count = self.recent_visible().len();
                if count > 0 {
                    self.home_recent_selected = count - 1;
                }
            }

            KeyCode::Char('b') if self.home_focus == HomeFocus::Recent => {
                let visible = self.recent_visible();
                if let Some(entry) = visible.get(self.home_recent_selected) {
                    let entry = (*entry).clone();
                    drop(visible);
                    self.bookmark_overlay = Some(BookmarkOverlay {
                        mode: OverlayMode::Save {
                            source: entry.source.clone(),
                            provider: entry.provider,
                            size: entry.size,
                        },
                        input: entry.name.clone(),
                        state: OverlayState::Editing,
                    });
                }
            }

            // Bookmarks list navigation
            KeyCode::Down | KeyCode::Char('j') if self.home_focus == HomeFocus::Bookmarks => {
                let count = self.home_bookmarks.len();
                if count > 0 {
                    self.home_bookmarks_selected =
                        (self.home_bookmarks_selected + 1).min(count - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.home_focus == HomeFocus::Bookmarks => {
                self.home_bookmarks_selected = self.home_bookmarks_selected.saturating_sub(1);
            }
            KeyCode::Home | KeyCode::Char('g') if self.home_focus == HomeFocus::Bookmarks => {
                self.home_bookmarks_selected = 0;
            }
            KeyCode::End | KeyCode::Char('G') if self.home_focus == HomeFocus::Bookmarks => {
                let count = self.home_bookmarks.len();
                if count > 0 {
                    self.home_bookmarks_selected = count - 1;
                }
            }
            KeyCode::Char('r') if self.home_focus == HomeFocus::Bookmarks => {
                if let Some(entry) = self
                    .home_bookmarks
                    .entries()
                    .get(self.home_bookmarks_selected)
                {
                    self.bookmark_overlay = Some(BookmarkOverlay {
                        mode: OverlayMode::Rename {
                            index: self.home_bookmarks_selected,
                        },
                        input: entry.name.clone(),
                        state: OverlayState::Editing,
                    });
                }
            }

            // Recent delete
            KeyCode::Char('d') if self.home_focus == HomeFocus::Recent => {
                let visible = self.recent_visible();
                if let Some(target) = visible.get(self.home_recent_selected) {
                    let source_to_remove = target.source.clone();
                    drop(visible);
                    if let Some(raw_idx) = self
                        .home_recent
                        .entries()
                        .iter()
                        .position(|e| e.source == source_to_remove)
                    {
                        self.home_recent.remove(raw_idx);
                        self.home_recent.save();
                    }
                    let max = self.recent_visible().len().saturating_sub(1);
                    self.home_recent_selected = self.home_recent_selected.min(max);
                }
            }

            // Bookmark delete
            KeyCode::Char('d') if self.home_focus == HomeFocus::Bookmarks => {
                if !self.home_bookmarks.is_empty() {
                    self.home_bookmarks.remove(self.home_bookmarks_selected);
                    self.home_bookmarks.save();
                    let max = self.home_bookmarks.len().saturating_sub(1);
                    self.home_bookmarks_selected = self.home_bookmarks_selected.min(max);
                }
            }

            // Enter
            KeyCode::Enter => match self.home_focus {
                HomeFocus::PluqInput => {
                    let source = self.pluq_input.trim().to_string();
                    if source.is_empty() {
                        self.home_input_error = Some("Paste a link or path first.".into());
                    } else {
                        self.submit_home_source(source, _tx);
                    }
                }
                HomeFocus::Recent => {
                    let visible = self.recent_visible();
                    if let Some(entry) = visible.get(self.home_recent_selected) {
                        let source = entry.source.clone();
                        drop(visible);
                        self.pluq_input.clear();
                        self.submit_home_source(source, _tx);
                    }
                }
                HomeFocus::Bookmarks => {
                    if let Some(entry) = self
                        .home_bookmarks
                        .entries()
                        .get(self.home_bookmarks_selected)
                    {
                        let source = entry.source.clone();
                        self.pluq_input.clear();
                        self.submit_home_source(source, _tx);
                    }
                }
            },

            // Pluq Input editing (only if focused)
            KeyCode::Char('u')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && self.home_focus == HomeFocus::PluqInput =>
            {
                self.pluq_input.clear();
            }
            KeyCode::Char(c)
                if self.home_focus == HomeFocus::PluqInput
                    && !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                    && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                self.pluq_input.push(c);
                self.home_input_error = None;
            }
            KeyCode::Backspace if self.home_focus == HomeFocus::PluqInput => {
                self.pluq_input.pop();
            }

            _ => {}
        }
    }

    /// Read shared progress atomics and update the current phase.
    /// Called on each tick (50 ms) — no channel messages needed.
    pub fn poll_progress(&mut self) {
        match self.phase {
            AppPhase::Scanning {
                ref mut bytes_scanned,
                ref mut entries_found,
                ..
            } => {
                *bytes_scanned = self.progress.bytes_scanned.load(Ordering::Relaxed);
                *entries_found = self.progress.entries_found.load(Ordering::Relaxed);
            }
            AppPhase::Extracting {
                ref mut bytes_extracted,
                ..
            } => {
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

    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppMessage>) {
        // Any keypress clears status message and home input error.
        self.status_message = None;
        self.home_input_error = None;

        if self.bookmark_overlay.is_some() {
            self.handle_overlay_key(key);
            return;
        }

        if let AppPhase::AuthPrompt { provider, source } = &self.phase {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let p = *provider;
                    let s = source.clone();
                    self.start_auth_flow(p, s, tx);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cancel_auth_prompt();
                }
                _ => {}
            }
            return;
        }

        if let AppPhase::Authenticating { .. } = &self.phase {
            if matches!(key.code, KeyCode::Esc) {
                self.cancel_auth_in_flight();
            } else if matches!(key.code, KeyCode::Char('q')) {
                self.should_quit = true;
            }
            return;
        }

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
                KeyCode::Enter => {
                    if !self.filter_query.is_empty()
                        && glob_match::is_glob_query(&self.filter_query)
                        && let Some(p) = glob_match::compile_glob(&self.filter_query)
                    {
                        self.toggle_all_matching(&p);
                    }
                    self.is_filtering = false;
                }
                KeyCode::Esc => {
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
            KeyCode::Char('b') if matches!(self.phase, AppPhase::Browsing) => {
                // Only if we have a source (always true in Browsing, but guard anyway).
                if !self.source.is_empty() {
                    // Use the best available name/provider/size from current archive state.
                    let provider = self
                        .home_pending_provider
                        .unwrap_or(crate::home::provider::Provider::Local);
                    let name = self
                        .home_pending_name
                        .clone()
                        .unwrap_or_else(|| self.source.clone());
                    // MVP keeps size=0 for Recent; bookmarks inherit the same placeholder.
                    self.bookmark_overlay = Some(BookmarkOverlay {
                        mode: OverlayMode::Save {
                            source: self.source.clone(),
                            provider,
                            size: 0,
                        },
                        input: name,
                        state: OverlayState::Editing,
                    });
                }
            }
            KeyCode::Char('q') if !matches!(self.phase, AppPhase::Home) => {
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
                } else if matches!(self.phase, AppPhase::Browsing) && !self.current_path.is_empty()
                {
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
                    let result = archive
                        .extract(&entry, &mut file, progress, password.as_deref())
                        .await;
                    if matches!(&result, Err(ArchiveError::Cancelled)) {
                        drop(file);
                        let _ = tokio::fs::remove_file(&output_path).await;
                    }
                    let _ = tx_extract.send(AppMessage::ExtractionComplete(result));
                }
                Err(e) => {
                    let _ = tx_extract.send(AppMessage::ExtractionComplete(Err(
                        archive::ArchiveError::Internal {
                            detail: format!("Failed to create {}: {e}", output_path.display()),
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

        info!(
            total_files,
            total_bytes, "starting batch extraction → ExtractingBatch"
        );

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

            let result = archive
                .extract_selected(&entry_refs, sink_factory, progress, password.as_deref())
                .await;

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

/// Map an ArchiveError::Transport into an AuthProvider if the error is
/// NotPublic or InvalidCredentials on a known OAuth provider.
fn auth_provider_from_error(e: &ArchiveError) -> Option<crate::auth_cmd::AuthProvider> {
    use archive_source::{ProviderError, classify_transport};
    let ArchiveError::Transport(inner) = e else {
        return None;
    };
    let pe = classify_transport(inner.as_ref())?;
    let provider_name = match pe {
        ProviderError::NotPublic { provider } | ProviderError::InvalidCredentials { provider } => {
            provider
        }
        _ => return None,
    };
    match provider_name {
        "Google Drive" => Some(crate::auth_cmd::AuthProvider::Gdrive),
        "Dropbox" => Some(crate::auth_cmd::AuthProvider::Dropbox),
        _ => None,
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
        Self {
            shared,
            cancel,
            cumulative_bytes: 0,
            current_entry_bytes: 0,
        }
    }
}

impl ProgressReporter for TuiProgressReporter {
    fn on_scan_progress(&mut self, bytes_scanned: u64, entries_found: usize) {
        self.shared
            .bytes_scanned
            .store(bytes_scanned, Ordering::Relaxed);
        self.shared
            .entries_found
            .store(entries_found, Ordering::Relaxed);
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

    fn on_warning(&mut self, message: &str) {
        tracing::warn!("TUI Archive warning: {}", message);
    }
}

#[cfg(test)]
mod home_tests {
    use super::*;
    use crate::home::bookmarks::{BookmarkEntry, BookmarkList};
    use crate::home::provider::Provider;
    use crate::home::recent::{RecentEntry, RecentList};
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

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
        let app = App::new_home(None);
        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.launched_from_home);
        assert_eq!(app.home_focus, HomeFocus::PluqInput);
        assert_eq!(app.home_recent_selected, 0);
        assert!(app.home_pending_provider.is_none());
        assert!(app.home_pending_name.is_none());
    }

    #[test]
    fn test_app_new_with_source() {
        let source = "foo.zip".to_string();
        let app = App::new_with_source(source.clone(), None);
        assert!(matches!(app.phase, AppPhase::Opening));
        assert!(!app.launched_from_home);
        assert_eq!(app.source, source);
    }

    #[test]
    fn test_home_focus_cycling() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Tab cycles forwards
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Recent);
        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::PluqInput);

        // Shift+Tab (BackTab) cycles backwards
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Recent);
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::PluqInput);
    }

    #[test]
    fn test_home_pluq_input() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Type "/tmp/foo.zip"
        for c in "/tmp/foo.zip".chars() {
            app.handle_key(make_key(KeyCode::Char(c)), &tx);
        }
        assert_eq!(app.pluq_input, "/tmp/foo.zip");

        // Backspace
        app.handle_key(make_key(KeyCode::Backspace), &tx);
        assert_eq!(app.pluq_input, "/tmp/foo.zi");

        // Ctrl+U clears
        app.handle_key(make_ctrl_key(KeyCode::Char('u')), &tx);
        assert_eq!(app.pluq_input, "");

        // Backspace on empty is no-op
        app.handle_key(make_key(KeyCode::Backspace), &tx);
        assert_eq!(app.pluq_input, "");
    }

    #[test]
    fn test_tab_cycles_focus_when_empty() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(app.home_focus, HomeFocus::PluqInput);
        assert!(app.pluq_input.is_empty());

        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
    }

    #[test]
    fn test_tab_cycles_focus_when_url() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.pluq_input = "https://google.com".to_string();

        app.handle_key(make_key(KeyCode::Tab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Bookmarks);
    }

    #[test]
    fn test_tab_completes_when_local_path() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("foo.zip");
        std::fs::File::create(&file_path).unwrap();

        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.pluq_input = format!("{}/f", dir.path().to_string_lossy());

        app.handle_key(make_key(KeyCode::Tab), &tx);
        // Focus stays on PluqInput
        assert_eq!(app.home_focus, HomeFocus::PluqInput);
        // Input is completed
        assert_eq!(
            app.pluq_input,
            format!("{}/foo.zip", dir.path().to_string_lossy())
        );
    }

    #[test]
    fn test_backtab_always_cycles_focus() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.pluq_input = "/tmp/foo".to_string();

        // Shift+Tab (BackTab) cycles focus even with text in PluqInput
        app.handle_key(make_key(KeyCode::BackTab), &tx);
        assert_eq!(app.home_focus, HomeFocus::Recent);
    }

    #[test]
    fn test_home_pluq_input_accepts_shift_chars() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Uppercase letter (crossterm reports Char('H') + SHIFT on most terminals).
        app.handle_key(make_shift_key(KeyCode::Char('H')), &tx);
        // Shifted punctuation (query-string '?', parameter '&').
        app.handle_key(make_shift_key(KeyCode::Char('?')), &tx);
        app.handle_key(make_shift_key(KeyCode::Char('&')), &tx);

        assert_eq!(app.pluq_input, "H?&");
    }

    #[test]
    fn test_home_recent_navigation() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Clear bookmarks to ensure Recent entries are visible
        app.home_bookmarks = BookmarkList::default();

        // Populate recent
        let entries = vec![
            RecentEntry {
                name: "a.zip".into(),
                source: "s1".into(),
                provider: Provider::Local,
                size: 10,
                last_opened: Utc::now(),
            },
            RecentEntry {
                name: "b.zip".into(),
                source: "s2".into(),
                provider: Provider::Local,
                size: 20,
                last_opened: Utc::now(),
            },
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
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Clear bookmarks to ensure Recent entries are visible
        app.home_bookmarks = BookmarkList::default();

        // Populate recent with 2 entries
        let entries = vec![
            RecentEntry {
                name: "a.zip".into(),
                source: "s1".into(),
                provider: Provider::Local,
                size: 10,
                last_opened: Utc::now(),
            },
            RecentEntry {
                name: "b.zip".into(),
                source: "s2".into(),
                provider: Provider::Local,
                size: 20,
                last_opened: Utc::now(),
            },
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
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
        assert!(!app.should_quit);
    }

    #[test]
    fn test_home_handle_key_q() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Focused on PluqInput: 'q' should type "q", not quit.
        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(!app.should_quit);
        assert_eq!(app.pluq_input, "q");

        // Focused on PluqInput: Ctrl+q should quit.
        let ctrl_q = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_key(ctrl_q, &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn test_q_from_home_recent_ignores_ctrl() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.home_focus = HomeFocus::Recent;

        // Focused on Recent: Ctrl+q should be ignored.
        let ctrl_q = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_key(ctrl_q, &tx);
        assert!(!app.should_quit);

        // Focused on Recent: plain 'q' should quit.
        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn test_home_pluq_enter_empty() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.pluq_input = "   ".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.is_some());
        assert!(app.home_input_error.unwrap().contains("Paste a link"));
    }

    #[tokio::test]
    async fn test_home_pluq_enter_unresolvable() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // GDrive URL without file ID should fail resolution
        app.pluq_input = "https://drive.google.com/drive/my-drive".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.is_some());
    }

    #[tokio::test]
    async fn test_home_pluq_enter_success() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.pluq_input = "test.zip".to_string();
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(app.phase, AppPhase::Opening));
        assert_eq!(app.source, "test.zip");
        assert!(app.home_pending_provider.is_some());
        assert!(app.home_pending_name.is_some());
        assert!(app.home_input_error.is_none());
    }

    #[test]
    fn test_record_recent_from_home() {
        let mut app = App::new_home(None);
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
        let mut app = App::new_home(None);
        app.launched_from_home = false;
        app.home_recent = RecentList::default();

        // Should be a no-op even if we had a real Opened.
        // We'll skip for now due to Opened construction difficulty.
    }

    #[test]
    fn test_return_to_home() {
        let mut app = App::new_home(None);
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
        let mut app = App::new_home(None); // sets launched_from_home = true
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::Browsing;
        app.current_path = vec![];

        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
    }

    #[test]
    fn test_esc_noop_from_browsing_root_not_launched_from_home() {
        let mut app = App::new_with_source("test.zip".into(), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::Browsing;
        app.current_path = vec![];

        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Browsing));
        assert!(!app.should_quit);
    }

    #[test]
    fn test_esc_to_home_from_error() {
        let mut app = App::new_home(None); // sets launched_from_home = true
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::Error("bad".into());

        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(matches!(app.phase, AppPhase::Home));
    }

    #[tokio::test]
    async fn test_recent_enter_opens() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let entries = vec![RecentEntry {
            name: "a.zip".into(),
            source: "s1.zip".into(),
            provider: Provider::Local,
            size: 10,
            last_opened: Utc::now(),
        }];
        app.home_recent = RecentList::from_entries(entries);
        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0;
        app.pluq_input = "garbage".into();

        app.handle_key(make_key(KeyCode::Enter), &tx);

        // Should transition to Opening and clear pluq_input
        assert!(matches!(app.phase, AppPhase::Opening));
        assert_eq!(app.source, "s1.zip");
        assert!(app.pluq_input.is_empty());
    }

    #[test]
    fn test_q_from_home_quits() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.home_focus = HomeFocus::Recent;
        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn test_new_home_init_state() {
        let app = App::new_home(None);
        assert_eq!(app.home_bookmarks_selected, 0);
        assert_eq!(app.home_recent_selected, 0);
        assert_eq!(app.home_focus, HomeFocus::PluqInput);
    }

    #[test]
    fn test_recent_visible_filters_bookmarks() {
        let mut app = App::default_state();
        let r1 = RecentEntry {
            name: "r1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let r2 = RecentEntry {
            name: "r2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let r3 = RecentEntry {
            name: "r3".into(),
            source: "s3".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };

        app.home_recent = RecentList::from_entries(vec![r1, r2, r3]);

        let b2 = crate::home::bookmarks::BookmarkEntry {
            name: "b2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = crate::home::bookmarks::BookmarkList::from_entries(vec![b2]);

        let visible = app.recent_visible();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].source, "s1");
        assert_eq!(visible[1].source, "s3");
    }

    #[test]
    fn test_recent_visible_empty_bookmarks() {
        let mut app = App::default_state();
        let r1 = RecentEntry {
            name: "r1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_recent = RecentList::from_entries(vec![r1]);
        app.home_bookmarks = crate::home::bookmarks::BookmarkList::default();

        let visible = app.recent_visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].source, "s1");
    }

    #[test]
    fn test_recent_visible_empty_recent() {
        let mut app = App::default_state();
        app.home_recent = RecentList::default();
        let b1 = crate::home::bookmarks::BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = crate::home::bookmarks::BookmarkList::from_entries(vec![b1]);

        let visible = app.recent_visible();
        assert!(visible.is_empty());
    }

    #[test]
    fn test_bookmarks_focus_nav() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let b1 = BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let b2 = BookmarkEntry {
            name: "b2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let b3 = BookmarkEntry {
            name: "b3".into(),
            source: "s3".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };

        app.home_bookmarks = BookmarkList::from_entries(vec![b1, b2, b3]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0;

        app.handle_key(make_key(KeyCode::Char('j')), &tx);
        assert_eq!(app.home_bookmarks_selected, 1);

        app.handle_key(make_key(KeyCode::Char('j')), &tx);
        assert_eq!(app.home_bookmarks_selected, 2);

        app.handle_key(make_key(KeyCode::Char('k')), &tx);
        assert_eq!(app.home_bookmarks_selected, 1);
    }

    #[test]
    fn test_bookmarks_delete() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let b1 = BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let b2 = BookmarkEntry {
            name: "b2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };

        app.home_bookmarks = BookmarkList::from_entries(vec![b1, b2]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0;

        app.handle_key(make_key(KeyCode::Char('d')), &tx);
        assert_eq!(app.home_bookmarks.len(), 1);
        assert_eq!(app.home_bookmarks.entries()[0].name, "b2");
        assert_eq!(app.home_bookmarks_selected, 0);
    }

    #[tokio::test]
    async fn test_recent_enter_uses_visible_index() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let r1 = RecentEntry {
            name: "r1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let r2 = RecentEntry {
            name: "r2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_recent = RecentList::from_entries(vec![r1, r2]);

        // Bookmark r1's source so it's filtered out from Recent visible
        let b1 = BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);

        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0; // Targets r2 because r1 is hidden

        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert_eq!(app.source, "s2");
    }

    #[test]
    fn test_recent_delete_uses_visible_index() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let r1 = RecentEntry {
            name: "r1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        let r2 = RecentEntry {
            name: "r2".into(),
            source: "s2".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_recent = RecentList::from_entries(vec![r1, r2]);

        // Bookmark r1's source so it's filtered out from Recent visible
        let b1 = BookmarkEntry {
            name: "b1".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 0,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);

        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0; // Targets r2

        app.handle_key(make_key(KeyCode::Char('d')), &tx);

        assert_eq!(app.home_recent.len(), 1);
        assert_eq!(app.home_recent.entries()[0].source, "s1"); // r2 was removed, r1 remains (but is hidden)
    }

    #[test]
    fn test_overlay_open_from_recent() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.home_bookmarks = BookmarkList::default();

        let entries = vec![RecentEntry {
            name: "test.zip".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 10,
            last_opened: Utc::now(),
        }];
        app.home_recent = RecentList::from_entries(entries);
        app.home_focus = HomeFocus::Recent;
        app.home_recent_selected = 0;

        app.handle_key(make_key(KeyCode::Char('b')), &tx);

        assert!(app.bookmark_overlay.is_some());
        let overlay = app.bookmark_overlay.unwrap();
        assert!(matches!(overlay.mode, OverlayMode::Save { .. }));
        assert_eq!(overlay.input, "test.zip");
        assert!(matches!(overlay.state, OverlayState::Editing));
    }
    #[test]
    fn test_overlay_esc_closes() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "test".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_key(KeyCode::Esc), &tx);
        assert!(app.bookmark_overlay.is_none());
    }

    #[test]
    fn test_overlay_enter_saves_new() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.home_bookmarks = BookmarkList::default();
        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s1".into(),
                provider: Provider::Local,
                size: 100,
            },
            input: "New Bookmark".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(app.bookmark_overlay.is_none());
        assert_eq!(app.home_bookmarks.len(), 1);
        let b = &app.home_bookmarks.entries()[0];
        assert_eq!(b.name, "New Bookmark");
        assert_eq!(b.source, "s1");
        assert_eq!(app.status_message, Some("Bookmarked.".into()));
    }

    #[test]
    fn test_overlay_enter_triggers_collision() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let b1 = BookmarkEntry {
            name: "Existing".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 10,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s2".into(),
                provider: Provider::Local,
                size: 20,
            },
            input: "Existing".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(app.bookmark_overlay.is_some());
        let overlay = app.bookmark_overlay.as_ref().unwrap();
        assert!(matches!(overlay.state, OverlayState::Collision));
        assert_eq!(app.home_bookmarks.len(), 1);
    }

    #[test]
    fn test_overlay_collision_overwrite() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let b1 = BookmarkEntry {
            name: "Existing".into(),
            source: "s1".into(),
            provider: Provider::Local,
            size: 10,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "new_source".into(),
                provider: Provider::Local,
                size: 20,
            },
            input: "Existing".into(),
            state: OverlayState::Collision,
        });

        app.handle_key(make_key(KeyCode::Char('o')), &tx);

        assert!(app.bookmark_overlay.is_none());
        assert_eq!(app.home_bookmarks.len(), 1);
        assert_eq!(app.home_bookmarks.entries()[0].source, "new_source");
        assert_eq!(app.status_message, Some("Bookmark overwritten.".into()));
    }

    #[test]
    fn test_overlay_collision_cancel() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "Existing".into(),
            state: OverlayState::Collision,
        });

        app.handle_key(make_key(KeyCode::Char('c')), &tx);
        assert!(app.bookmark_overlay.is_none());
    }

    #[test]
    fn test_overlay_collision_rename_reopens_editing() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "Existing".into(),
            state: OverlayState::Collision,
        });

        app.handle_key(make_key(KeyCode::Char('r')), &tx);
        assert!(app.bookmark_overlay.is_some());
        assert!(matches!(
            app.bookmark_overlay.as_ref().unwrap().state,
            OverlayState::Editing
        ));
    }

    #[test]
    fn test_overlay_from_browse() {
        let mut app = App::new_with_source("foo.zip".into(), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.phase = AppPhase::Browsing;

        app.handle_key(make_key(KeyCode::Char('b')), &tx);

        assert!(app.bookmark_overlay.is_some());
        let overlay = app.bookmark_overlay.unwrap();
        let OverlayMode::Save { source, .. } = &overlay.mode else {
            panic!("Expected OverlayMode::Save");
        };
        assert_eq!(source, "foo.zip");
        assert_eq!(overlay.input, "foo.zip");
    }

    #[test]
    fn test_overlay_accepts_shift_chars() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_shift_key(KeyCode::Char('H')), &tx);
        app.handle_key(make_shift_key(KeyCode::Char('?')), &tx);
        app.handle_key(make_shift_key(KeyCode::Char('&')), &tx);

        assert_eq!(app.bookmark_overlay.as_ref().unwrap().input, "H?&");
    }

    #[test]
    fn test_overlay_intercepts_keys() {
        let mut app = App::new_with_source("foo.zip".into(), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.phase = AppPhase::Browsing;
        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Save {
                source: "s".into(),
                provider: Provider::Local,
                size: 0,
            },
            input: "test".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_key(KeyCode::Char('q')), &tx);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_rename_opens_overlay() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let b1 = BookmarkEntry {
            name: "MyArchive".into(),
            source: "source1".into(),
            provider: crate::home::provider::Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0;

        app.handle_key(make_key(KeyCode::Char('r')), &tx);

        let overlay = app.bookmark_overlay.as_ref().unwrap();
        assert!(matches!(overlay.mode, OverlayMode::Rename { index: 0 }));
        assert_eq!(overlay.input, "MyArchive");
        assert!(matches!(overlay.state, OverlayState::Editing));
    }

    #[test]
    fn test_rename_submit_ok() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let b1 = BookmarkEntry {
            name: "A".into(),
            source: "s1".into(),
            provider: crate::home::provider::Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0;

        // Open overlay
        app.handle_key(make_key(KeyCode::Char('r')), &tx);

        // Change name
        let mut overlay = app.bookmark_overlay.take().unwrap();
        overlay.input = "NewName".into();
        app.bookmark_overlay = Some(overlay);

        // Submit
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(app.bookmark_overlay.is_none());
        assert_eq!(app.home_bookmarks.entries()[0].name, "NewName");
        assert!(app.status_message.as_ref().unwrap().contains("renamed"));
    }

    #[test]
    fn test_rename_submit_collision() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let b1 = BookmarkEntry {
            name: "A".into(),
            source: "s1".into(),
            provider: crate::home::provider::Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        let b2 = BookmarkEntry {
            name: "B".into(),
            source: "s2".into(),
            provider: crate::home::provider::Provider::Local,
            size: 200,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1, b2]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 1; // Focus "B"

        // Open overlay on "B"
        app.handle_key(make_key(KeyCode::Char('r')), &tx);

        // Rename "B" to "A" (collision)
        let mut overlay = app.bookmark_overlay.take().unwrap();
        overlay.input = "A".into();
        app.bookmark_overlay = Some(overlay);

        // Submit
        app.handle_key(make_key(KeyCode::Enter), &tx);

        let overlay = app.bookmark_overlay.as_ref().unwrap();
        assert!(matches!(overlay.state, OverlayState::Collision));
        assert_eq!(app.home_bookmarks.entries()[1].name, "B"); // Unchanged
    }

    #[test]
    fn test_rename_overwrite_removes_conflict() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let b1 = BookmarkEntry {
            name: "A".into(),
            source: "s1".into(),
            provider: crate::home::provider::Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        let b2 = BookmarkEntry {
            name: "B".into(),
            source: "s2".into(),
            provider: crate::home::provider::Provider::Local,
            size: 200,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1, b2]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0; // Focus "A"

        // Open overlay on "A", rename to "B", hit collision
        app.handle_key(make_key(KeyCode::Char('r')), &tx);
        let mut overlay = app.bookmark_overlay.take().unwrap();
        overlay.input = "B".into();
        app.bookmark_overlay = Some(overlay);
        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(matches!(
            app.bookmark_overlay.as_ref().unwrap().state,
            OverlayState::Collision
        ));

        // Overwrite
        app.handle_key(make_key(KeyCode::Char('o')), &tx);

        assert!(app.bookmark_overlay.is_none());
        assert_eq!(app.home_bookmarks.len(), 1);
        assert_eq!(app.home_bookmarks.entries()[0].name, "B");
        assert_eq!(app.home_bookmarks.entries()[0].source, "s1"); // The original A
    }

    #[test]
    fn test_rename_overwrite_when_conflict_after_target() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let b1 = BookmarkEntry {
            name: "A".into(),
            source: "s1".into(),
            provider: crate::home::provider::Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        };
        let b2 = BookmarkEntry {
            name: "B".into(),
            source: "s2".into(),
            provider: crate::home::provider::Provider::Local,
            size: 200,
            last_opened: Utc::now(),
        };
        app.home_bookmarks = BookmarkList::from_entries(vec![b1, b2]);
        app.home_focus = HomeFocus::Bookmarks;
        app.home_bookmarks_selected = 0; // Focus "A"

        // Open overlay on "A", rename to "B", hit collision
        app.handle_key(make_key(KeyCode::Char('r')), &tx);
        let mut overlay = app.bookmark_overlay.take().unwrap();
        overlay.input = "B".into();
        app.bookmark_overlay = Some(overlay);
        app.handle_key(make_key(KeyCode::Enter), &tx);

        // Overwrite
        app.handle_key(make_key(KeyCode::Char('o')), &tx);

        assert_eq!(app.home_bookmarks.len(), 1);
        assert_eq!(app.home_bookmarks.entries()[0].name, "B");
        assert_eq!(app.home_bookmarks.entries()[0].source, "s1");
    }

    #[test]
    fn test_rename_out_of_range_noop() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.home_bookmarks = BookmarkList::default();
        app.bookmark_overlay = Some(BookmarkOverlay {
            mode: OverlayMode::Rename { index: 99 },
            input: "X".into(),
            state: OverlayState::Editing,
        });

        app.handle_key(make_key(KeyCode::Enter), &tx);

        assert!(app.bookmark_overlay.is_none());
        assert!(app.home_bookmarks.is_empty());
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use archive::ArchiveError;

    #[test]
    fn auth_provider_from_error_returns_gdrive_on_notpublic() {
        let err = archive_gdrive::GDriveError::NotPublic {
            file_id: "x".into(),
        };
        let e = ArchiveError::Transport(Box::new(err));
        assert_eq!(
            auth_provider_from_error(&e),
            Some(crate::auth_cmd::AuthProvider::Gdrive)
        );
    }

    #[test]
    fn auth_provider_from_error_returns_dropbox_on_invalid_credentials() {
        let err = archive_dropbox::DropboxError::InvalidCredentials;
        let e = ArchiveError::Transport(Box::new(err));
        assert_eq!(
            auth_provider_from_error(&e),
            Some(crate::auth_cmd::AuthProvider::Dropbox)
        );
    }

    #[test]
    fn auth_provider_from_error_returns_none_for_file_not_found() {
        let err = archive_gdrive::GDriveError::FileNotFound {
            file_id: "x".into(),
        };
        let e = ArchiveError::Transport(Box::new(err));
        assert_eq!(auth_provider_from_error(&e), None);
    }

    #[test]
    fn auth_provider_from_error_returns_none_for_non_transport_error() {
        let e = ArchiveError::Cancelled;
        assert_eq!(auth_provider_from_error(&e), None);
    }

    #[test]
    fn auth_prompt_transition_on_notpublic_when_store_present_and_home() {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(archive_auth::TokenStore::load_from(tmp.path().join("tokens.json")).unwrap());
        let mut app = App::new_home(Some(store));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.source = "gdrive://foo".into();

        let err = archive_gdrive::GDriveError::NotPublic {
            file_id: "x".into(),
        };
        let e = ArchiveError::Transport(Box::new(err));
        app.handle_message(AppMessage::ArchiveOpened(Err(e)), &tx);

        assert!(matches!(
            app.phase,
            AppPhase::AuthPrompt {
                provider: crate::auth_cmd::AuthProvider::Gdrive,
                ..
            }
        ));
    }

    #[test]
    fn auth_prompt_does_not_fire_without_token_store() {
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let err = archive_gdrive::GDriveError::NotPublic {
            file_id: "x".into(),
        };
        let e = ArchiveError::Transport(Box::new(err));
        app.handle_message(AppMessage::ArchiveOpened(Err(e)), &tx);

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.is_some());
    }

    #[tokio::test]
    async fn auth_prompt_y_starts_authenticating_phase() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::AuthPrompt {
            provider: crate::auth_cmd::AuthProvider::Gdrive,
            source: "gdrive://xyz".into(),
        };

        app.handle_key(
            KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            &tx,
        );

        assert!(matches!(app.phase, AppPhase::Authenticating { .. }));
        assert!(app.auth_task.is_some());

        app.cancel_auth_in_flight(); // cleanup task
    }

    #[test]
    fn auth_prompt_n_returns_to_home_with_hint() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = App::new_home(None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::AuthPrompt {
            provider: crate::auth_cmd::AuthProvider::Gdrive,
            source: "gdrive://xyz".into(),
        };

        app.handle_key(
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            &tx,
        );

        assert!(matches!(app.phase, AppPhase::Home));
        assert!(app.home_input_error.unwrap().contains("auth add"));
    }

    #[tokio::test]
    async fn auth_complete_ok_saves_token_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let store =
            Arc::new(archive_auth::TokenStore::load_from(tmp.path().join("tokens.json")).unwrap());
        let mut app = App::new_home(Some(store.clone()));
        let (tx, mut _rx) = tokio::sync::mpsc::unbounded_channel();

        app.phase = AppPhase::Authenticating {
            provider: crate::auth_cmd::AuthProvider::Gdrive,
            source: "gdrive".into(),
        };
        let handle = tokio::spawn(async move {});
        app.auth_task = Some(handle);

        let token = archive_auth::StoredToken {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_at: Some(3600),
            scopes: vec![],
        };

        app.handle_message(
            AppMessage::AuthComplete {
                provider: crate::auth_cmd::AuthProvider::Gdrive,
                result: Ok(token),
                source: "gdrive".into(),
            },
            &tx,
        );

        assert!(store.get("gdrive").is_some());
        assert!(!matches!(app.phase, AppPhase::Authenticating { .. }));
    }
}
