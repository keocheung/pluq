#[derive(Debug, thiserror::Error)]
pub enum GDriveError {
    #[error("Google Drive: file not found ({file_id})")]
    FileNotFound { file_id: String },

    #[error("Google Drive: file is not shared publicly ({file_id})")]
    NotPublic { file_id: String },

    #[error("Google Drive: API key invalid or Drive API not enabled")]
    InvalidApiKey,

    #[error("Google Drive: rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Google Drive: quota exceeded")]
    QuotaExceeded,

    #[error("Google Drive: API key required — set ARCHIVE_SNIPER_GDRIVE_KEY")]
    ApiKeyMissing,

    #[error("Google Drive: could not extract file ID from URL")]
    InvalidUrl { url: String },

    #[error("Google Drive API error: {detail}")]
    Api { status: u16, detail: String },

    #[error("Google Drive: OAuth token refresh failed — {reason}")]
    TokenRefreshFailed { reason: String },
}
