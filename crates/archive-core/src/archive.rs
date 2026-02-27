//! High-level entry point for opening and working with archives.
//!
//! ```ignore
//! let archive = Archive::open("https://example.com/data.zip").await?;
//! for entry in archive.entries() {
//!     println!("{}", entry.name);
//! }
//! let data = archive.extract(&archive.entries()[0]).await?;
//! ```

use bytes::Bytes;

use crate::junk;
use crate::source::Source;
use crate::zip::ZipArchive;
use crate::{ArchiveEntry, ArchiveError};

/// A format-agnostic archive handle.
///
/// Created via [`Archive::open`], which auto-detects the source type
/// (local file or HTTP URL) and the archive format (ZIP, RAR, 7z, ...).
///
/// This is the primary public API. CLI, UI, server, and dev code should
/// all go through this type rather than wiring up low-level readers
/// and format parsers manually.
pub struct Archive {
    inner: ArchiveInner,
}

/// Internal enum for format-specific archive implementations.
/// Hidden from consumers — they only see `Archive`.
enum ArchiveInner {
    Zip(ZipArchive<Source>),
    // Future: Rar(...), SevenZip(...), Tar(...)
}

impl Archive {
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
    /// let archive = Archive::open("/path/to/file.zip").await?;
    ///
    /// // Cloud file (Dropbox, Drive, etc.)
    /// let archive = Archive::open("https://dropbox.com/...?dl=1").await?;
    /// ```
    pub async fn open(source: &str) -> Result<Self, ArchiveError> {
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

    /// Extract a single entry's contents, decompressing if needed.
    ///
    /// CRC verification is mandatory — never returns corrupted bytes.
    pub async fn extract(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.extract(entry).await,
        }
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
    pub async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError> {
        match &self.inner {
            ArchiveInner::Zip(z) => z.test(entry).await,
        }
    }
}
