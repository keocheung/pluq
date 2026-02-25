use serde::{Deserialize, Serialize};

/// Format-agnostic metadata for a single file inside an archive.
///
/// Every format parser (ZIP, RAR, 7z, …) produces `ArchiveEntry` values
/// so the rest of the application can work with a uniform type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// Full path of the entry inside the archive (e.g. `"photos/cat.jpg"`).
    pub name: String,

    /// Size of the entry's data as stored in the archive (after compression).
    pub compressed_size: u64,

    /// Original size of the entry's data before compression.
    pub uncompressed_size: u64,

    /// `true` if this entry represents a directory, not a file.
    pub is_directory: bool,

    /// Format-specific byte offset used internally for extraction.
    ///
    /// For ZIP this is the offset of the Local File Header.
    pub(crate) offset: u64,

    /// Format-specific compression method identifier.
    ///
    /// For ZIP: 0 = Stored (no compression), 8 = Deflate.
    pub(crate) compression_method: u16,
}
