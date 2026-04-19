use archive_auth::{AuthError, StoredToken, DROPBOX_CONFIG};

/// Run the PKCE loopback OAuth flow for Dropbox and return a fresh token.
pub async fn authenticate(timeout_secs: u64) -> Result<StoredToken, AuthError> {
    let client_id = crate::oauth_client_id().ok_or_else(|| {
        AuthError::OAuth(
            "Dropbox OAuth client ID not configured. \
             Set ARCHIVE_SNIPER_DROPBOX_CLIENT_ID."
                .into(),
        )
    })?;
    let client_secret = crate::oauth_client_secret();
    archive_auth::authenticate(
        &DROPBOX_CONFIG,
        &client_id,
        client_secret.as_deref(),
        timeout_secs,
    )
    .await
}
