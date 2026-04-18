use archive_auth::{AuthError, StoredToken, GDRIVE_CONFIG};

/// Run the PKCE loopback OAuth flow for Google Drive and return a fresh token.
///
/// Thin wrapper over `archive_auth::authenticate` that:
/// - guards client credential discovery (returns `AuthError` if not configured), and
/// - binds the provider-specific `GDRIVE_CONFIG`.
///
/// Called from `snipe auth add gdrive` (step 5) and the TUI OAuth prompt (step 6).
pub async fn authenticate(timeout_secs: u64) -> Result<StoredToken, AuthError> {
    let client_id = crate::oauth_client_id()
        .ok_or_else(|| AuthError::OAuth("GDrive OAuth client ID not configured. Set ARCHIVE_SNIPER_GDRIVE_CLIENT_ID.".into()))?;
    let client_secret = crate::oauth_client_secret();
    archive_auth::authenticate(
        &GDRIVE_CONFIG,
        &client_id,
        client_secret.as_deref(),
        timeout_secs,
    )
    .await
}
