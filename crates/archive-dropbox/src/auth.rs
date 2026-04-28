use std::sync::Arc;

use archive_auth::{ProviderOAuthConfig, StoredToken, TokenStore};
use tokio::sync::RwLock;

/// How a `DropboxReadAt` authenticates requests.
pub enum DropboxAuth {
    /// Phase 4 — App Basic Auth (`Authorization: Basic base64(key:secret)`).
    /// Used for public shared links. No refresh cycle.
    AppCredentials { app_key: String, app_secret: String },
    /// Phase 4.5 — OAuth Bearer. Refreshed on 401, persisted to store.
    /// Used for private shared links (requires user consent).
    OAuth {
        token: Arc<RwLock<StoredToken>>,
        store: Arc<TokenStore>,
        client_id: String,
        client_secret: Option<String>,
        /// OAuth endpoint config. Production uses `&archive_auth::DROPBOX_CONFIG`;
        /// tests supply a mock-pointing config.
        config: &'static ProviderOAuthConfig,
    },
}

impl DropboxAuth {
    /// Convenience constructor for App Basic Auth.
    pub fn app_credentials(app_key: String, app_secret: String) -> Self {
        Self::AppCredentials {
            app_key,
            app_secret,
        }
    }

    /// Convenience constructor for OAuth — uses the production `DROPBOX_CONFIG`.
    pub fn oauth(
        token: StoredToken,
        store: Arc<TokenStore>,
        client_id: String,
        client_secret: Option<String>,
    ) -> Self {
        Self::oauth_with_config(
            token,
            store,
            client_id,
            client_secret,
            &archive_auth::DROPBOX_CONFIG,
        )
    }

    /// Constructor with an explicit config pointer — used by tests to inject a
    /// mock token endpoint, and by callers that need a custom provider config.
    pub fn oauth_with_config(
        token: StoredToken,
        store: Arc<TokenStore>,
        client_id: String,
        client_secret: Option<String>,
        config: &'static ProviderOAuthConfig,
    ) -> Self {
        Self::OAuth {
            token: Arc::new(RwLock::new(token)),
            store,
            client_id,
            client_secret,
            config,
        }
    }
}

/// Load the Dropbox app credentials (key + secret) for App Authentication.
///
/// Priority: env vars `ARCHIVE_SNIPER_DROPBOX_APP_KEY` + `ARCHIVE_SNIPER_DROPBOX_APP_SECRET`,
/// falling back to embedded defaults (`*_EMBED` compile-time vars).
/// Returns `None` if either component is missing.
pub fn app_credentials() -> Option<(String, String)> {
    let key = std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("ARCHIVE_SNIPER_DROPBOX_APP_KEY_EMBED").map(String::from))?;
    let secret = std::env::var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| option_env!("ARCHIVE_SNIPER_DROPBOX_APP_SECRET_EMBED").map(String::from))?;
    Some((key, secret))
}

/// OAuth installed-app client ID.
///
/// Priority: env var `ARCHIVE_SNIPER_DROPBOX_CLIENT_ID` > embedded default.
pub fn oauth_client_id() -> Option<String> {
    if let Ok(v) = std::env::var("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID")
        && !v.is_empty()
    {
        return Some(v);
    }
    option_env!("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID_EMBED").map(String::from)
}

/// OAuth installed-app client secret.
///
/// `None` = public client (PKCE only, no secret).
/// Dropbox supports both confidential and public installed apps.
/// Priority: env var `ARCHIVE_SNIPER_DROPBOX_CLIENT_SECRET` > embedded default.
pub fn oauth_client_secret() -> Option<String> {
    if let Ok(v) = std::env::var("ARCHIVE_SNIPER_DROPBOX_CLIENT_SECRET")
        && !v.is_empty()
    {
        return Some(v);
    }
    option_env!("ARCHIVE_SNIPER_DROPBOX_CLIENT_SECRET_EMBED").map(String::from)
}
