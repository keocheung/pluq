mod cli_progress;
pub mod home;
mod tui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tracing::{info, warn};

use tokio::io::AsyncWrite;

use archive::{Archive, ArchiveEntry};
use archive_source::{ResolvedSource, SourceConfig, classify_transport, resolve};

/// Archive Sniper — surgical archive extraction from local or cloud storage.
#[derive(Parser)]
#[command(name = "snipe", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Open an archive directly in the TUI. Omit to launch the Home Screen.
    source: Option<String>,

    /// Enable debug logging to archive-sniper.log (or set ARCHIVE_SNIPER_LOG=debug).
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
    /// Extract a single file from an archive.
    Extract {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
        /// Name of the entry to extract (as shown by `list`).
        entry: String,
        /// Output file path.
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
    },
    /// Extract all files from an archive to a directory.
    ExtractAll {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // ── Logging setup ────────────────────────────────────────────────────
    // Writes to archive-sniper.log in the current directory. Activated by
    // --debug flag or ARCHIVE_SNIPER_LOG env var (e.g. ARCHIVE_SNIPER_LOG=trace).
    // When neither is set, no subscriber is installed and all tracing macros
    // are zero-cost no-ops.
    let _log_guard = init_logging(cli.debug);

    info!(command = ?std::env::args().collect::<Vec<_>>(), "snipe starting");

    // ── Cache cleaning ───────────────────────────────────────────────────
    if cli.clean_cache {
        match archive::cache_dir() {
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

    let run_result = match (cli.source, cli.command) {
        (None, None) => run_home(cancel).await,
        (Some(src), None) => run_browse(src, cancel).await,
        (None, Some(cmd)) => run_subcommand(cmd, cancel).await,
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

async fn run_home(_cancel: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    tui::run_home().await
}

async fn run_browse(
    source: String,
    _cancel: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = SourceConfig::new();
    let reader = resolve(&source, &config)?;
    tui::run_with_source(source, reader).await?;
    Ok(())
}

async fn run_subcommand(
    command: Command,
    cancel: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::List { source, all } => {
            let archive = open_with_progress(&source, &cancel).await?;

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
        Command::Extract {
            source,
            entry,
            output,
        } => {
            let archive = open_with_progress(&source, &cancel).await?;

            let found = archive
                .entries()
                .iter()
                .find(|e| e.name == entry)
                .ok_or_else(|| format!("entry '{}' not found in archive", entry))?;

            let out_path = if output.is_dir() {
                let file_name = std::path::Path::new(&entry).file_name().unwrap_or_default();
                output.join(file_name)
            } else {
                output
            };

            // Ensure parent directory exists.
            if let Some(parent) = out_path.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent).await?;
            }

            let reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            let progress: Arc<Mutex<dyn archive::ProgressReporter>> =
                Arc::new(Mutex::new(reporter));

            {
                let mut p = progress.lock().unwrap();
                p.on_entry_start(&found.name, found.uncompressed_size);
            }

            let mut file = tokio::fs::File::create(&out_path).await?;
            let bytes_written = archive
                .extract(found, &mut file, progress.clone(), None)
                .await?;

            {
                let mut p = progress.lock().unwrap();
                p.on_entry_complete(&found.name, true);
            }

            println!(
                "  Extracted '{}' → {} ({})",
                entry,
                out_path.display(),
                format_size(bytes_written)
            );
        }
        Command::ExtractAll {
            source,
            output,
            all,
        } => {
            let config = SourceConfig::new();
            let reader = resolve(&source, &config)?;
            let archive = open_with_progress(&source, &cancel).await?;

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

            // Pre-filter: skip directories and unsafe paths before calling extract_selected.
            // CLI-local path traversal guard (core-level sanitization is a future phase).
            let safe_entries: Vec<&ArchiveEntry> = entries
                .iter()
                .copied()
                .filter(|e| !e.is_directory)
                .filter(|e| {
                    let safe = !e.name.split('/').any(|seg| seg == "..")
                        && !e.name.starts_with('/')
                        && !e.name.starts_with('\\');
                    if !safe {
                        eprintln!("  SKIP {} (unsafe path)", e.name);
                    }
                    safe
                })
                .collect();

            let skipped = entries.iter().filter(|e| !e.is_directory).count() - safe_entries.len();

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

            if skipped > 0 {
                eprintln!("\n  {skipped} file(s) skipped (unsafe paths)");
            }
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
            let archive = open_with_progress(&source, &cancel).await?;

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
) -> Result<Archive<ResolvedSource>, Box<dyn std::error::Error>> {
    eprintln!("  Opening {source}...");
    let config = SourceConfig::new();
    let reader = resolve(source, &config)?;
    let opened = Archive::open_reader(source, reader).await?;
    match opened {
        archive::Opened::Ready(a) => Ok(a),
        archive::Opened::NeedsScan(pending) => {
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
/// Activated by `--debug` flag (sets level to `debug`) or the `ARCHIVE_SNIPER_LOG`
/// env var (accepts any tracing level: `trace`, `debug`, `info`, `warn`, `error`).
/// When neither is set, returns `None` and all tracing macros are zero-cost no-ops.
fn init_logging(debug_flag: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let env_level = std::env::var("ARCHIVE_SNIPER_LOG").ok();

    let level = if debug_flag {
        Some("debug".to_string())
    } else {
        env_level
    };

    let level = level?;

    let file_appender = tracing_appender::rolling::never(".", "archive-sniper.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder().parse_lossy(format!(
                "archive_core={level},archive={level},archive_cli={level}"
            )),
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
        // snipe -> source: None, command: None
        let cli = Cli::try_parse_from(["snipe"]).unwrap();
        assert_eq!(cli.source, None);
        assert!(cli.command.is_none());

        // snipe file.zip -> source: Some("file.zip"), command: None
        let cli = Cli::try_parse_from(["snipe", "file.zip"]).unwrap();
        assert_eq!(cli.source.as_deref(), Some("file.zip"));
        assert!(cli.command.is_none());

        // snipe list file.zip -> source: None, command: Some(Command::List { ... })
        let cli = Cli::try_parse_from(["snipe", "list", "file.zip"]).unwrap();
        assert_eq!(cli.source, None);
        assert!(
            matches!(cli.command, Some(Command::List { ref source, all: false }) if source == "file.zip")
        );

        // snipe file.zip list file.zip -> error (args_conflicts_with_subcommands)
        let res = Cli::try_parse_from(["snipe", "file.zip", "list", "file.zip"]);
        assert!(res.is_err());

        // snipe extract file.zip foo.txt -o out/ -> Extract variant
        let cli =
            Cli::try_parse_from(["snipe", "extract", "file.zip", "foo.txt", "-o", "out/"]).unwrap();
        assert_eq!(cli.source, None);
        assert!(
            matches!(cli.command, Some(Command::Extract { ref source, ref entry, ref output }) if source == "file.zip" && entry == "foo.txt" && output.as_os_str() == "out/")
        );
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
}
