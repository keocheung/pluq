use archive_core::ArchiveEntry;
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

/// Cached metadata for an archive — entry list + fingerprint.
///
/// Format-agnostic: works for ZIP, RAR, 7z, etc. Supersedes [`ZipIndex`](archive_core::ZipIndex).
/// Stored as pretty-printed JSON alongside the archive or in an app cache directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveCache {
    pub fingerprint: ArchiveFingerprint,
    pub entries: Vec<ArchiveEntry>,
    /// Creation time as Unix timestamp (seconds).
    pub created_at: u64,
}

impl ArchiveCache {
    /// Return `true` if this cache is still valid for `current`.
    ///
    /// Checks ETag first (most authoritative), then Last-Modified, then mtime,
    /// then falls back to source size.
    pub fn is_valid(&self, current: &ArchiveFingerprint) -> bool {
        if self.fingerprint.source != current.source {
            return false;
        }
        // ETag is the most authoritative signal — if both sides have one, use it.
        if let (Some(cached), Some(live)) = (&self.fingerprint.etag, &current.etag) {
            return cached == live;
        }
        // Fall back through weaker signals.
        if let (Some(cached), Some(live)) = (&self.fingerprint.last_modified, &current.last_modified) {
            return cached == live;
        }
        if let (Some(cached), Some(live)) = (self.fingerprint.mtime, current.mtime) {
            return cached == live;
        }
        self.fingerprint.source_size == current.source_size
    }

    /// Serialize to pretty-printed JSON and write to `path`.
    pub fn save(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Deserialize from JSON at `path`.
    pub fn load(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json)
            .map_err(std::io::Error::other)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(source: &str, size: u64) -> ArchiveFingerprint {
        ArchiveFingerprint {
            source: source.into(),
            source_size: size,
            etag: None,
            last_modified: None,
            mtime: None,
        }
    }

    fn make_cache(fp: ArchiveFingerprint) -> ArchiveCache {
        ArchiveCache {
            fingerprint: fp,
            entries: vec![],
            created_at: 0,
        }
    }

    #[test]
    fn round_trip_empty_cache() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = make_cache(fingerprint("test.zip", 1234));
        cache.save(tmp.path()).unwrap();
        let loaded = ArchiveCache::load(tmp.path()).unwrap();
        assert_eq!(loaded.fingerprint, cache.fingerprint);
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn round_trip_preserves_entries() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let entry = ArchiveEntry::new_test("hello.txt", false);
        let mut cache = make_cache(fingerprint("test.zip", 100));
        cache.entries.push(entry);
        cache.save(tmp.path()).unwrap();
        let loaded = ArchiveCache::load(tmp.path()).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "hello.txt");
    }

    #[test]
    fn is_valid_matches_same_fingerprint() {
        let fp = fingerprint("test.zip", 999);
        let cache = make_cache(fp.clone());
        assert!(cache.is_valid(&fp));
    }

    #[test]
    fn is_valid_fails_on_source_mismatch() {
        let cache = make_cache(fingerprint("a.zip", 100));
        assert!(!cache.is_valid(&fingerprint("b.zip", 100)));
    }

    #[test]
    fn is_valid_uses_etag_when_present() {
        let mut fp_cached = fingerprint("f.zip", 100);
        fp_cached.etag = Some("abc123".into());
        let cache = make_cache(fp_cached);

        // Matching ETag → valid, even if size differs.
        let mut current = fingerprint("f.zip", 999);
        current.etag = Some("abc123".into());
        assert!(cache.is_valid(&current));

        // Different ETag → invalid.
        current.etag = Some("xyz".into());
        assert!(!cache.is_valid(&current));
    }

    #[test]
    fn is_valid_falls_back_to_size_without_etag() {
        let cache = make_cache(fingerprint("f.zip", 512));
        assert!(cache.is_valid(&fingerprint("f.zip", 512)));
        assert!(!cache.is_valid(&fingerprint("f.zip", 513)));
    }
}
