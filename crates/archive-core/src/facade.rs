//! High-level entry point for opening and working with archives.
//!
//! ```ignore
//! let archive = ArchiveCore::open("https://example.com/data.zip").await?;
//! for entry in archive.entries() {
//!     println!("{}", entry.name);
//! }
//! let data = archive.extract_to_bytes(&archive.entries()[0]).await?;
//! ```

use bytes::Bytes;
use tokio::io::AsyncWrite;

use crate::io::Source;
use crate::junk;
use crate::zip::ZipArchive;
use crate::{ArchiveEntry, CoreError};

/// A format-agnostic archive handle.
///
/// Created via [`ArchiveCore::open`], which auto-detects the source type
/// (local file or HTTP URL) and the archive format (ZIP, RAR, 7z, ...)./
///
/// This is the primary public API for the core layer. The state layer
/// (`archive`) wraps this type — products should interact with
/// `archive::Archive`, not this type directly.
pub struct ArchiveCore {
    inner: ArchiveInner,
}

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `ArchiveCore`.
enum ArchiveInner {
    Zip(ZipArchive<Source>),
    // Future: Rar(...), SevenZip(...), Tar(...))
}

impl ArchiveCore {
    /// Open an archive from a local path or URL.
    ///
    /// Auto-detects:
    /// - **Source:** local file (any path) or HTTP URL (`http://` / `https://`)
    /// - **Format:** currently ZIP only (by attempting to parse); more formats coming
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Local file
    /// let archive = ArchiveCore::open("/path/to/file.zip").await?;
    ///
    /// // Cloud file (Dropbox, Drive, etc.)
    /// let archive = ArchiveCore::open("https://dropbox.com/...?dl=1").await?;
    /// ```
    pub async fn open(source: &str) -> Result<Self, CoreError> {
        let reader = Source::detect(source);

        // For now, try ZIP. When we add RAR/7z, we'll try multiple formats
        // or sniff magic bytes to pick the right parser.
        let zip = ZipArchive::open(reader).await?;

        Ok(Self {
            inner: ArchiveInner::Zip(zip),
        })
    }

    /// Return all entries (files and directories) in the archive, including
    /// OS-generated junk (`.DS_Store`, `__MACOSX/`, `Thumbs.db`, etc.).
    ///
    /// Prefer [`entries_clean`](Self::entries_clean) for user-facing output.
    pub fn entries(&self) -> &[ArchiveEntry] {
        match &self.inner {
            ArchiveInner::Zip(z) => z.entries(),
        }
    }

    /// Return entries with OS-generated junk files filtered out.
    ///
    /// This is the **default** for user-facing output (CLI, UI). Filters:
    /// - macOS: `__MACOSX/`, `._*` sidecars, `.DS_Store`
    /// - Windows: `Thumbs.db`, `desktop.ini`, `ehthumbs.db`  
    /// - Linux/KDE: `.directory`
    pub fn entries_clean(&self) -> Vec<&ArchiveEntry> {
        self.entries()
            .iter()
            .filter(|e| !junk::is_junk(e))
            .collect()
    }

    /// Extract a single entry, streaming decompressed bytes to `sink`.
    ///
    /// Returns the number of bytes written. CRC verification is mandatory.
    ///
    /// For convenience, see [`extract_to_bytes`](Self::extract_to_bytes).
    pub async fn extract<W: AsyncWrite + Send + Unpin>(
        &self,
        entry: &ArchiveEntry,
        sink: &mut W,
    ) -> Result<u64, CoreError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry, sink).await,
        }
    }

    /// Extract a single entry into memory, returning decompressed bytes.
    ///
    /// Convenience wrapper around [`extract`](Self::extract) for small entries.
    /// For large entries (multi-GB), use `extract()` with a file sink instead.
    pub async fn extract_to_bytes(&self, entry: &ArchiveEntry) -> Result<Bytes, CoreError> {
        let mut buf = Vec::with_capacity(entry.uncompressed_size as usize);
        self.extract(entry, &mut buf).await?;
        Ok(Bytes::from(buf))
    }

    /// Bytes that must be downloaded and decompressed to produce this entry.
    ///
    /// Used by the "extraction tax" UX for cloud sniping cost warnings.
    pub fn extraction_cost(&self, entry: &ArchiveEntry) -> u64 {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extraction_cost(entry),
        }
    }

    /// Decompress and verify CRC without retaining output.
    ///
    /// Used for archive integrity checking.
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), CoreError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry).await,
        }
    }
}
