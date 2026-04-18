#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no config directory available on this platform")]
    NoConfigDir,
    #[error("OAuth error: {0}")]
    OAuth(String),
    #[error("browser launch failed: {0}")]
    Browser(String),
    #[error("callback server error: {0}")]
    Callback(String),
    #[error("authentication timed out after {secs}s")]
    Timeout { secs: u64 },
    #[error("authentication cancelled by user")]
    Cancelled,
}
