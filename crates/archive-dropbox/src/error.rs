#[derive(Debug, thiserror::Error)]
pub enum DropboxError {
    #[error("Dropbox: shared link not found or expired ({url})")]
    FileNotFound { url: String },

    #[error("Dropbox: shared link is password-protected or private ({url})")]
    NotPublic { url: String },

    #[error(
        "Dropbox: app credentials invalid — check ARCHIVE_SNIPER_DROPBOX_APP_KEY / ARCHIVE_SNIPER_DROPBOX_APP_SECRET"
    )]
    InvalidCredentials,

    #[error("Dropbox: rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Dropbox: monthly bandwidth quota exceeded")]
    QuotaExceeded,

    #[error(
        "Dropbox: app credentials required — set ARCHIVE_SNIPER_DROPBOX_APP_KEY and ARCHIVE_SNIPER_DROPBOX_APP_SECRET"
    )]
    AppCredentialsMissing,

    #[error("Dropbox: invalid shared link URL ({url})")]
    InvalidUrl { url: String },

    #[error("Dropbox: OAuth token refresh failed — {reason}")]
    TokenRefreshFailed { reason: String },

    #[error("Dropbox API error: {detail}")]
    Api { status: u16, detail: String },
}
