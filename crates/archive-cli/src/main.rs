mod cli_progress;
mod provider;
mod tui;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;

use clap::{Parser, Subcommand};
use tracing::info;

use archive::Archive;

/// Archive Sniper — surgical archive extraction from local or cloud storage.
#[derive(Parser)]
#[command(name = "snipe", version, about)]
struct Cli {
    /// Enable debug logging to archive-sniper.log (or set ARCHIVE_SNIPER_LOG=debug).
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive archive browser.
    Browse {
        /// Path to a local file or an HTTP(S) URL.
        source: String,
    },
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

    if let Err(e) = run(cli.command).await {
        if let Some(archive::ArchiveError::Transport(inner)) =
            e.downcast_ref::<archive::ArchiveError>()
            && let Some(msg) = provider::format_transport_error(inner.as_ref())
        {
            eprintln!("  Error: {msg}");
            std::process::exit(1);
        }
        eprintln!("  Error: {e}");
        std::process::exit(1);
    }
}

async fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Browse { source } => {
            let reader = provider::build_reader(&source)?;
            tui::run(source, reader).await?;
        }
        Command::List { source, all } => {
            let cancel = Arc::new(AtomicBool::new(false));
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
            let cancel = Arc::new(AtomicBool::new(false));
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
            if let Some(parent) = out_path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }

            let reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            let progress: Arc<Mutex<dyn archive::ProgressReporter>> =
                Arc::new(Mutex::new(reporter));

            {
                let mut p = progress.lock().unwrap();
                p.on_entry_start(&found.name, found.uncompressed_size);
            }

            let mut file = tokio::fs::File::create(&out_path).await?;
            let bytes_written = archive.extract(found, &mut file, progress.clone(), None).await?;

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
        Command::ExtractAll { source, output, all } => {
            let cancel = Arc::new(AtomicBool::new(false));
            let archive = open_with_progress(&source, &cancel).await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            let output_dir = output
                .unwrap_or_else(|| default_output_folder(&source).unwrap_or_else(|| PathBuf::from(".")));

            tokio::fs::create_dir_all(&output_dir).await?;

            let file_count = entries.iter().filter(|e| !e.is_directory).count();
            let mut reporter = cli_progress::CliProgressReporter::new(cancel.clone());
            reporter.set_total_entries(file_count as u64);
            let progress: Arc<Mutex<dyn archive::ProgressReporter>> =
                Arc::new(Mutex::new(reporter));

            let mut extracted: usize = 0;
            let mut skipped: usize = 0;

            for entry in &entries {
                if entry.is_directory {
                    continue;
                }

                // CLI-local path traversal guard (core-level sanitization is a future phase).
                if entry.name.split('/').any(|seg| seg == "..")
                    || entry.name.starts_with('/')
                    || entry.name.starts_with('\\')
                {
                    eprintln!("  SKIP {} (unsafe path)", entry.name);
                    skipped += 1;
                    continue;
                }

                let dest = output_dir.join(&entry.name);
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_start(&entry.name, entry.uncompressed_size);
                }

                let mut file = tokio::fs::File::create(&dest).await?;
                archive.extract(entry, &mut file, progress.clone(), None).await?;

                {
                    let mut p = progress.lock().unwrap();
                    p.on_entry_complete(&entry.name, true);
                }
                extracted += 1;
            }

            if skipped > 0 {
                eprintln!("\n  {skipped} file(s) skipped (unsafe paths)");
            }
            println!(
                "\n  Extracted {extracted} file(s) to {}/",
                output_dir.display()
            );
        }
        Command::Test { source, all } => {
            let cancel = Arc::new(AtomicBool::new(false));
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
) -> Result<Archive, Box<dyn std::error::Error>> {
    eprintln!("  Opening {source}...");
    let opened = open_archive(source).await?;
    match opened {
        archive::Opened::Ready(a) => Ok(a),
        archive::Opened::NeedsScan(pending) => {
            let info = pending.scan_info().clone();
            eprintln!(
                "  Archive requires scanning{}",
                match &info {
                    archive::ScanInfo::Required { estimated_bytes, .. } =>
                        format!(" (~{})", format_size(*estimated_bytes)),
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

/// Open an archive, using the provider dispatch for cloud URLs.
async fn open_archive(source: &str) -> Result<archive::Opened, Box<dyn std::error::Error>> {
    match provider::build_reader(source)? {
        Some(reader) => Ok(Archive::open_reader(source, reader).await?),
        None => Ok(Archive::open(source).await?),
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
    // For Google Drive "view" links, the last segment is just "view".
    // Try to extract the file ID instead.
    if archive_gdrive::is_gdrive_url(source) {
        if let Some(id) = archive_gdrive::extract_file_id(source) {
            // If the URL ends in "view", "uc", or "open" (ignoring query), use the ID.
            let filename = source
                .split('?')
                .next()?
                .rsplit('/')
                .next()
                .or_else(|| source.rsplit('\\').next())?;
            if filename == "view" || filename == "uc" || filename == "open" {
                return Some(PathBuf::from(id));
            }
        }
    }

    // Extract filename from URL path or filesystem path.
    let filename = source
        .rsplit('/')
        .next()
        .or_else(|| source.rsplit('\\').next())?;
    // Strip query string if URL.
    let filename = filename.split('?').next()?;
    let stem = Path::new(filename).file_stem()?;
    if stem.is_empty() {
        return None;
    }
    Some(PathBuf::from(stem))
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
            tracing_subscriber::EnvFilter::builder()
                .parse_lossy(format!("archive_core={level},archive={level},archive_cli={level}"))
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
            default_output_folder(
                "https://drive.google.com/file/d/xxx/data.rar?confirm=t"
            ),
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
