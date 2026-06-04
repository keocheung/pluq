use std::sync::Arc;

use archive_auth::{AuthError, TokenStore};
use clap::{Subcommand, ValueEnum};

/// Subcommands for `pluq auth`.
#[derive(Subcommand)]
pub enum AuthAction {
    /// Run the OAuth browser flow and store the resulting token.
    #[command(arg_required_else_help = true)]
    Add {
        /// Cloud provider to authenticate: `gdrive` (Google Drive) or `dropbox`.
        #[arg(value_enum)]
        provider: AuthProvider,
        /// Seconds to wait for the browser callback (default 300).
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Remove a stored token.
    #[command(arg_required_else_help = true)]
    Remove {
        /// Cloud provider whose token to remove: `gdrive` or `dropbox`.
        #[arg(value_enum)]
        provider: AuthProvider,
    },
    /// List providers with stored tokens (no secrets shown).
    List,
}

#[derive(Debug, Copy, Clone, PartialEq, ValueEnum)]
pub enum AuthProvider {
    /// Google Drive
    Gdrive,
    /// Dropbox
    Dropbox,
}

impl AuthProvider {
    fn store_key(&self) -> &'static str {
        match self {
            Self::Gdrive => "gdrive",
            Self::Dropbox => "dropbox",
        }
    }
    fn display_name(&self) -> &'static str {
        match self {
            Self::Gdrive => "Google Drive",
            Self::Dropbox => "Dropbox",
        }
    }
}

pub async fn run_auth(action: AuthAction) -> Result<(), Box<dyn std::error::Error>> {
    let store = match TokenStore::load_default() {
        Ok(s) => Arc::new(s),
        Err(e) => return Err(describe_store_load_error(&e).into()),
    };

    match action {
        AuthAction::Add { provider, timeout } => run_add(provider, timeout, store).await,
        AuthAction::Remove { provider } => run_remove(provider, store),
        AuthAction::List => run_list(store),
    }
}

/// Build a user-facing message for a token-store load failure.
///
/// `TokenStore::load_default()` can only fail with `NoConfigDir` (path could
/// not be resolved), `Io` (file unreadable), or `Json` (file unparseable).
fn describe_store_load_error(err: &AuthError) -> String {
    match err {
        AuthError::NoConfigDir => {
            "OAuth token store unavailable — no standard config directory on this platform."
                .to_string()
        }
        AuthError::Json(e) => {
            format!(
                "OAuth token store at {} is corrupt: {e}\n  \
                 Recover by deleting that file or restoring a backup.",
                store_path_for_message(),
            )
        }
        AuthError::Io(e) => {
            format!(
                "Cannot read OAuth token store at {}: {e}",
                store_path_for_message(),
            )
        }
        // Other variants cannot arise from load_default(); fall back to Display.
        other => other.to_string(),
    }
}

/// Resolved token-store path for error messages, or a generic phrase if the
/// path itself cannot be resolved.
fn store_path_for_message() -> String {
    TokenStore::default_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "the token store".to_string())
}

async fn run_add(
    provider: AuthProvider,
    timeout: u64,
    store: Arc<TokenStore>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "  Opening browser to authenticate with {}...",
        provider.display_name()
    );
    eprintln!("  (If no browser opens, copy the URL printed below into your browser.)");
    let token = match provider {
        AuthProvider::Gdrive => archive_gdrive::oauth::authenticate(timeout).await?,
        AuthProvider::Dropbox => archive_dropbox::oauth::authenticate(timeout).await?,
    };
    store.save(provider.store_key(), token)?;
    if let Ok(path) = TokenStore::default_path() {
        eprintln!(
            "  {} token saved to {}",
            provider.display_name(),
            path.display()
        );
    } else {
        eprintln!("  {} token saved.", provider.display_name());
    }
    Ok(())
}

fn run_remove(
    provider: AuthProvider,
    store: Arc<TokenStore>,
) -> Result<(), Box<dyn std::error::Error>> {
    let had = store.get(provider.store_key()).is_some();
    store.remove(provider.store_key())?;
    if had {
        eprintln!("  Removed {} token.", provider.display_name());
    } else {
        eprintln!("  No {} token was stored.", provider.display_name());
    }
    Ok(())
}

fn run_list(store: Arc<TokenStore>) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(path) = TokenStore::default_path() {
        println!("  Token store: {}", path.display());
    }
    let mut providers = store.list();
    providers.sort();
    if providers.is_empty() {
        println!("  No OAuth tokens stored.");
        return Ok(());
    }
    println!();
    println!(
        "  {:<10} {:<12} {:<15}",
        "PROVIDER", "STATUS", "REFRESH TOKEN"
    );
    println!(
        "  {:<10} {:<12} {:<15}",
        "--------", "------", "-------------"
    );
    for name in providers {
        if let Some(token) = store.get(&name) {
            let status = if token.is_expired(0) {
                "expired"
            } else if token.expires_at.is_some() {
                "active"
            } else {
                "no expiry"
            };
            let has_refresh = token.refresh_token.is_some();
            println!(
                "  {:<10} {:<12} {:<15}",
                name,
                status,
                if has_refresh { "yes" } else { "no" }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_auth::{StoredToken, TokenStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, Arc<TokenStore>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(TokenStore::load_from(dir.path().join("tokens.json")).unwrap());
        (dir, store)
    }

    fn dummy_token(access: &str, refresh: Option<&str>, expires_at: Option<u64>) -> StoredToken {
        StoredToken {
            access_token: access.into(),
            refresh_token: refresh.map(str::to_string),
            expires_at,
            scopes: vec![],
        }
    }

    #[test]
    fn remove_absent_provider_is_noop() {
        let (_dir, store) = fresh_store();
        // Should not error on empty store.
        run_remove(AuthProvider::Gdrive, store).unwrap();
    }

    #[test]
    fn remove_present_provider_clears_entry() {
        let (_dir, store) = fresh_store();
        store
            .save(
                "gdrive",
                dummy_token("access_xyz", Some("refresh_abc"), None),
            )
            .unwrap();
        run_remove(AuthProvider::Gdrive, store.clone()).unwrap();
        assert!(store.get("gdrive").is_none());
    }

    #[test]
    fn list_empty_store_succeeds() {
        let (_dir, store) = fresh_store();
        run_list(store).unwrap();
    }

    #[test]
    fn list_with_entries_succeeds() {
        let (_dir, store) = fresh_store();
        store
            .save(
                "gdrive",
                dummy_token("access_xyz", Some("refresh_abc"), None),
            )
            .unwrap();
        store
            .save("dropbox", dummy_token("access_qqq", None, Some(1)))
            .unwrap();
        run_list(store).unwrap();
    }

    #[test]
    fn provider_store_keys_are_stable() {
        // Regression guard: these keys must match TokenStore::save callers in providers.
        assert_eq!(AuthProvider::Gdrive.store_key(), "gdrive");
        assert_eq!(AuthProvider::Dropbox.store_key(), "dropbox");
    }

    #[test]
    fn corrupt_store_error_names_file_and_recovery() {
        let json_err: AuthError =
            serde_json::from_str::<std::collections::HashMap<String, String>>("not json")
                .unwrap_err()
                .into();
        let msg = describe_store_load_error(&json_err);
        assert!(msg.contains("corrupt"), "msg: {msg}");
        assert!(msg.to_lowercase().contains("recover"), "msg: {msg}");
        assert!(
            !msg.contains("no standard config directory"),
            "must not reuse the NoConfigDir message — msg: {msg}"
        );
    }

    #[test]
    fn no_config_dir_error_keeps_message() {
        let msg = describe_store_load_error(&AuthError::NoConfigDir);
        assert!(msg.contains("no standard config directory"), "msg: {msg}");
    }

    #[test]
    fn io_store_error_names_file() {
        let io_err: AuthError = std::io::Error::other("disk gone").into();
        let msg = describe_store_load_error(&io_err);
        assert!(msg.contains("Cannot read"), "msg: {msg}");
        assert!(msg.contains("disk gone"), "msg: {msg}");
    }
}
