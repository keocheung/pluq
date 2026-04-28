use serde::{Deserialize, Serialize};

/// Fingerprint for detecting whether a cached entry list is still valid.
///
/// Staleness detection uses the first available signal in priority order:
/// ETag (authoritative), Last-Modified, mtime, source_size (weakest).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveFingerprint {
    /// The source identifier — local path or URL.
    pub source: String,
    /// Total byte size of the source at cache time.
    pub source_size: u64,
    /// HTTP ETag header value, if the server provided one.
    pub etag: Option<String>,
    /// HTTP Last-Modified header value, if present.
    pub last_modified: Option<String>,
    /// Local file mtime as Unix timestamp (seconds), if available.
    pub mtime: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponseMetadata {
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
