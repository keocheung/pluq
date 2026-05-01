mod auth_cmd;
mod cli_progress;
mod glob_match;
pub mod home;
mod tui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use tracing::{info, warn};

use tokio::io::AsyncWrite;

use archive::{Archive, ArchiveEntry};
use archive_source::{ResolvedSource, SourceConfig, classify_transport, resolve};

/// pluq — surgical archive extraction from local or cloud storage.
#[derive(Parser)]
#[command(name = "pluq", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Open an archive directly in the TUI. Omit to launch the Home Screen.
    source: Option<String>,

    /// Enable debug logging to pluq.log (or set PLUQ_LOG=debug).
    #[arg(long, global = true)]
    debug: bool,

    /// Delete the persistent cache directory before starting the operation.
    #[arg(long, global = true)]
    clean_cache: bool,

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
    let cli = Cli::parse();

    // ── Logging setup ────────────────────────────────────────────────────
    // Writes to pluq.log in the current directory. Activated by
    // --debug flag or PLUQ_LOG env var (e.g. PLUQ_LOG=trace).
    // When neither is set, no subscriber is installed and all tracing macros
    // are zero-cost no-ops.
    let _log_guard = init_logging(cli.debug);

    info!(command = ?std::env::args().collect::<Vec<_>>(), "pluq starting");

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
        (None, None) => run_home(cancel, token_store).await,
        (Some(src), None) => run_browse(src, cancel, token_store).await,
        (None, Some(cmd)) => run_subcommand(cmd, cancel, token_store).await,
        (Some(_), Some(_)) => unreachable!(), // args_conflicts_with_subcommands
    };

    if let Err(e) = run_result {
        // Clean exit on Ctrl-C — message already printed by the handler.
        if let Some(archive::ArchiveError::Cancelled) = e.downcast_ref::<archive::ArchiveError>() {
            std::process::exit(130); // Standard Unix exit code for Ctrl-C
        }
        if let Some(archive::ArchiveError::Transport(inner)) =
            e.downcast_ref::<archive::ArchiveError>()
            && let Some(pe) = classify_transport(inner.as_ref())
        {
            eprintln!("  Error: {pe}");
            std::process::exit(1);
        }
        eprintln!("  Error: {e}");
        std::process::exit(1);
    }
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
) -> Result<(), Box<dyn std::error::Error>> {
    tui::run_home(token_store).await
}

async fn run_browse(
    source: String,
    _cancel: Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
) -> Result<(), Box<dyn std::error::Error>> {
    tui::run_with_source(source, token_store).await?;
    Ok(())
}

async fn run_subcommand(
    command: Command,
    cancel: Arc<AtomicBool>,
    token_store: Option<Arc<archive_auth::TokenStore>>,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Auth { action } => {
            return auth_cmd::run_auth(action, token_store).await;
        }
        Command::List { source, all } => {
            let archive = open_with_progress(&source, &cancel, token_store).await?;

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
            let archive = open_with_progress(&source, &cancel, token_store).await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            let output_dir = output.unwrap_or_else(|| {
                reader
                    .suggested_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
            });

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
            let archive = open_with_progress(&source, &cancel, token_store).await?;

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

                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_start(&entry.name, entry.uncompressed_size);
                }

                match archive.test(entry, progress.clone()).await {
                    Ok(()) => {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_complete(&entry.name, true);
                    }
                    Err(e) => {
                        let mut p = progress.lock().unwrap();
                        p.on_entry_complete(&entry.name, false);
                        drop(p);
                        eprintln!("  FAIL {} — {e}", entry.name);
                        failures += 1;
                    }
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
) -> Result<Archive<ResolvedSource>, Box<dyn std::error::Error>> {
    eprintln!("  Opening {source}...");
    let config = source_config(token_store);
    let opened = archive_cache::open(source, &config, archive::ArchiveConfig::default()).await?;
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

/// Initialize file-based logging. Returns a guard that must be held alive for the
/// duration of the program (dropping it flushes the writer).
///
/// Activated by `--debug` flag (sets level to `debug`) or the `PLUQ_LOG`
/// env var (accepts any tracing level: `trace`, `debug`, `info`, `warn`, `error`).
/// When neither is set, returns `None` and all tracing macros are zero-cost no-ops.
fn init_logging(debug_flag: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_level = std::env::var("PLUQ_LOG").ok();

    let level = if debug_flag {
        Some("debug".to_string())
    } else {
        env_level
    };

    let level = level?;

    let file_appender = tracing_appender::rolling::never(".", "pluq.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .parse_lossy(format!("archive_core={level},archive={level},pluq={level}")),
        )
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    Some(guard)
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

        // pluq file.zip list file.zip -> error (args_conflicts_with_subcommands)
        let res = Cli::try_parse_from(["pluq", "file.zip", "list", "file.zip"]);
        assert!(res.is_err());

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
        assert!(result.is_err()); // clap ValueEnum rejects unknown
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
}
