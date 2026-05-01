use std::path::PathBuf;

use archive_core::fingerprint::ArchiveFingerprint;
use archive_core::ArchiveEntry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use archive_core::FormatCacheState;

/// Cached metadata for an archive — entry list + format state + fingerprint.
///
/// Format-agnostic at the fingerprint level. Format-specific extraction data
/// lives in [`FormatCacheState`]. Stored as pretty-printed JSON in the platform
/// cache directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveCache {
    pub fingerprint: ArchiveFingerprint,
    pub entries: Vec<ArchiveEntry>,
    pub format_state: FormatCacheState,
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
        if let (Some(cached), Some(live)) =
            (&self.fingerprint.last_modified, &current.last_modified)
        {
            return cached == live;
        }
        if let (Some(cached), Some(live)) = (self.fingerprint.mtime, current.mtime) {
            return cached == live;
        }
        self.fingerprint.source_size == current.source_size
    }

    /// Serialize to pretty-printed JSON and write to `path`.
    pub fn save(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Deserialize from JSON at `path`.
    pub fn load(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(std::io::Error::other)
    }
}

// ── Cache directory helpers ────────────────────────────────────────────────────

/// Default cache directory (platform-aware):
/// Linux: `~/.cache/pluq/`
/// macOS: `~/Library/Caches/pluq/`
/// Windows: `{LOCALAPPDATA}/pluq/`
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("pluq"))
}

/// Deterministic cache file path for a source URL or local path.
/// SHA-256 hash of the source string, truncated to 32 hex chars.
pub fn cache_path_for(source: &str) -> Option<PathBuf> {
    cache_dir().map(|dir| {
        let digest = Sha256::digest(source.as_bytes());
        let hash: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        dir.join(format!("{hash}.json"))
    })
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
            format_state: FormatCacheState::Zip { locations: vec![] },
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

    #[test]
    fn round_trip_rar_format_state() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = ArchiveCache {
            fingerprint: fingerprint("test.rar", 2048),
            entries: vec![],
            format_state: FormatCacheState::Rar {
                locations: vec![],
                archive_preamble: vec![0x52, 0x61, 0x72, 0x21],
                end_block: vec![0xFF],
                archive_size: 2048,
            },
            created_at: 1234567890,
        };
        cache.save(tmp.path()).unwrap();
        let loaded = ArchiveCache::load(tmp.path()).unwrap();
        assert_eq!(loaded.fingerprint, cache.fingerprint);
        assert_eq!(loaded.created_at, 1234567890);
        if let FormatCacheState::Rar {
            archive_preamble,
            archive_size,
            ..
        } = loaded.format_state
        {
            assert_eq!(archive_preamble, vec![0x52, 0x61, 0x72, 0x21]);
            assert_eq!(archive_size, 2048);
        } else {
            panic!("expected Rar format state");
        }
    }

    #[test]
    fn round_trip_rar4_format_state() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = ArchiveCache {
            fingerprint: fingerprint("test4.rar", 1024),
            entries: vec![],
            format_state: FormatCacheState::Rar4 {
                locations: vec![],
                archive_preamble: vec![0x52, 0x61, 0x72],
                end_block: vec![0x00],
                archive_size: 1024,
            },
            created_at: 0,
        };
        cache.save(tmp.path()).unwrap();
        let loaded = ArchiveCache::load(tmp.path()).unwrap();
        if let FormatCacheState::Rar4 {
            archive_preamble,
            archive_size,
            ..
        } = loaded.format_state
        {
            assert_eq!(archive_preamble, vec![0x52, 0x61, 0x72]);
            assert_eq!(archive_size, 1024);
        } else {
            panic!("expected Rar4 format state");
        }
    }

    #[test]
    fn cache_path_for_is_deterministic() {
        let p1 = cache_path_for("test.rar");
        let p2 = cache_path_for("test.rar");
        assert_eq!(p1, p2);
        // Different sources produce different paths.
        let p3 = cache_path_for("other.rar");
        assert_ne!(p1, p3);
    }

    #[test]
    fn cache_path_for_ends_with_json() {
        if let Some(path) = cache_path_for("test.rar") {
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("json"));
        }
    }
}
