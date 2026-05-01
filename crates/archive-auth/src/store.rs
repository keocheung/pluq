use crate::error::AuthError;
use directories::ProjectDirs;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    /// Some providers may not return one on first exchange.
    pub refresh_token: Option<String>,
    /// Unix seconds since epoch. `None` = never expires / unknown.
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}

impl StoredToken {
    /// Returns true if the token expires within the grace period.
    /// Use to decide whether to refresh proactively.
    pub fn is_expired(&self, grace_secs: u64) -> bool {
        match self.expires_at {
            None => false,
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                now + grace_secs >= exp
            }
        }
    }
}

pub struct TokenStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, StoredToken>>,
}

impl TokenStore {
    /// Load tokens from the default location: `~/.config/pluq/tokens.json`.
    /// Missing file is NOT an error — returns an empty store.
    pub fn load_default() -> Result<Self, AuthError> {
        Self::load_from(Self::default_path()?)
    }

    /// Load tokens from an explicit path (used by tests).
    pub fn load_from(path: PathBuf) -> Result<Self, AuthError> {
        let inner = if path.exists() {
            let content = fs::read_to_string(&path)?;
            serde_json::from_str(&content)?
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// Clone the token for `provider` if present.
    pub fn get(&self, provider: &str) -> Option<StoredToken> {
        let inner = self.inner.lock().unwrap();
        inner.get(provider).cloned()
    }

    /// Persist `token` for `provider`, atomically.
    /// On Unix, chmod 0600 the file after write.
    pub fn save(&self, provider: &str, token: StoredToken) -> Result<(), AuthError> {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            inner.insert(provider.to_string(), token);
            inner.clone()
        };
        self.persist(&snapshot)
    }

    /// Remove token for `provider`. No-op if absent.
    pub fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            if inner.remove(provider).is_some() {
                Some(inner.clone())
            } else {
                None
            }
        };

        if let Some(snapshot) = snapshot {
            self.persist(&snapshot)?;
        }
        Ok(())
    }

    /// List provider names with stored tokens. Does NOT return token contents.
    pub fn list(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner.keys().cloned().collect()
    }

    /// Resolve the default path. Public for CLI `pluq auth list` diagnostics.
    pub fn default_path() -> Result<PathBuf, AuthError> {
        ProjectDirs::from("com", "pluq", "pluq")
            .map(|dirs| dirs.config_dir().join("tokens.json"))
            .ok_or(AuthError::NoConfigDir)
    }

    fn persist(&self, inner: &HashMap<String, StoredToken>) -> Result<(), AuthError> {
        let json = serde_json::to_string_pretty(inner)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("no parent directory for token store"))?;

        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }

        let pid = std::process::id();
        let tmp_path = self.path.with_extension(format!("tmp.{}", pid));

        let write_result = (|| -> Result<(), AuthError> {
            let mut tmp_file = fs::File::create(&tmp_path)?;
            tmp_file.write_all(json.as_bytes())?;
            tmp_file.sync_all()?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_path);
            return write_result;
        }

        fs::rename(&tmp_path, &self.path)?;

        #[cfg(unix)]
        {
            // Belt-and-suspenders: fsync parent directory for POSIX durability
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_from_missing_file_returns_empty_store() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn save_then_get_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path).unwrap();
        let token = StoredToken {
            access_token: "test_access".to_string(),
            refresh_token: Some("test_refresh".to_string()),
            expires_at: Some(1234567890),
            scopes: vec!["scope1".to_string()],
        };
        store.save("test", token.clone()).unwrap();

        let retrieved = store.get("test").unwrap();
        assert_eq!(retrieved.access_token, token.access_token);
        assert_eq!(retrieved.refresh_token, token.refresh_token);
        assert_eq!(retrieved.expires_at, token.expires_at);
        assert_eq!(retrieved.scopes, token.scopes);
    }

    #[test]
    fn save_overwrites_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path).unwrap();
        let token1 = StoredToken {
            access_token: "token1".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        let token2 = StoredToken {
            access_token: "token2".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        store.save("test", token1).unwrap();
        store.save("test", token2).unwrap();

        assert_eq!(store.get("test").unwrap().access_token, "token2");
    }

    #[test]
    fn remove_clears_entry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path).unwrap();
        let token = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        store.save("test", token).unwrap();
        assert!(store.get("test").is_some());

        store.remove("test").unwrap();
        assert!(store.get("test").is_none());
    }

    #[test]
    fn list_returns_provider_names_only() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path).unwrap();
        let token = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        store.save("gdrive", token.clone()).unwrap();
        store.save("dropbox", token).unwrap();

        let mut list = store.list();
        list.sort();
        assert_eq!(list, vec!["dropbox", "gdrive"]);
    }

    #[test]
    fn store_survives_reload() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let token = StoredToken {
            access_token: "persistent".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };

        {
            let store = TokenStore::load_from(path.clone()).unwrap();
            store.save("test", token).unwrap();
        }

        let store = TokenStore::load_from(path).unwrap();
        assert_eq!(store.get("test").unwrap().access_token, "persistent");
    }

    #[test]
    fn is_expired_logic() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let token = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: Some(now + 100),
            scopes: vec![],
        };

        assert!(!token.is_expired(0));
        assert!(!token.is_expired(50));
        assert!(token.is_expired(110));

        let permanent = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        assert!(!permanent.is_expired(1000));
    }

    #[test]
    #[cfg(unix)]
    fn save_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("tokens.json");
        let store = TokenStore::load_from(path.clone()).unwrap();
        let token = StoredToken {
            access_token: "test".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec![],
        };
        store.save("test", token).unwrap();

        let metadata = fs::metadata(path).unwrap();
        let mode = metadata.permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
