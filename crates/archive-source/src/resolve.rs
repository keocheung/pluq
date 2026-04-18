use std::sync::Arc;
use archive_core::{CachingReadAt, HttpReadAt, LocalReadAt};
use archive_dropbox::{self, DropboxAuth, DropboxReadAt};
use archive_gdrive::{self, GDriveAuth, GoogleDriveReadAt};
use tracing::{info, warn};

use crate::config::SourceConfig;
use crate::error::SourceError;
use crate::resolved::ResolvedSource;

/// Resolve user input into a fully-constructed reader.
///
/// Detects the source type (local path, cloud provider URL, or generic HTTP),
/// looks up credentials, and constructs the appropriate `ReadAt` implementation.
///
/// Returns `Err(SourceError)` if a provider was detected but setup failed
/// (e.g., missing API key, unparseable URL).
pub fn resolve(input: &str, config: &SourceConfig) -> Result<ResolvedSource, SourceError> {
    // ── Google Drive? ────────────────────────────────────────────────────
    if archive_gdrive::is_gdrive_url(input) {
        info!(url = %input, "Google Drive URL detected");

        let file_id =
            archive_gdrive::extract_file_id(input).ok_or_else(|| SourceError::InvalidUrl {
                url: input.to_string(),
                reason: "could not extract Google Drive file ID".into(),
            })?;

        let auth = resolve_gdrive_auth(config)?;
        let reader = CachingReadAt::new(GoogleDriveReadAt::new(file_id.clone(), auth));
        return Ok(ResolvedSource::GDrive(reader, input.to_string(), file_id));
    }

    // ── Dropbox? ─────────────────────────────────────────────────────────
    if archive_dropbox::is_dropbox_url(input) {
        info!(url = %input, "Dropbox URL detected");
        let auth = resolve_dropbox_auth(config)?;
        let reader = CachingReadAt::new(DropboxReadAt::new(input.to_string(), auth));
        return Ok(ResolvedSource::Dropbox(reader, input.to_string()));
    }

    // ── Generic HTTP? ────────────────────────────────────────────────────
    if input.starts_with("http://") || input.starts_with("https://") {
        info!(url = %input, "generic HTTP URL detected");
        let client = reqwest::Client::builder()
            .user_agent("archive-sniper")
            .build()
            .expect("failed to create HTTP client");
        let reader = CachingReadAt::new(HttpReadAt::new(input, client));
        return Ok(ResolvedSource::Http(reader, input.to_string()));
    }

    // ── Local file ───────────────────────────────────────────────────────
    info!(path = %input, "local file path detected");
    Ok(ResolvedSource::Local(
        LocalReadAt::new(input),
        input.to_string(),
    ))
}

pub(crate) fn resolve_gdrive_auth(config: &SourceConfig) -> Result<GDriveAuth, SourceError> {
    if config.token_store.as_ref().and_then(|s| s.get("gdrive")).is_some()
        && archive_gdrive::oauth_client_id().is_none()
    {
        warn!("Google Drive: OAuth token found but no client ID configured — falling back to API key. Refresh will not work.");
    }

    // Prefer OAuth when both a stored token and a client ID exist.
    if let Some(store) = &config.token_store
        && let Some(token) = store.get("gdrive")
        && let Some(client_id) = archive_gdrive::oauth_client_id()
    {
        let client_secret = archive_gdrive::oauth_client_secret();
        info!("Google Drive: using OAuth credentials");
        return Ok(GDriveAuth::oauth(token, Arc::clone(store), client_id, client_secret));
    }

    // Fallback to API key.
    if let Some(key) = archive_gdrive::api_key() {
        info!("Google Drive: using API key (no OAuth token found)");
        return Ok(GDriveAuth::ApiKey(key));
    }

    Err(SourceError::NoCredentials {
        provider: "Google Drive",
        guidance: "Set ARCHIVE_SNIPER_GDRIVE_KEY, or authenticate via `snipe auth add gdrive`."
            .into(),
    })
}

pub(crate) fn resolve_dropbox_auth(config: &SourceConfig) -> Result<DropboxAuth, SourceError> {
    if config.token_store.as_ref().and_then(|s| s.get("dropbox")).is_some()
        && archive_dropbox::oauth_client_id().is_none()
    {
        warn!("Dropbox: OAuth token found but no client ID configured — falling back to app credentials. Refresh will not work.");
    }

    if let Some(store) = &config.token_store
        && let Some(token) = store.get("dropbox")
        && let Some(client_id) = archive_dropbox::oauth_client_id()
    {
        let client_secret = archive_dropbox::oauth_client_secret();
        info!("Dropbox: using OAuth credentials");
        return Ok(DropboxAuth::oauth(token, Arc::clone(store), client_id, client_secret));
    }

    if let Some((key, secret)) = archive_dropbox::app_credentials() {
        info!("Dropbox: using app credentials (no OAuth token found)");
        return Ok(DropboxAuth::app_credentials(key, secret));
    }

    Err(SourceError::NoCredentials {
        provider: "Dropbox",
        guidance: "Set ARCHIVE_SNIPER_DROPBOX_APP_KEY + ARCHIVE_SNIPER_DROPBOX_APP_SECRET, or authenticate via `snipe auth add dropbox`."
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // Use a global mutex to serialize tests that mutate the environment.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn dropbox_url_without_app_credentials_returns_no_credentials() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
        }
        let result = resolve(
            "https://www.dropbox.com/s/abc/x.zip?dl=0",
            &SourceConfig::default(),
        );
        assert!(matches!(
            result,
            Err(SourceError::NoCredentials {
                provider: "Dropbox",
                ..
            })
        ));
    }

    #[test]
    fn dropbox_url_with_app_credentials_returns_dropbox_variant() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET", "test-secret");
        }
        let url = "https://www.dropbox.com/s/abc/x.zip?dl=0";
        let result = resolve(url, &SourceConfig::default());

        match result {
            Ok(ResolvedSource::Dropbox(_, r_url)) => assert_eq!(r_url, url),
            _ => panic!("expected Dropbox variant"),
        }

        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn dropbox_url_with_only_key_returns_no_credentials() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY", "test-key");
        }
        let result = resolve(
            "https://www.dropbox.com/s/abc/x.zip?dl=0",
            &SourceConfig::default(),
        );
        assert!(matches!(
            result,
            Err(SourceError::NoCredentials {
                provider: "Dropbox",
                ..
            })
        ));
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
        }
    }

    #[test]
    fn gdrive_url_with_oauth_token_uses_oauth_variant() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store.save("gdrive", archive_auth::StoredToken {
            access_token: "test_access".into(),
            refresh_token: Some("test_refresh".into()),
            expires_at: Some(1234567890),
            scopes: vec![],
        }).unwrap();
        
        let config = SourceConfig::new().with_token_store(Arc::new(store));
        
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID", "test-client-id");
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_KEY");
        }
        
        let auth = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::OAuth { .. }));
        
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID");
        }
    }

    #[test]
    fn gdrive_url_without_oauth_token_falls_back_to_apikey() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_GDRIVE_KEY", "test-key");
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID");
        }
        let auth = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::ApiKey(k) if k == "test-key"));
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_KEY");
        }
    }

    #[test]
    fn gdrive_url_oauth_token_present_but_no_client_id_falls_back() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store.save("gdrive", archive_auth::StoredToken {
            access_token: "test_access".into(),
            refresh_token: Some("test_refresh".into()),
            expires_at: Some(1234567890),
            scopes: vec![],
        }).unwrap();
        
        let config = SourceConfig::new().with_token_store(Arc::new(store));
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_GDRIVE_KEY", "test-key");
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID");
        }
        
        let auth = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::ApiKey(k) if k == "test-key"));
        
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_KEY");
        }
    }

    #[test]
    fn dropbox_url_with_oauth_token_uses_oauth_variant() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store.save("dropbox", archive_auth::StoredToken {
            access_token: "test_access".into(),
            refresh_token: Some("test_refresh".into()),
            expires_at: Some(1234567890),
            scopes: vec![],
        }).unwrap();
        
        let config = SourceConfig::new().with_token_store(Arc::new(store));
        
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID", "test-client-id");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
        }
        
        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::OAuth { .. }));
        
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID");
        }
    }

    #[test]
    fn dropbox_url_without_oauth_token_falls_back_to_app_credentials() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET", "test-secret");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID");
        }
        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::AppCredentials { .. }));
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn dropbox_url_oauth_token_but_no_client_id_falls_back() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store.save("dropbox", archive_auth::StoredToken {
            access_token: "test_access".into(),
            refresh_token: Some("test_refresh".into()),
            expires_at: Some(1234567890),
            scopes: vec![],
        }).unwrap();
        
        let config = SourceConfig::new().with_token_store(Arc::new(store));
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET", "test-secret");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_CLIENT_ID");
        }
        
        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::AppCredentials { .. }));
        
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn gdrive_url_no_credentials_at_all_returns_no_credentials_error() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_KEY");
            std::env::remove_var("ARCHIVE_SNIPER_GDRIVE_CLIENT_ID");
        }
        
        let auth = resolve_gdrive_auth(&config);
        assert!(matches!(
            auth,
            Err(SourceError::NoCredentials { provider: "Google Drive", .. })
        ));
    }
}
