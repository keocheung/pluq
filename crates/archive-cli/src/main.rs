use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use archive::Archive;

/// Archive Sniper — surgical archive extraction from local or cloud storage.
#[derive(Parser)]
#[command(name = "snipe", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::List { source, all } => {
            let archive = Archive::open(&source).await?.ready().await?;

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
            let archive = Archive::open(&source).await?.ready().await?;

            let found = archive
                .entries()
                .iter()
                .find(|e| e.name == entry)
                .ok_or_else(|| format!("entry '{}' not found in archive", entry))?;

            let data = archive.extract_to_bytes(found, None).await?;

            let out_path = if output.is_dir() {
                let file_name = std::path::Path::new(&entry).file_name().unwrap_or_default();
                output.join(file_name)
            } else {
                output
            };

            tokio::fs::write(&out_path, &data).await?;
            println!(
                "  Extracted '{}' → {} ({})",
                entry,
                out_path.display(),
                format_size(data.len() as u64)
            );
        }
        Command::Test { source, all } => {
            let archive = Archive::open(&source).await?.ready().await?;

            let entries: Vec<_> = if all {
                archive.entries().iter().collect()
            } else {
                archive.entries_clean()
            };

            let mut failures: usize = 0;

            for entry in &entries {
                if entry.is_directory {
                    continue;
                }
                match archive.test(entry).await {
                    Ok(()) => println!("  OK  {}", entry.name),
                    Err(e) => {
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
    // Extract filename from URL path or filesystem path.
    let filename = source.rsplit('/').next().or_else(|| source.rsplit('\\').next())?;
    // Strip query string if URL.
    let filename = filename.split('?').next()?;
    let stem = Path::new(filename).file_stem()?;
    if stem.is_empty() {
        return None;
    }
    Some(PathBuf::from(stem))
}

fn format_size(bytes: u64) -> String {
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
        assert_eq!(default_output_folder(""), None);
    }
}
