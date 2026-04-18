use std::sync::Arc;

use archive_auth::{ProviderOAuthConfig, StoredToken, TokenStore};
use tokio::sync::RwLock;

/// How a `GoogleDriveReadAt` authenticates requests.
pub enum GDriveAuth {
    /// Phase 4 — legacy API key in query string.
    ApiKey(String),
    /// Phase 4.5 — OAuth Bearer. Refreshed on 401, persisted to store.
    OAuth {
        token: Arc<RwLock<StoredToken>>,
        store: Arc<TokenStore>,
        client_id: String,
        client_secret: Option<String>,
        /// OAuth endpoint config. Production uses `&archive_auth::GDRIVE_CONFIG`;
        /// tests supply a mock-pointing config to exercise the real refresh path.
        config: &'static ProviderOAuthConfig,
    },
}

impl GDriveAuth {
    /// Convenience constructor for production — uses `&archive_auth::GDRIVE_CONFIG`.
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
            &archive_auth::GDRIVE_CONFIG,
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

/// Load the Google Drive API key.
///
/// Priority: env var `ARCHIVE_SNIPER_GDRIVE_KEY` > embedded default.
/// Returns `None` if no key is available.
pub fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ARCHIVE_SNIPER_GDRIVE_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    option_env!("ARCHIVE_SNIPER_GDRIVE_KEY_EMBED").map(|s| s.to_string())
}

/// OAuth installed-app client ID.
///
/// Priority: env var `ARCHIVE_SNIPER_GDRIVE_CLIENT_ID` > embedded default.
pub fn oauth_client_id() -> Option<String> {
    if let Ok(v) = std::env::var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID")
        && !v.is_empty()
    {
        return Some(v);
    }
    option_env!("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID_EMBED").map(String::from)
}

/// OAuth installed-app client secret.
///
/// Google requires this even for loopback + PKCE flows.
/// Priority: env var `ARCHIVE_SNIPER_GDRIVE_CLIENT_SECRET` > embedded default.
pub fn oauth_client_secret() -> Option<String> {
    if let Ok(v) = std::env::var("ARCHIVE_SNIPER_GDRIVE_CLIENT_SECRET")
        && !v.is_empty()
    {
        return Some(v);
    }
    option_env!("ARCHIVE_SNIPER_GDRIVE_CLIENT_SECRET_EMBED").map(String::from)
}
