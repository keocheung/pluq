use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::provider::Provider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentEntry {
    /// Display name. For MVP, equal to `archive_source::suggest_name_from_str(&source)`
    /// or the raw filename, whichever is clearer. Falls back to the full source.
    pub name: String,
    /// The raw source string (URL or path) — the dedupe key.
    pub source: String,
    pub provider: Provider,
    pub size: u64,
    pub last_opened: DateTime<Utc>,
}

#[derive(Default)]
pub struct RecentList {
    entries: Vec<RecentEntry>,
}

const MAX_RECENT: usize = 10;

impl RecentList {
    /// Read `~/.config/archive-sniper/recent.json`. Returns an empty list if the
    /// file does not exist, is empty, or fails to parse (log a warning on parse
    /// failure, never propagate).
    pub fn load() -> Self {
        if let Some(path) = recent_path() {
            Self::load_from(&path)
        } else {
            Self {
                entries: Vec::new(),
            }
        }
    }

    /// Internal load helper for testing and main load.
    fn load_from(path: &Path) -> Self {
        if !path.exists() {
            return Self {
                entries: Vec::new(),
            };
        }

        match std::fs::read_to_string(path) {
            Ok(content) => {
                if content.trim().is_empty() {
                    return Self {
                        entries: Vec::new(),
                    };
                }
                match serde_json::from_str::<Vec<RecentEntry>>(&content) {
                    Ok(entries) => Self { entries },
                    Err(e) => {
                        tracing::warn!("Failed to parse recent.json at {}: {}", path.display(), e);
                        Self {
                            entries: Vec::new(),
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to read recent.json at {}: {}", path.display(), e);
                Self {
                    entries: Vec::new(),
                }
            }
        }
    }

    /// Write atomically via tempfile + rename. Creates the parent directory
    /// with `fs::create_dir_all` if missing. Errors are logged and swallowed
    /// — the Home Screen must still function if the disk is read-only.
    pub fn save(&self) {
        if let Some(path) = recent_path()
            && let Err(e) = self.save_to(&path)
        {
            tracing::error!("Failed to save recent.json to {}: {}", path.display(), e);
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

    /// Insert or promote an entry to the top. Dedup key = `source`. Caps the
    /// list to `MAX_RECENT` by dropping the tail.
    pub fn push(&mut self, entry: RecentEntry) {
        // Remove existing if present (dedupe by source)
        if let Some(idx) = self.entries.iter().position(|e| e.source == entry.source) {
            self.entries.remove(idx);
        }

        // Insert at top
        self.entries.insert(0, entry);

        // Cap list
        if self.entries.len() > MAX_RECENT {
            self.entries.truncate(MAX_RECENT);
        }
    }

    /// Remove the entry at `index`. Out-of-range is a no-op.
    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }

    pub fn entries(&self) -> &[RecentEntry] {
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
impl RecentList {
    pub fn from_entries(entries: Vec<RecentEntry>) -> Self {
        Self { entries }
    }
}

fn recent_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "archive-sniper")?;
    Some(dirs.config_dir().join("recent.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(name: &str, source: &str) -> RecentEntry {
        RecentEntry {
            name: name.to_string(),
            source: source.to_string(),
            provider: Provider::Local,
            size: 100,
            last_opened: Utc::now(),
        }
    }

    #[test]
    fn test_push_new_entry() {
        let mut list = RecentList {
            entries: Vec::new(),
        };
        list.push(make_entry("a", "source_a"));
        assert_eq!(list.entries().len(), 1);
        assert_eq!(list.entries()[0].source, "source_a");

        list.push(make_entry("b", "source_b"));
        assert_eq!(list.entries().len(), 2);
        assert_eq!(list.entries()[0].source, "source_b");
        assert_eq!(list.entries()[1].source, "source_a");
    }

    #[test]
    fn test_push_promotes_existing() {
        let mut list = RecentList {
            entries: Vec::new(),
        };
        list.push(make_entry("a", "source_a"));
        list.push(make_entry("b", "source_b"));
        assert_eq!(list.entries()[0].source, "source_b");

        list.push(make_entry("a", "source_a"));
        assert_eq!(list.entries().len(), 2);
        assert_eq!(list.entries()[0].source, "source_a");
        assert_eq!(list.entries()[1].source, "source_b");
    }

    #[test]
    fn test_push_caps_at_max() {
        let mut list = RecentList {
            entries: Vec::new(),
        };
        for i in 0..MAX_RECENT + 5 {
            list.push(make_entry(&format!("n{}", i), &format!("s{}", i)));
        }
        assert_eq!(list.entries().len(), MAX_RECENT);
        assert_eq!(list.entries()[0].source, format!("s{}", MAX_RECENT + 4));
        assert_eq!(list.entries()[MAX_RECENT - 1].source, "s5");
    }

    #[test]
    fn test_remove() {
        let mut list = RecentList {
            entries: Vec::new(),
        };
        list.push(make_entry("a", "s_a"));
        list.push(make_entry("b", "s_b"));

        list.remove(5); // no-op
        assert_eq!(list.len(), 2);

        list.remove(0);
        assert_eq!(list.len(), 1);
        assert_eq!(list.entries()[0].source, "s_a");
    }

    #[test]
    fn test_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");

        let mut list = RecentList {
            entries: Vec::new(),
        };
        let e1 = make_entry("a", "s_a");
        list.push(e1.clone());

        list.save_to(&path).unwrap();

        let loaded = RecentList::load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.entries()[0].source, e1.source);
        assert_eq!(loaded.entries()[0].name, e1.name);
    }

    #[test]
    fn test_load_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let list = RecentList::load_from(&path);
        assert!(list.is_empty());
    }

    #[test]
    fn test_load_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "").unwrap();
        let list = RecentList::load_from(&path);
        assert!(list.is_empty());
    }

    #[test]
    fn test_load_corrupt_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, "{ invalid json }").unwrap();
        let list = RecentList::load_from(&path);
        assert!(list.is_empty());
    }
}
