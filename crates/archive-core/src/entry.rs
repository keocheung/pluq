use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::codec::CompressionMethod;

/// Format-agnostic metadata for a single file inside an archive.
///
/// Every format parser (ZIP, RAR, 7z, …) produces `ArchiveEntry` values
/// so the rest of the application can work with a uniform type.
///
/// # Format-specific location data
///
/// `ArchiveEntry` contains **zero format-specific fields**. Byte offsets,
/// folder indexes, CRC values, and other per-format location details are held
/// privately by the format parser in a parallel array keyed by [`index`].
/// This keeps the public API stable as new formats are added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// Full path of the entry inside the archive (e.g. `"photos/cat.jpg"`).
    /// Always `/`-separated, even on Windows archives.
    pub name: String,

    /// Size of the entry's data as stored in the archive (after compression).
    pub compressed_size: u64,

    /// Original size of the entry's data before compression.
    pub uncompressed_size: u64,

    /// `true` if this entry represents a directory, not a file.
    pub is_directory: bool,

    /// `true` if this entry's data is encrypted.
    ///
    /// Attempting to extract an encrypted entry without a password will return
    /// [`ArchiveError::PasswordRequired`].
    pub is_encrypted: bool,

    /// Last-modified timestamp, if present in the archive.
    ///
    /// Precision and epoch vary by format (ZIP: 2-second DOS precision,
    /// 7z: 100-nanosecond Windows FILETIME).
    #[serde(
        serialize_with = "serialize_system_time",
        deserialize_with = "deserialize_system_time",
        default
    )]
    pub modified_at: Option<SystemTime>,

    /// Compression algorithm used for this entry — for display/informational purposes only.
    ///
    /// `None` if the format parser could not map the raw method ID to a known algorithm.
    pub compression_method: Option<CompressionMethod>,

    /// Internal index into the format parser's private location table.
    ///
    /// This is NOT a stable public identifier — it is implementation detail.
    /// Consumers must not store or compare this value across archive re-opens.
    pub(crate) index: usize,
}

// ── SystemTime serde helpers ───────────────────────────────────────────────
//
// SystemTime is not directly serde-serialisable, so we round-trip through
// seconds since the Unix epoch.

fn serialize_system_time<S>(time: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match time {
        Some(t) => {
            let secs = t
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            serializer.serialize_some(&secs)
        }
        None => serializer.serialize_none(),
    }
}

fn deserialize_system_time<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<u64> = Option::deserialize(deserializer)?;
    Ok(opt.map(|secs| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)))
}
