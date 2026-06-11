use std::path::PathBuf;

use archive_core::fingerprint::ArchiveFingerprint;
use archive_core::ArchiveEntry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use archive_core::FormatCacheState;

/// Bump this constant whenever the on-disk shape of [`ArchiveCache`] or any
/// type it transitively contains changes (new field without a `default`,
/// variant rename, etc.). A mismatch triggers a silent re-scan — no migration.
///
/// **Current value: 2.**
pub const SCHEMA_VERSION: u32 = 2;

/// Cached metadata for an archive — entry list + format state + fingerprint.
///
/// Format-agnostic at the fingerprint level. Format-specific extraction data
/// lives in [`FormatCacheState`]. Stored as pretty-printed JSON in the platform
/// cache directory.
///
/// The `schema_version` field guards against stale on-disk caches surviving a
/// format-shape change. Older files that lack this field deserialise it as `0`
/// (via `#[serde(default)]`) and are treated as version-mismatched → re-scan.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveCache {
    /// Schema version written by the code that created this cache file.
    /// Compared against [`SCHEMA_VERSION`] in [`ArchiveCache::is_valid`].
    /// Defaults to `0` when absent (old cache files) so they are uniformly rejected.
    #[serde(default)]
    pub schema_version: u32,
    pub fingerprint: ArchiveFingerprint,
    pub entries: Vec<ArchiveEntry>,
    pub format_state: FormatCacheState,
    /// Creation time as Unix timestamp (seconds).
    pub created_at: u64,
}

impl ArchiveCache {
    /// Return `true` if this cache is still valid for `current`.
    ///
    /// **Schema version is checked first** — cheapest and most decisive. If the
    /// stored version differs from [`SCHEMA_VERSION`] the entire cache is stale
    /// regardless of fingerprint equality.
    ///
    /// Then checks ETag (most authoritative), Last-Modified, mtime, and finally
    /// source size.
    pub fn is_valid(&self, current: &ArchiveFingerprint) -> bool {
        self.invalidity_reason(current).is_none()
    }

    /// Returns `Some(reason)` if this cache is **not** valid for `current`, else
    /// `None`. The reason is a static string for logging/diagnostics — it names
    /// which fingerprint signal rejected the cache (schema, source, etag,
    /// last-modified, mtime, or size). Never shown to users.
    ///
    /// Same precedence as [`Self::is_valid`]: schema first, then source, then ETag
    /// (decisive when both present), then Last-Modified, mtime, and size.
    pub fn invalidity_reason(&self, current: &ArchiveFingerprint) -> Option<&'static str> {
        // Version check first: stale schema → immediate rejection.
        if self.schema_version != SCHEMA_VERSION {
            return Some("schema version");
        }
        if self.fingerprint.source != current.source {
            return Some("source string");
        }
        // ETag is the most authoritative signal — if both sides have one, use it.
        if let (Some(cached), Some(live)) = (&self.fingerprint.etag, &current.etag) {
            return (cached != live).then_some("etag");
        }
        // Fall back through weaker signals.
        if let (Some(cached), Some(live)) =
            (&self.fingerprint.last_modified, &current.last_modified)
        {
            return (cached != live).then_some("last-modified");
        }
        if let (Some(cached), Some(live)) = (self.fingerprint.mtime, current.mtime) {
            return (cached != live).then_some("mtime");
        }
        (self.fingerprint.source_size != current.source_size).then_some("source size")
    }

    /// Serialize to pretty-printed JSON and write to `path`.
    pub fn save(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Deserialize from JSON at `path`.
    ///
    /// If the file deserializes successfully but carries an old `schema_version`,
    /// this function emits a single `info!` log line and returns an
    /// [`std::io::ErrorKind::InvalidData`] error so callers treat it identically to
    /// a missing or corrupt cache file (→ re-scan, no crash).
    pub fn load(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let json = std::fs::read_to_string(path)?;
        let cache: Self = serde_json::from_str(&json).map_err(std::io::Error::other)?;
        if cache.schema_version != SCHEMA_VERSION {
            info!(
                old = cache.schema_version,
                expected = SCHEMA_VERSION,
                "cache schema mismatch (got {}, expected {}); rescanning",
                cache.schema_version,
                SCHEMA_VERSION,
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache schema version mismatch",
            ));
        }
        Ok(cache)
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
            schema_version: SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION,
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
    fn is_valid_fails_on_schema_mismatch() {
        // Same source + size, but schema_version = 0 (old cache file).
        let fp = fingerprint("schema_test.zip", 42);
        let cache = ArchiveCache {
            schema_version: 0, // deliberate mismatch
            fingerprint: fp.clone(),
            entries: vec![],
            format_state: FormatCacheState::Zip { locations: vec![] },
            created_at: 0,
        };
        assert!(
            !cache.is_valid(&fp),
            "is_valid must return false when schema_version != SCHEMA_VERSION"
        );
    }

    #[test]
    fn load_rejects_schema_mismatch_with_invalid_data() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write a cache with schema_version = 0 directly.
        let old_cache = ArchiveCache {
            schema_version: 0,
            fingerprint: fingerprint("old.zip", 100),
            entries: vec![],
            format_state: FormatCacheState::Zip { locations: vec![] },
            created_at: 0,
        };
        // Bypass save() to write schema_version: 0 without triggering any guard.
        let json = serde_json::to_string_pretty(&old_cache).unwrap();
        std::fs::write(tmp.path(), json).unwrap();

        let err = ArchiveCache::load(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
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
