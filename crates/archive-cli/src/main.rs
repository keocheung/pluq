use std::path::PathBuf;

use clap::{Parser, Subcommand};

use archive_core::{ArchiveError, HttpReadAt, LocalReadAt, ReadAt, ZipArchive};
use bytes::Bytes;

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
}

/// Wrapper enum so we can use either reader without `dyn ReadAt`.
///
/// Rust's `impl Future` return types in traits are not dyn-compatible,
/// so we use static dispatch via an enum instead of `Box<dyn ReadAt>`.
enum Source {
    Local(LocalReadAt),
    Http(HttpReadAt),
}

impl ReadAt for Source {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ArchiveError> {
        match self {
            Source::Local(r) => r.read_range(offset, len).await,
            Source::Http(r) => r.read_range(offset, len).await,
        }
    }

    async fn size(&self) -> Result<u64, ArchiveError> {
        match self {
            Source::Local(r) => r.size().await,
            Source::Http(r) => r.size().await,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::List { source } => {
            let zip = open_zip(&source).await?;

            println!("  {:<12} {:<12} {}", "SIZE", "COMPRESSED", "NAME");
            println!("  {:<12} {:<12} {}", "----", "----------", "----");

            for entry in zip.entries() {
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

            println!(
                "\n  {} entries total",
                zip.entries().iter().filter(|e| !e.is_directory).count()
            );
        }
        Command::Extract {
            source,
            entry,
            output,
        } => {
            let zip = open_zip(&source).await?;

            let found = zip
                .entries()
                .iter()
                .find(|e| e.name == entry)
                .ok_or_else(|| format!("entry '{}' not found in archive", entry))?;

            let data = zip.extract(found).await?;

            // If output is a directory, use the entry's filename inside it.
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
    }

    Ok(())
}

/// Open a ZIP archive from either a local path or an HTTP URL.
async fn open_zip(source: &str) -> Result<ZipArchive<Source>, Box<dyn std::error::Error>> {
    let reader = if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::new();
        Source::Http(HttpReadAt::new(source, client))
    } else {
        Source::Local(LocalReadAt::new(source))
    };

    let zip = ZipArchive::open(reader).await?;
    Ok(zip)
}

/// Format a byte count as a human-readable string.
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
