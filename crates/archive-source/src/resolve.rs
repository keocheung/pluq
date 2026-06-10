use archive_core::{CachingReadAt, HttpReadAt, LocalReadAt};
use archive_dropbox::{self, DropboxAuth, DropboxReadAt};
use archive_gdrive::{self, GDriveAuth, GoogleDriveReadAt};
use std::sync::Arc;
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

        let (auth, fallback) = resolve_gdrive_auth(config)?;
        let reader = CachingReadAt::new(GoogleDriveReadAt::new_with_fallback(
            file_id.clone(),
            auth,
            fallback,
        ));
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
            .user_agent("pluq")
            // Bound a stalled cold connection / mid-transfer stall so a generic HTTP
            // source fails fast instead of hanging indefinitely (mirrors the cloud readers).
            .connect_timeout(std::time::Duration::from_secs(30))
            .read_timeout(std::time::Duration::from_secs(60))
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

pub(crate) fn resolve_gdrive_auth(
    config: &SourceConfig,
) -> Result<(GDriveAuth, Option<GDriveAuth>), SourceError> {
    let token_in_store = config
        .token_store
        .as_ref()
        .and_then(|s| s.get("gdrive"))
        .is_some();
    let oauth_client_id_set = archive_gdrive::oauth_client_id().is_some();
    let api_key_set = archive_gdrive::api_key().is_some();
    info!(
        token_in_store,
        oauth_client_id_set, // PLUQ_GDRIVE_CLIENT_ID (env or embedded)
        api_key_set,         // PLUQ_GDRIVE_KEY (env or embedded)
        "Google Drive credential resolution"
    );

    if token_in_store && !oauth_client_id_set {
        warn!(
            "Google Drive: stored OAuth token present but PLUQ_GDRIVE_CLIENT_ID is unset — OAuth unusable (no refresh), falling back to API key"
        );
    }

    // Prefer OAuth when both a stored token and a client ID exist — it reaches
    // picker-granted private files. Pair it with the public API key as a
    // fallback so a *pasted* public link (which `drive.file` OAuth cannot see,
    // → 404) still opens. The reader latches whichever auth works.
    if let Some(store) = &config.token_store
        && let Some(token) = store.get("gdrive")
        && let Some(client_id) = archive_gdrive::oauth_client_id()
    {
        let client_secret = archive_gdrive::oauth_client_secret();
        let fallback = archive_gdrive::api_key().map(GDriveAuth::ApiKey);
        info!(
            api_key_fallback = fallback.is_some(),
            "Google Drive: using OAuth credentials (public-link API-key fallback)"
        );
        let primary = GDriveAuth::oauth(token, Arc::clone(store), client_id, client_secret);
        return Ok((primary, fallback));
    }

    // API key only (no OAuth token in store).
    if let Some(key) = archive_gdrive::api_key() {
        info!("Google Drive: using API key (no OAuth token found)");
        return Ok((GDriveAuth::ApiKey(key), None));
    }

    Err(SourceError::NoCredentials {
        provider: "Google Drive",
        guidance: "Set PLUQ_GDRIVE_KEY, or authenticate via `pluq auth add gdrive`.".into(),
    })
}

pub(crate) fn resolve_dropbox_auth(config: &SourceConfig) -> Result<DropboxAuth, SourceError> {
    // Log what credential material is actually visible to this process — presence
    // only, never values. This is the first place to look when "creds are set" but
    // the wrong path is taken (e.g. APP_KEY/SECRET present but CLIENT_ID absent).
    let token_in_store = config
        .token_store
        .as_ref()
        .and_then(|s| s.get("dropbox"))
        .is_some();
    let oauth_client_id_set = archive_dropbox::oauth_client_id().is_some();
    let app_credentials_set = archive_dropbox::app_credentials().is_some();
    info!(
        token_in_store,
        oauth_client_id_set, // PLUQ_DROPBOX_CLIENT_ID (env or embedded)
        app_credentials_set, // PLUQ_DROPBOX_APP_KEY + PLUQ_DROPBOX_APP_SECRET (env or embedded)
        "Dropbox credential resolution"
    );

    if token_in_store && !oauth_client_id_set {
        warn!(
            "Dropbox: stored OAuth token present but PLUQ_DROPBOX_CLIENT_ID is unset — OAuth unusable (no refresh), falling back to app credentials"
        );
    }

    if let Some(store) = &config.token_store
        && let Some(token) = store.get("dropbox")
        && let Some(client_id) = archive_dropbox::oauth_client_id()
    {
        let client_secret = archive_dropbox::oauth_client_secret();
        info!("Dropbox: using OAuth credentials (token + client ID present)");
        return Ok(DropboxAuth::oauth(
            token,
            Arc::clone(store),
            client_id,
            client_secret,
        ));
    }

    if let Some((key, secret)) = archive_dropbox::app_credentials() {
        info!("Dropbox: using app credentials (APP_KEY/APP_SECRET)");
        return Ok(DropboxAuth::app_credentials(key, secret));
    }

    Err(SourceError::NoCredentials {
        provider: "Dropbox",
        guidance: "Set PLUQ_DROPBOX_APP_KEY + PLUQ_DROPBOX_APP_SECRET, or authenticate via `pluq auth add dropbox`."
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
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
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
            std::env::set_var("PLUQ_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("PLUQ_DROPBOX_APP_SECRET", "test-secret");
        }
        let url = "https://www.dropbox.com/s/abc/x.zip?dl=0";
        let result = resolve(url, &SourceConfig::default());

        match result {
            Ok(ResolvedSource::Dropbox(_, r_url)) => assert_eq!(r_url, url),
            _ => panic!("expected Dropbox variant"),
        }

        unsafe {
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn dropbox_url_with_only_key_returns_no_credentials() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // SAFETY: test mutates process env. Run single-threaded if flaky.
        unsafe {
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
            std::env::set_var("PLUQ_DROPBOX_APP_KEY", "test-key");
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
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
        }
    }

    #[test]
    fn gdrive_url_with_oauth_token_uses_oauth_variant() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store
            .save(
                "gdrive",
                archive_auth::StoredToken {
                    access_token: "test_access".into(),
                    refresh_token: Some("test_refresh".into()),
                    expires_at: Some(1234567890),
                    scopes: vec![],
                },
            )
            .unwrap();

        let config = SourceConfig::new().with_token_store(Arc::new(store));

        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_GDRIVE_CLIENT_ID", "test-client-id");
            std::env::remove_var("PLUQ_GDRIVE_KEY");
        }

        let (auth, fallback) = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::OAuth { .. }));
        assert!(fallback.is_none(), "no API key set → no fallback");

        unsafe {
            std::env::remove_var("PLUQ_GDRIVE_CLIENT_ID");
        }
    }

    #[test]
    fn gdrive_oauth_with_api_key_present_sets_apikey_fallback() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store
            .save(
                "gdrive",
                archive_auth::StoredToken {
                    access_token: "test_access".into(),
                    refresh_token: Some("test_refresh".into()),
                    expires_at: Some(1234567890),
                    scopes: vec![],
                },
            )
            .unwrap();

        let config = SourceConfig::new().with_token_store(Arc::new(store));
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_GDRIVE_CLIENT_ID", "test-client-id");
            std::env::set_var("PLUQ_GDRIVE_KEY", "test-key");
        }

        // OAuth primary (picker-granted files) + public API-key fallback (pasted
        // public links the OAuth scope can't see).
        let (auth, fallback) = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::OAuth { .. }));
        assert!(matches!(fallback, Some(GDriveAuth::ApiKey(k)) if k == "test-key"));

        unsafe {
            std::env::remove_var("PLUQ_GDRIVE_CLIENT_ID");
            std::env::remove_var("PLUQ_GDRIVE_KEY");
        }
    }

    #[test]
    fn gdrive_url_without_oauth_token_falls_back_to_apikey() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_GDRIVE_KEY", "test-key");
            std::env::remove_var("PLUQ_GDRIVE_CLIENT_ID");
        }
        let (auth, fallback) = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::ApiKey(k) if k == "test-key"));
        assert!(fallback.is_none(), "API-key-only path has no fallback");
        unsafe {
            std::env::remove_var("PLUQ_GDRIVE_KEY");
        }
    }

    #[test]
    fn gdrive_url_oauth_token_present_but_no_client_id_falls_back() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store
            .save(
                "gdrive",
                archive_auth::StoredToken {
                    access_token: "test_access".into(),
                    refresh_token: Some("test_refresh".into()),
                    expires_at: Some(1234567890),
                    scopes: vec![],
                },
            )
            .unwrap();

        let config = SourceConfig::new().with_token_store(Arc::new(store));
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_GDRIVE_KEY", "test-key");
            std::env::remove_var("PLUQ_GDRIVE_CLIENT_ID");
        }

        let (auth, fallback) = resolve_gdrive_auth(&config).unwrap();
        assert!(matches!(auth, GDriveAuth::ApiKey(k) if k == "test-key"));
        assert!(fallback.is_none(), "API-key-only path has no fallback");

        unsafe {
            std::env::remove_var("PLUQ_GDRIVE_KEY");
        }
    }

    #[test]
    fn dropbox_url_with_oauth_token_uses_oauth_variant() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store
            .save(
                "dropbox",
                archive_auth::StoredToken {
                    access_token: "test_access".into(),
                    refresh_token: Some("test_refresh".into()),
                    expires_at: Some(1234567890),
                    scopes: vec![],
                },
            )
            .unwrap();

        let config = SourceConfig::new().with_token_store(Arc::new(store));

        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_DROPBOX_CLIENT_ID", "test-client-id");
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
        }

        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::OAuth { .. }));

        unsafe {
            std::env::remove_var("PLUQ_DROPBOX_CLIENT_ID");
        }
    }

    #[test]
    fn dropbox_url_without_oauth_token_falls_back_to_app_credentials() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("PLUQ_DROPBOX_APP_SECRET", "test-secret");
            std::env::remove_var("PLUQ_DROPBOX_CLIENT_ID");
        }
        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::AppCredentials { .. }));
        unsafe {
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn dropbox_url_oauth_token_but_no_client_id_falls_back() {
        let dir = tempdir().unwrap();
        let store = archive_auth::TokenStore::load_from(dir.path().join("tokens.json")).unwrap();
        store
            .save(
                "dropbox",
                archive_auth::StoredToken {
                    access_token: "test_access".into(),
                    refresh_token: Some("test_refresh".into()),
                    expires_at: Some(1234567890),
                    scopes: vec![],
                },
            )
            .unwrap();

        let config = SourceConfig::new().with_token_store(Arc::new(store));
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var("PLUQ_DROPBOX_APP_KEY", "test-key");
            std::env::set_var("PLUQ_DROPBOX_APP_SECRET", "test-secret");
            std::env::remove_var("PLUQ_DROPBOX_CLIENT_ID");
        }

        let auth = resolve_dropbox_auth(&config).unwrap();
        assert!(matches!(auth, DropboxAuth::AppCredentials { .. }));

        unsafe {
            std::env::remove_var("PLUQ_DROPBOX_APP_KEY");
            std::env::remove_var("PLUQ_DROPBOX_APP_SECRET");
        }
    }

    #[test]
    fn gdrive_url_no_credentials_at_all_returns_no_credentials_error() {
        let config = SourceConfig::default();
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var("PLUQ_GDRIVE_KEY");
            std::env::remove_var("PLUQ_GDRIVE_CLIENT_ID");
        }

        let auth = resolve_gdrive_auth(&config);
        assert!(matches!(
            auth,
            Err(SourceError::NoCredentials {
                provider: "Google Drive",
                ..
            })
        ));
    }
}
