mod auth_cmd;
mod bug_report;
mod build_info;
mod cli_progress;
mod glob_match;
pub mod home;
mod redact;
mod tui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use tracing::{info, warn};

use tokio::io::AsyncWrite;

use archive::ArchiveEntry;
use archive_source::{SourceConfig, classify_transport, resolve};

/// pluq — surgical archive extraction from local or cloud storage.
#[derive(Parser)]
#[command(name = "pluq", version, about)]
struct Cli {
    /// Open an archive directly in the TUI. Omit to launch the Home Screen.
    source: Option<String>,

    /// Escalate log level to debug (default: info; PLUQ_LOG also accepted).
    #[arg(long, global = true)]
    debug: bool,

    /// Delete the persistent cache directory before starting the operation.
    #[arg(long, global = true)]
    clean_cache: bool,

    /// Delete the debug log directory before starting the operation.
    #[arg(long, global = true)]
    clean_logs: bool,

    /// ZIP filename encoding: auto detects; explicit values override archive flags (not file contents).
    #[arg(long, global = true, default_value = "auto", value_name = "ENCODING")]
    encoding: archive::FilenameEncoding,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List files inside an archive.
    List {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
        /// Show all entries including OS-generated junk files
        /// (.DS_Store, __MACOSX/, Thumbs.db, etc.).
        #[arg(long)]
        all: bool,
    },
    /// Extract files from an archive. With no glob, extracts everything.
    ///
    /// Glob matching is gitignore-style: a pattern containing `/` matches the
    /// full entry path (e.g. `photos/**/*.jpg`); otherwise it matches the
    /// entry's basename only (e.g. `*.jpg`).
    Get {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
        /// Optional glob pattern. Omit to extract all entries.
        glob: Option<String>,
        /// Output directory (default: archive name without extension).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Include OS-generated junk files (.DS_Store, __MACOSX/, Thumbs.db, etc.).
        #[arg(long)]
        all: bool,
    },
    /// Test archive integrity — decompress and CRC-verify every entry.
    ///
    /// Exits with a non-zero status code if any entry fails.
    Test {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
        /// Show all entries including OS-generated junk files.
        #[arg(long)]
        all: bool,
    },
    /// Manage OAuth tokens for cloud providers.
    Auth {
        #[command(subcommand)]
        action: auth_cmd::AuthAction,
    },
    /// Write a redacted local diagnostic file for bug reports (never uploaded).
    BugReport,
    /// Open the browser to report a bug, suggest an idea, or say hi.
    Feedback {
        /// Report a bug (opens a prefilled new-issue form).
        #[arg(long, group = "target")]
        bug: bool,
        /// Suggest an idea (opens the Ideas discussion category).
        #[arg(long, group = "target")]
        idea: bool,
        /// Say hi / ask a question (opens Discussions home).
        #[arg(long, group = "target")]
        say_hi: bool,
    },
    /// Print a shell completion script to stdout.
    ///
    /// Install by redirecting to your shell's completion directory, e.g.
    /// `pluq completions bash > ~/.local/share/bash-completion/completions/pluq`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

#[tokio::main]
async fn main() {
    // ── Panic handling ───────────────────────────────────────────────────
    let log_msg = if let Some(path) = log_path() {
        format!("- Logs: {}", path.display())
    } else {
        String::new()
    };

    human_panic::setup_panic!(
        human_panic::Metadata::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .authors("Stefan Dekanski")
            .homepage(REPO_URL)
            .support(log_msg.clone())
    );

    let cli = Cli::parse();

    // ── Log cleaning ─────────────────────────────────────────────────────
    // Runs before init_logging: the daily appender opens a handle inside the
    // logs dir, so removing the dir afterwards would orphan this run's log.
    // init_logging recreates the dir. No tracing macros here — no subscriber
    // is installed yet, so info!/warn! would be silent no-ops.
    if cli.clean_logs {
        match log_path() {
            Some(dir) if dir.exists() => match std::fs::remove_dir_all(&dir) {
                Ok(_) => eprintln!("  Logs cleared: {}", dir.display()),
                Err(e) => eprintln!("  Warning: Failed to clear logs: {e}"),
            },
            Some(_) => {
                eprintln!("  Logs directory does not exist, nothing to clear.");
            }
            None => {}
        }
    }

    // ── Logging setup ────────────────────────────────────────────────────
    // Always-on rolling log file under the platform state directory.
    // Level escalates via --debug flag or PLUQ_LOG env var.
    let _log_guard = init_logging(cli.debug);

    info!(
        command = ?std::env::args().map(|a| redact::scrub(&a).into_owned()).collect::<Vec<_>>(),
        "pluq starting"
    );

    // ── Cache cleaning ───────────────────────────────────────────────────
    if cli.clean_cache {
        match archive_cache::cache_dir() {
            Some(dir) if dir.exists() => {
                info!(path = %dir.display(), "clearing cache directory");
                match std::fs::remove_dir_all(&dir) {
                    Ok(_) => {
                        eprintln!("  Cache cleared: {}", dir.display());
                    }
                    Err(e) => {
                        warn!(path = %dir.display(), error = %e, "failed to clear cache");
                        eprintln!("  Warning: Failed to clear cache: {e}");
                    }
                }
            }
            Some(_) => {
                eprintln!("  Cache directory does not exist, nothing to clear.");
            }
            None => {}
        }
    }

    // ── Ctrl-C cancellation ───────────────────────────────────────────────
    // Shared flag: set by the signal handler, polled by scan/extraction loops.
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_handler = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_for_handler.store(true, Ordering::Relaxed);
        eprintln!("\n  Cancelling...");
    });

    let token_store = archive_auth::TokenStore::load_default()
        .inspect_err(
            |e| warn!(error = %e, "failed to load OAuth token store; continuing without OAuth"),
        )
        .ok()
        .map(std::sync::Arc::new);

    let run_result = match (cli.source, cli.command) {
        (None, None) => run_home(cancel, token_store, cli.encoding).await,
        (Some(src), None) => run_browse(src, cancel, token_store, cli.encoding).await,
        (None, Some(cmd)) => run_subcommand(cmd, cancel, token_store, cli.encoding).await,
        (Some(_), Some(_)) => {
            eprintln!("  Error: cannot pass both <SOURCE> and a subcommand");
            eprintln!("  Usage: pluq [OPTIONS] <SOURCE>");
            eprintln!("         pluq [OPTIONS] <COMMAND>");
            std::process::exit(2);
        }
    };

    if let Err(e) = run_result {
        // Clean exit on Ctrl-C — message already printed by the handler.
        if let Some(archive::ArchiveError::Cancelled) = e.downcast_ref::<archive::ArchiveError>() {
            std::process::exit(130); // Standard Unix exit code for Ctrl-C
        }
        // OAuth flow cancelled via Ctrl-C — "Cancelling..." already printed.
        if let Some(archive_auth::AuthError::Cancelled) =
            e.downcast_ref::<archive_auth::AuthError>()
        {
            std::process::exit(130);
        }
        if let Some(archive::ArchiveError::Transport(inner)) =
            e.downcast_ref::<archive::ArchiveError>()
            && let Some(pe) = classify_transport(inner.as_ref())
        {
            let suffix = unsupported_suffix(e.as_ref()).unwrap_or_default();
            eprintln!("  Error: {pe}{suffix}");
            std::process::exit(1);
        }
        let suffix = unsupported_suffix(e.as_ref()).unwrap_or_default();
        eprintln!("  Error: {e}{suffix}");
        std::process::exit(1);
    }
}

pub(crate) const REPO_URL: &str = "https://github.com/stefandekanski/pluq";
pub(crate) const ISSUES_NEW_URL: &str =
    "https://github.com/stefandekanski/pluq/issues/new?template=bug_report.yml";
pub(crate) const DISCUSSIONS_URL: &str = "https://github.com/stefandekanski/pluq/discussions";
const IDEAS_URL: &str = "https://github.com/stefandekanski/pluq/discussions/categories/ideas";

fn unsupported_suffix(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current = Some(e);
    while let Some(err) = current {
        if matches!(
            err.downcast_ref::<archive::ArchiveError>(),
            Some(archive::ArchiveError::UnsupportedFormat { .. })
        ) {
            return Some(format!("\n  → request support: {IDEAS_URL}"));
        }
        current = err.source();
    }
    None
}

fn source_config(token_store: Option<Arc<archive_auth::TokenStore>>) -> SourceConfig {
    let mut cfg = SourceConfig::new();
    if let Some(store) = token_store {
        cfg = cfg.with_token_store(store);
    }
    cfg
}

async fn run_home(
    _cancel: Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
    encoding: archive::FilenameEncoding,
) -> Result<(), Box<dyn std::error::Error>> {
    tui::run_home(token_store, encoding).await
}

async fn run_browse(
    source: String,
    _cancel: Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
    encoding: archive::FilenameEncoding,
) -> Result<(), Box<dyn std::error::Error>> {
    tui::run_with_source(source, token_store, encoding).await?;
    Ok(())
}

async fn run_subcommand(
    command: Command,
    cancel: Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
    encoding: archive::FilenameEncoding,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Auth { action } => {
            return auth_cmd::run_auth(action).await;
        }
        Command::BugReport => {
            return bug_report::run_bug_report().await;
        }
        Command::Feedback { bug, idea, say_hi } => {
            let url = if bug {
                ISSUES_NEW_URL
            } else if idea {
                IDEAS_URL
            } else if say_hi {
                DISCUSSIONS_URL
            } else {
                // No flag → Discussions home is the natural landing.
                eprintln!("  Tip: --bug to report a bug, --idea to suggest one, --say-hi to chat.");
                DISCUSSIONS_URL
            };
            if webbrowser::open(url).is_err() {
                eprintln!("  Could not open a browser. Visit:\n    {url}");
            }
            return Ok(());
        }
        Command::List { source, all } => {
            let archive = open_with_progress(&source, &cancel, token_store, encoding).await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            println!("  {:<12} {:<12} NAME", "SIZE", "COMPRESSED");
            println!("  {:<12} {:<12} ----", "----", "----------");

            for entry in &entries {
                if entry.is_directory {
                    println!("  {:<12} {:<12} {}/", "-", "-", entry.name);
                } else {
                    println!(
                        "  {:<12} {:<12} {}",
                        format_size(entry.uncompressed_size),
                        format_size(entry.compressed_size),
                        entry.name
                    );
                }
            }

            let file_count = entries.iter().filter(|e| !e.is_directory).count();
            let total_count = archive.entries().iter().filter(|e| !e.is_directory).count();
            if !all && file_count < total_count {
                println!(
                    "\n  {file_count} files ({} OS junk files hidden — use --all to show)",
                    total_count - file_count
                );
            } else {
                println!("\n  {file_count} files total");
            }
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "pluq", &mut std::io::stdout());
        }
        Command::Get {
            source,
            glob,
            output,
            all,
        } => {
            let pattern = glob
                .as_deref()
                .map(glob::Pattern::new)
                .transpose()
                .map_err(|e| format!("invalid glob pattern: {e}"))?;

            let config = source_config(token_store.clone());
            let reader = resolve(&source, &config)?;
            let archive = open_with_progress(&source, &cancel, token_store, encoding).await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            let output_dir = match output {
                Some(dir) => dir,
                None => reader
                    .resolved_name()
                    .await
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".")),
            };

            tokio::fs::create_dir_all(&output_dir).await?;

            // Pre-filter: drop directories, apply glob (if any).
            let safe_entries: Vec<&ArchiveEntry> = entries
                .iter()
                .copied()
                .filter(|e| !e.is_directory)
                .filter(|e| match &pattern {
                    Some(p) => glob_match::glob_matches(p, &e.name),
                    None => true,
                })
                .collect();

            if safe_entries.is_empty() {
                if let Some(p) = &pattern {
                    eprintln!("  No entries match glob '{}'", p.as_str());
                    std::process::exit(1);
                } else {
                    eprintln!("  Archive contains no extractable entries");
                    return Ok(());
                }
            }

            let mut reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            reporter.set_total_entries(safe_entries.len() as u64);
            let progress: Arc<Mutex<dyn archive::ProgressReporter>> =
                Arc::new(Mutex::new(reporter));

            let output_dir_ref = &output_dir;
            let sink_factory =
                |entry: &ArchiveEntry| -> Option<Box<dyn AsyncWrite + Send + Unpin>> {
                    let dest = output_dir_ref.join(&entry.name);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let file = std::fs::File::create(&dest).ok()?;
                    Some(Box::new(tokio::fs::File::from_std(file)))
                };

            let batch = archive
                .extract_selected(&safe_entries, sink_factory, progress, None)
                .await?;

            if !batch.failed.is_empty() {
                eprintln!("  {} file(s) failed:", batch.failed.len());
                for (name, err) in &batch.failed {
                    eprintln!("    {} — {}", name, err);
                }
            }
            println!(
                "\n  {} file(s) extracted ({}) to {}/",
                batch.succeeded,
                format_size(batch.bytes_written),
                output_dir.display()
            );
        }
        Command::Test { source, all } => {
            let archive = open_with_progress(&source, &cancel, token_store, encoding).await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            let file_count = entries.iter().filter(|e| !e.is_directory).count();
            let mut reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            reporter.set_total_entries(file_count as u64);
            let progress: Arc<Mutex<dyn archive::ProgressReporter>> =
                Arc::new(Mutex::new(reporter));

            let mut failures: usize = 0;

            for entry in &entries {
                if entry.is_directory {
                    continue;
                }

                if let Err(e) = archive.test(entry, progress.clone()).await {
                    eprintln!("  FAIL {} — {e}", entry.name);
                    failures += 1;
                }
            }

            let files_tested = entries.iter().filter(|e| !e.is_directory).count();
            if failures == 0 {
                println!("\n  All {files_tested} file(s) OK");
            } else {
                eprintln!("\n  {failures}/{files_tested} file(s) FAILED");
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Open an archive with scan progress reporting.
///
/// If the archive needs scanning (cloud RAR without Quick Open), displays
/// a progress bar on stderr. Otherwise returns immediately.
async fn open_with_progress(
    source: &str,
    cancel: &Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
    encoding: archive::FilenameEncoding,
) -> Result<archive_cache::CachedArchive, Box<dyn std::error::Error>> {
    eprintln!("  Opening {source}...");
    let config = source_config(token_store);
    let opened = archive_cache::open(
        source,
        &config,
        archive::ArchiveConfig {
            filename_encoding: encoding,
            ..Default::default()
        },
    )
    .await?;
    match opened {
        archive_cache::Opened::Ready(a) => Ok(a),
        archive_cache::Opened::NeedsScan(pending) => {
            let info = pending.scan_info().clone();
            eprintln!(
                "  Archive requires scanning{}",
                match &info {
                    archive::ScanInfo::Required {
                        estimated_bytes, ..
                    } => format!(" (~{})", format_size(*estimated_bytes)),
                    _ => String::new(),
                }
            );
            let reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            let progress: std::sync::Arc<std::sync::Mutex<dyn archive::ProgressReporter>> =
                std::sync::Arc::new(std::sync::Mutex::new(reporter));
            Ok(pending.scan(progress).await?)
        }
    }
}

/// Derive a default extraction subfolder from an archive source.
///
/// Strips the file extension from the source's filename component:
/// - `photos.rar` → `photos/`
/// - `/tmp/backup.zip` → `backup/`
/// - `https://example.com/data.rar5` → `data/`
/// - `archive.tar.gz` → `archive.tar/` (strips one extension)
///
/// Returns `None` if no meaningful stem can be extracted.
pub fn default_output_folder(source: &str) -> Option<PathBuf> {
    archive_source::suggest_name_from_str(source).map(PathBuf::from)
}

pub(crate) fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn log_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "pluq", "pluq").map(|dirs| {
        let mut p = dirs
            .state_dir()
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs.cache_dir().to_path_buf());
        p.push("logs");
        p
    })
}

fn prune_logs(dir: &std::path::Path, max_total_bytes: u64) {
    if !dir.exists() {
        return;
    }
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata()
                && metadata.is_file()
            {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("pluq.log") {
                    files.push((entry.path(), metadata));
                }
            }
        }
    }
    // Sort oldest first
    files.sort_by_key(|(_, meta)| meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH));

    let mut total_size: u64 = files.iter().map(|(_, meta)| meta.len()).sum();
    for (path, meta) in files {
        if total_size <= max_total_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total_size = total_size.saturating_sub(meta.len());
        }
    }
}

/// Initialize file-based logging. Returns a guard that must be held alive for the
/// duration of the program (dropping it flushes the writer).
///
/// Activated by `--debug` flag (sets level to `debug`) or the `PLUQ_LOG`
/// env var (accepts any tracing level: `trace`, `debug`, `info`, `warn`, `error`).
fn init_logging(debug_flag: bool) -> tracing_appender::non_blocking::WorkerGuard {
    let env_level = std::env::var("PLUQ_LOG").ok();

    let level = if debug_flag {
        "debug".to_string()
    } else {
        env_level.unwrap_or_else(|| "info".to_string())
    };

    let log_dir = log_path().unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&log_dir).ok();
    prune_logs(&log_dir, 5 * 1024 * 1024);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "pluq.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(redact::RedactMakeWriter::new(non_blocking))
        .with_env_filter(
            // Cover every workspace crate so `--debug` captures the cloud/auth/source paths
            // too (the provider readers log under their own targets, e.g. `archive_dropbox`).
            tracing_subscriber::EnvFilter::builder().parse_lossy(format!(
                "archive_core={level},archive={level},archive_cache={level},\
                 archive_gdrive={level},archive_dropbox={level},archive_source={level},\
                 archive_auth={level},pluq={level}"
            )),
        )
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        // pluq -> source: None, command: None
        let cli = Cli::try_parse_from(["pluq"]).unwrap();
        assert_eq!(cli.source, None);
        assert!(cli.command.is_none());

        // pluq file.zip -> source: Some("file.zip"), command: None
        let cli = Cli::try_parse_from(["pluq", "file.zip"]).unwrap();
        assert_eq!(cli.source.as_deref(), Some("file.zip"));
        assert!(cli.command.is_none());

        // pluq list file.zip -> source: None, command: Some(Command::List { ... })
        let cli = Cli::try_parse_from(["pluq", "list", "file.zip"]).unwrap();
        assert_eq!(cli.source, None);
        assert!(
            matches!(cli.command, Some(Command::List { ref source, all: false }) if source == "file.zip")
        );

        // pluq file.zip list file.zip -> parses (both branches populated). Runtime in
        // main() now rejects this with exit code 2; clap itself no longer flags it.
        let cli = Cli::try_parse_from(["pluq", "file.zip", "list", "file.zip"]).unwrap();
        assert!(cli.source.is_some());
        assert!(cli.command.is_some());

        // pluq get file.zip -> Get with no glob
        let cli = Cli::try_parse_from(["pluq", "get", "file.zip"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Get { ref source, glob: None, output: None, all: false }) if source == "file.zip"
        ));

        // pluq get file.zip *.jpg -o out/ --all
        let cli = Cli::try_parse_from(["pluq", "get", "file.zip", "*.jpg", "-o", "out/", "--all"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Get { ref source, glob: Some(ref g), output: Some(ref o), all: true })
                if source == "file.zip" && g == "*.jpg" && o.as_os_str() == "out/"
        ));

        // pluq completions bash -> Completions { shell: Bash }
        let cli = Cli::try_parse_from(["pluq", "completions", "bash"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Completions { shell: Shell::Bash })
        ));
    }

    #[test]
    fn test_clean_logs_flag() {
        // global flag — parses before a bare source
        let cli = Cli::try_parse_from(["pluq", "--clean-logs", "file.zip"]).unwrap();
        assert!(cli.clean_logs);

        // and after a subcommand (global = true)
        let cli = Cli::try_parse_from(["pluq", "get", "file.zip", "--clean-logs"]).unwrap();
        assert!(cli.clean_logs);

        // absent by default
        let cli = Cli::try_parse_from(["pluq", "file.zip"]).unwrap();
        assert!(!cli.clean_logs);

        // all three global flags combinable — --debug, --clean-cache, --clean-logs
        let cli = Cli::try_parse_from([
            "pluq",
            "get",
            "file.zip",
            "--debug",
            "--clean-cache",
            "--clean-logs",
        ])
        .unwrap();
        assert!(cli.debug);
        assert!(cli.clean_cache);
        assert!(cli.clean_logs);
    }

    #[test]
    fn test_default_output_folder() {
        assert_eq!(
            default_output_folder("photos.rar"),
            Some(PathBuf::from("photos"))
        );
        assert_eq!(
            default_output_folder("/tmp/backup.zip"),
            Some(PathBuf::from("backup"))
        );
        assert_eq!(
            default_output_folder("https://drive.google.com/file/d/xxx/data.rar?confirm=t"),
            Some(PathBuf::from("data"))
        );
        assert_eq!(
            default_output_folder("archive.tar.gz"),
            Some(PathBuf::from("archive.tar"))
        );
        assert_eq!(
            default_output_folder(".hidden.zip"),
            Some(PathBuf::from(".hidden"))
        );
        assert_eq!(
            default_output_folder(
                "https://drive.google.com/file/d/1ExampleGDriveFileId000000000000/view?usp=sharing"
            ),
            Some(PathBuf::from("1ExampleGDriveFileId000000000000"))
        );
        assert_eq!(
            default_output_folder(
                "https://drive.google.com/uc?id=1ExampleGDriveFileId000000000000&export=download"
            ),
            Some(PathBuf::from("1ExampleGDriveFileId000000000000"))
        );
        assert_eq!(default_output_folder(""), None);
    }

    #[test]
    fn test_cli_parsing_bug_report() {
        let cli = Cli::try_parse_from(["pluq", "bug-report"]).unwrap();
        assert!(matches!(cli.command, Some(Command::BugReport)));
    }

    #[test]
    fn test_cli_parsing_auth_add() {
        let cli = Cli::try_parse_from(["pluq", "auth", "add", "gdrive"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: auth_cmd::AuthAction::Add { .. }
            })
        ));
    }

    #[test]
    fn test_cli_parsing_auth_list() {
        let cli = Cli::try_parse_from(["pluq", "auth", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: auth_cmd::AuthAction::List
            })
        ));
    }

    #[test]
    fn test_cli_parsing_auth_remove() {
        let cli = Cli::try_parse_from(["pluq", "auth", "remove", "dropbox"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: auth_cmd::AuthAction::Remove { .. }
            })
        ));
    }

    #[test]
    fn test_cli_parsing_auth_invalid_provider() {
        let result = Cli::try_parse_from(["pluq", "auth", "add", "onedrive"]);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("clap ValueEnum must reject unknown provider"),
        };
        let msg = err.to_string();
        // Discoverability: the error must name the valid providers.
        assert!(msg.contains("gdrive"), "error should list gdrive: {msg}");
        assert!(msg.contains("dropbox"), "error should list dropbox: {msg}");
    }

    #[test]
    fn test_cli_parsing_auth_add_missing_provider_lists_providers() {
        // Bare `pluq auth add` must not be a dead-end: arg_required_else_help prints the
        // subcommand help, which enumerates the valid providers.
        let result = Cli::try_parse_from(["pluq", "auth", "add"]);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("bare `auth add` must error (no provider)"),
        };
        let msg = err.to_string();
        assert!(msg.contains("gdrive"), "help should list gdrive: {msg}");
        assert!(msg.contains("dropbox"), "help should list dropbox: {msg}");
    }

    #[test]
    fn test_cli_parsing_auth_with_timeout() {
        let cli =
            Cli::try_parse_from(["pluq", "auth", "add", "gdrive", "--timeout", "60"]).unwrap();
        if let Some(Command::Auth {
            action: auth_cmd::AuthAction::Add { timeout, .. },
        }) = cli.command
        {
            assert_eq!(timeout, 60);
        } else {
            panic!("expected Auth::Add with timeout=60");
        }
    }

    #[test]
    fn test_cli_parsing_global_clean_cache_with_subcommand_url() {
        // Regression: pluq --clean-cache list <URL> used to error.
        let cli =
            Cli::try_parse_from(["pluq", "--clean-cache", "list", "https://example.com/a.zip"])
                .expect("global --clean-cache must coexist with subcommand + URL source");
        assert!(cli.clean_cache);
        assert!(matches!(cli.command, Some(Command::List { .. })));
    }

    #[test]
    fn test_cli_parsing_global_debug_with_subcommand_url() {
        let cli = Cli::try_parse_from(["pluq", "--debug", "list", "https://example.com/a.zip"])
            .expect("global --debug must coexist with subcommand + URL source");
        assert!(cli.debug);
    }

    #[test]
    fn test_cli_parsing_global_flags_combined_with_subcommand() {
        let cli = Cli::try_parse_from([
            "pluq",
            "--debug",
            "--clean-cache",
            "get",
            "https://example.com/a.zip",
            "-o",
            "/tmp/out",
        ])
        .unwrap();
        assert!(cli.debug);
        assert!(cli.clean_cache);
        assert!(matches!(cli.command, Some(Command::Get { .. })));
    }

    #[test]
    fn test_cli_parsing_global_flags_with_bare_source() {
        // pluq --debug --clean-cache <bare-source>  (drops into TUI).
        let cli = Cli::try_parse_from(["pluq", "--debug", "--clean-cache", "file.zip"]).unwrap();
        assert!(cli.debug);
        assert!(cli.clean_cache);
        assert_eq!(cli.source.as_deref(), Some("file.zip"));
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parsing_global_flags_no_command_no_source() {
        // pluq --clean-cache  (cache wipe then Home TUI).
        let cli = Cli::try_parse_from(["pluq", "--clean-cache"]).unwrap();
        assert!(cli.clean_cache);
        assert!(cli.source.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parsing_flag_after_subcommand_works() {
        // global flags should also work in the subcommand-suffix position.
        let cli = Cli::try_parse_from(["pluq", "list", "--debug", "file.zip"]).unwrap();
        assert!(cli.debug);
        assert!(matches!(cli.command, Some(Command::List { .. })));
    }

    #[test]
    fn test_cli_parsing_feedback() {
        let cli = Cli::try_parse_from(["pluq", "feedback", "--bug"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Feedback { bug: true, .. })
        ));

        // mutually exclusive: --bug + --idea must error
        assert!(Cli::try_parse_from(["pluq", "feedback", "--bug", "--idea"]).is_err());

        // no flag is allowed
        assert!(Cli::try_parse_from(["pluq", "feedback"]).is_ok());
    }
    #[test]
    fn filename_encoding_is_global_and_validated() {
        for args in [
            vec!["pluq", "--encoding", "cp932", "list", "test.zip"],
            vec!["pluq", "list", "--encoding", "shift_jis", "test.zip"],
            vec!["pluq", "get", "test.zip", "--encoding", "gbk"],
            vec!["pluq", "test", "test.zip", "--encoding", "utf-8"],
            vec!["pluq", "--encoding", "cp932", "test.zip"],
            vec!["pluq", "--encoding", "auto"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
        let err =
            Cli::try_parse_from(["pluq", "list", "--encoding", "unknown-charset", "test.zip"]);
        assert!(err.is_err());
    }
}
