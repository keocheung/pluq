use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::provider::Provider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookmarkEntry {
    /// User-editable display name. Defaults to `ResolvedSource::suggested_name()`
    /// at save time, but the user can override in the overlay (design spec §4.3).
    pub name: String,
    /// The raw source string (URL or path) — dedupe key for *matching against Recent*.
    /// NOT the uniqueness key within Bookmarks (name is, see `add` below).
    pub source: String,
    pub provider: Provider,
    pub size: u64,
    /// Set on first save; not updated on subsequent opens.
    pub last_opened: DateTime<Utc>,
}

#[derive(Default)]
pub struct BookmarkList {
    entries: Vec<BookmarkEntry>,
    /// Backing file. `Some` only for lists created via `load()`; `save()` is a
    /// no-op when `None`, so test-constructed lists never touch real config.
    path: Option<PathBuf>,
}

const MAX_BOOKMARKS: usize = 50;

impl BookmarkList {
    /// Read `~/.config/pluq/bookmarks.json`. Returns an empty list if the
    /// file does not exist, is empty, or fails to parse (log a warning on parse
    /// failure, never propagate).
    pub fn load() -> Self {
        match bookmarks_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Internal load helper for testing and main load. Records `path` so a
    /// later `save()` writes back to the same file.
    fn load_from(path: &Path) -> Self {
        Self {
            entries: read_entries(path),
            path: Some(path.to_path_buf()),
        }
    }

    /// Write atomically via tempfile + rename. Creates the parent directory
    /// with `fs::create_dir_all` if missing. Errors are logged and swallowed
    /// — the Home Screen must still function if the disk is read-only.
    /// No-op for lists without a backing path (test-constructed lists).
    pub fn save(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if let Err(e) = self.save_to(path) {
            tracing::error!("Failed to save bookmarks.json to {}: {}", path.display(), e);
        }
    }

    /// Internal save helper for testing and main save.
    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("json.tmp");
        let content = serde_json::to_vec_pretty(&self.entries).map_err(std::io::Error::other)?;

        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Add a new bookmark at the **head** of the list. Caller must have already
    /// resolved any name collision — this method assumes `entry.name` is unique.
    /// Caps at `MAX_BOOKMARKS`; silently drops the tail if over.
    pub fn add(&mut self, entry: BookmarkEntry) {
        self.entries.insert(0, entry);
        if self.entries.len() > MAX_BOOKMARKS {
            self.entries.truncate(MAX_BOOKMARKS);
        }
    }

    /// Rename the entry at `index` to `new_name`. Returns `Err(())` if the
    /// name collides with a different existing entry. Out-of-range → no-op
    /// returning `Ok(())` (consistent with `remove`).
    #[allow(clippy::result_unit_err)]
    pub fn rename(&mut self, index: usize, new_name: String) -> Result<(), ()> {
        if index >= self.entries.len() {
            return Ok(());
        }
        let collides = self
            .entries
            .iter()
            .enumerate()
            .any(|(i, e)| i != index && e.name == new_name);
        if collides {
            return Err(());
        }
        self.entries[index].name = new_name;
        Ok(())
    }

    /// Overwrite an existing bookmark with the given name. Replaces the matched
    /// entry in place (preserves list order). Returns true if a match was found.
    /// Used by the collision prompt's `(o) Overwrite` branch.
    pub fn overwrite_by_name(&mut self, entry: BookmarkEntry) -> bool {
        if let Some(idx) = self.entries.iter().position(|e| e.name == entry.name) {
            self.entries[idx] = entry;
            true
        } else {
            false
        }
    }

    /// Remove the entry at `index`. Out-of-range is a no-op.
    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }

    /// Does any entry have this name?
    pub fn has_name(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Does any entry have this source? Used by Recent's dedupe filter.
    pub fn has_source(&self, source: &str) -> bool {
        self.entries.iter().any(|e| e.source == source)
    }

    pub fn entries(&self) -> &[BookmarkEntry] {
        &self.entries
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
impl BookmarkList {
    pub fn from_entries(entries: Vec<BookmarkEntry>) -> Self {
        Self {
            entries,
            path: None,
        }
    }
}

/// Read and parse the bookmark file. Returns an empty list if the file is
/// missing, empty, or unparseable (parse/read failures are logged, not
/// propagated — the Home Screen must still work).
fn read_entries(path: &Path) -> Vec<BookmarkEntry> {
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return Vec::new();
            }
            match serde_json::from_str::<Vec<BookmarkEntry>>(&content) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!("Failed to parse bookmarks.json at {}: {}", path.display(), e);
                    Vec::new()
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read bookmarks.json at {}: {}", path.display(), e);
            Vec::new()
        }
    }
}

fn bookmarks_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "pluq")?;
    Some(dirs.config_dir().join("bookmarks.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(name: &str, source: &str) -> BookmarkEntry {
        BookmarkEntry {
            name: name.to_string(),
            source: source.to_string(),
            provider: Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        }
    }

    #[test]
    fn test_add_new_entry() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s_a"));
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries()[0].name, "a");

        list.add(make_entry("b", "s_b"));
        assert_eq!(list.len(), 2);
        assert_eq!(list.entries()[0].name, "b");
        assert_eq!(list.entries()[1].name, "a");
    }

    #[test]
    fn test_add_caps_at_max() {
        let mut list = BookmarkList::default();
        for i in 0..MAX_BOOKMARKS + 5 {
            list.add(make_entry(&format!("n{}", i), &format!("s{}", i)));
        }
        assert_eq!(list.len(), MAX_BOOKMARKS);
        assert_eq!(list.entries()[0].name, format!("n{}", MAX_BOOKMARKS + 4));
        assert_eq!(list.entries()[MAX_BOOKMARKS - 1].name, "n5");
    }

    #[test]
    fn test_rename_ok() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s_a"));
        list.rename(0, "new".to_string()).unwrap();
        assert_eq!(list.entries()[0].name, "new");
    }

    #[test]
    fn test_rename_collision() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s_a"));
        list.add(make_entry("b", "s_b"));
        assert!(list.rename(0, "a".to_string()).is_err());
        assert_eq!(list.entries()[0].name, "b");
    }

    #[test]
    fn test_rename_out_of_range() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s_a"));
        assert!(list.rename(5, "new".to_string()).is_ok());
        assert_eq!(list.entries()[0].name, "a");
    }

    #[test]
    fn test_overwrite_by_name_hit() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s1"));
        list.add(make_entry("b", "s2"));
        let entry = make_entry("a", "s3");
        assert!(list.overwrite_by_name(entry.clone()));
        assert_eq!(list.entries()[1].source, "s3");
        assert_eq!(list.entries()[0].name, "b"); // Order preserved
    }

    #[test]
    fn test_overwrite_by_name_miss() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s1"));
        let entry = make_entry("xxx", "s2");
        assert!(!list.overwrite_by_name(entry));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_has_name() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s1"));
        assert!(list.has_name("a"));
        assert!(!list.has_name("b"));
    }

    #[test]
    fn test_has_source() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s1"));
        assert!(list.has_source("s1"));
        assert!(!list.has_source("s2"));
    }

    #[test]
    fn test_remove() {
        let mut list = BookmarkList::default();
        list.add(make_entry("a", "s_a"));
        list.add(make_entry("b", "s_b"));

        list.remove(5); // no-op
        assert_eq!(list.len(), 2);

        list.remove(0);
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries()[0].name, "a");
    }

    #[test]
    fn test_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookmarks.json");

        let mut list = BookmarkList::default();
        let e1 = make_entry("a", "s_a");
        list.add(e1.clone());

        list.save_to(&path).unwrap();

        let loaded = BookmarkList::load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.entries()[0].source, e1.source);
        assert_eq!(loaded.entries()[0].name, e1.name);
    }

    #[test]
    fn test_load_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let list = BookmarkList::load_from(&path);
        assert!(list.is_empty());
    }

    #[test]
    fn test_load_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "").unwrap();
        let list = BookmarkList::load_from(&path);
        assert!(list.is_empty());
    }

    #[test]
    fn test_load_corrupt_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, "{ invalid json }").unwrap();
        let list = BookmarkList::load_from(&path);
        assert!(list.is_empty());
    }
}
