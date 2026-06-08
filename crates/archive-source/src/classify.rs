use std::time::Duration;

use archive_dropbox::DropboxError;
use archive_gdrive::GDriveError;

/// Provider-agnostic error classification for transport tunnel errors.
///
/// Used by product layers to interpret `ArchiveError::Transport` contents
/// without depending on individual provider crates.
#[derive(Debug)]
pub enum ProviderError {
    /// File exists but is not publicly shared. OAuth may help.
    NotPublic { provider: &'static str },
    /// File does not exist or was deleted.
    FileNotFound { provider: &'static str },
    /// Rate limited by provider. Retry after delay.
    RateLimited {
        provider: &'static str,
        retry_after: Option<Duration>,
    },
    /// API key or credentials are invalid.
    InvalidCredentials { provider: &'static str },
    /// Provider-specific quota exceeded.
    QuotaExceeded { provider: &'static str },
    /// Network connect/read timed out. Transient — retry.
    Timeout { provider: &'static str },
    /// Provider server problem (5xx, range-not-honored, other API error).
    /// NOT a credentials issue — must never trigger an auth/sign-in prompt.
    /// Transient — retry.
    Unavailable { provider: &'static str },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublic { provider } => {
                write!(f, "{provider}: file is not publicly shared")
            }
            Self::FileNotFound { provider } => {
                write!(f, "{provider}: file not found or deleted")
            }
            Self::RateLimited {
                provider,
                retry_after,
            } => {
                write!(f, "{provider}: rate limited")?;
                if let Some(d) = retry_after {
                    write!(f, " (retry after {}s)", d.as_secs())?;
                }
                Ok(())
            }
            Self::InvalidCredentials { provider } => {
                write!(f, "{provider}: invalid credentials")
            }
            Self::QuotaExceeded { provider } => {
                write!(f, "{provider}: quota exceeded")
            }
            Self::Timeout { provider } => {
                write!(f, "{provider}: connection timed out")
            }
            Self::Unavailable { provider } => {
                write!(f, "{provider}: temporary server problem")
            }
        }
    }
}

impl std::error::Error for ProviderError {}

/// Attempt to classify a transport tunnel error into a provider-agnostic category.
///
/// Called by products on `ArchiveError::Transport` contents. Returns `None`
/// if the error is not from a known provider (e.g., plain HTTP error, or
/// from a decorator like `AuthReadAt`).
///
/// # Example
///
/// ```ignore
/// if let ArchiveError::Transport(inner) = &err {
///     if let Some(pe) = classify_transport(inner.as_ref()) {
///         eprintln!("Provider error: {pe}");
///     }
/// }
/// ```
pub fn classify_transport(err: &(dyn std::error::Error + 'static)) -> Option<ProviderError> {
    // ── Google Drive ─────────────────────────────────────────────────────
    if let Some(e) = err.downcast_ref::<GDriveError>() {
        return Some(classify_gdrive(e));
    }

    // ── Dropbox ──────────────────────────────────────────────────────────
    if let Some(e) = err.downcast_ref::<DropboxError>() {
        return Some(classify_dropbox(e));
    }

    None
}

fn classify_gdrive(e: &GDriveError) -> ProviderError {
    match e {
        GDriveError::FileNotFound { .. } => ProviderError::FileNotFound {
            provider: "Google Drive",
        },
        GDriveError::NotPublic { .. } => ProviderError::NotPublic {
            provider: "Google Drive",
        },
        GDriveError::InvalidApiKey => ProviderError::InvalidCredentials {
            provider: "Google Drive",
        },
        GDriveError::ApiKeyMissing => ProviderError::InvalidCredentials {
            provider: "Google Drive",
        },
        GDriveError::RateLimited { retry_after_secs } => ProviderError::RateLimited {
            provider: "Google Drive",
            retry_after: Some(Duration::from_secs(*retry_after_secs)),
        },
        GDriveError::QuotaExceeded => ProviderError::QuotaExceeded {
            provider: "Google Drive",
        },
        GDriveError::Timeout => ProviderError::Timeout {
            provider: "Google Drive",
        },
        GDriveError::InvalidUrl { .. } => ProviderError::InvalidCredentials {
            provider: "Google Drive",
        },
        // A generic API/transport error (5xx, range-not-honored, unexpected status)
        // is NOT an auth problem — must not trigger a sign-in prompt.
        GDriveError::Api { .. } => ProviderError::Unavailable {
            provider: "Google Drive",
        },
        GDriveError::TokenRefreshFailed { .. } => ProviderError::InvalidCredentials {
            provider: "Google Drive",
        },
    }
}

fn classify_dropbox(e: &DropboxError) -> ProviderError {
    match e {
        DropboxError::FileNotFound { .. } => ProviderError::FileNotFound {
            provider: "Dropbox",
        },
        DropboxError::NotPublic { .. } => ProviderError::NotPublic {
            provider: "Dropbox",
        },
        DropboxError::InvalidCredentials | DropboxError::AppCredentialsMissing => {
            ProviderError::InvalidCredentials {
                provider: "Dropbox",
            }
        }
        DropboxError::RateLimited { retry_after_secs } => ProviderError::RateLimited {
            provider: "Dropbox",
            retry_after: Some(std::time::Duration::from_secs(*retry_after_secs)),
        },
        DropboxError::QuotaExceeded => ProviderError::QuotaExceeded {
            provider: "Dropbox",
        },
        DropboxError::Timeout => ProviderError::Timeout {
            provider: "Dropbox",
        },
        DropboxError::InvalidUrl { .. } => ProviderError::InvalidCredentials {
            provider: "Dropbox",
        },
        // Range-not-honored (load-balanced 200 backend) and generic API/transport
        // errors are NOT auth problems — must not trigger a sign-in prompt.
        DropboxError::RangeNotHonored { .. } | DropboxError::Api { .. } => {
            ProviderError::Unavailable {
                provider: "Dropbox",
            }
        }
        DropboxError::TokenRefreshFailed { .. } => ProviderError::InvalidCredentials {
            provider: "Dropbox",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_dropbox_file_not_found() {
        let err = DropboxError::FileNotFound { url: "x".into() };
        let pe = classify_transport(&err).unwrap();
        assert!(matches!(
            pe,
            ProviderError::FileNotFound {
                provider: "Dropbox"
            }
        ));
    }

    #[test]
    fn classify_dropbox_not_public() {
        let err = DropboxError::NotPublic { url: "x".into() };
        let pe = classify_transport(&err).unwrap();
        assert!(matches!(
            pe,
            ProviderError::NotPublic {
                provider: "Dropbox"
            }
        ));
    }

    #[test]
    fn classify_dropbox_invalid_credentials() {
        let err = DropboxError::InvalidCredentials;
        let pe = classify_transport(&err).unwrap();
        assert!(matches!(
            pe,
            ProviderError::InvalidCredentials {
                provider: "Dropbox"
            }
        ));
    }

    #[test]
    fn classify_dropbox_rate_limited() {
        let err = DropboxError::RateLimited {
            retry_after_secs: 30,
        };
        let pe = classify_transport(&err).unwrap();
        if let ProviderError::RateLimited {
            provider,
            retry_after,
        } = pe
        {
            assert_eq!(provider, "Dropbox");
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
        } else {
            panic!("expected RateLimited");
        }
    }

    #[test]
    fn classify_dropbox_range_not_honored_is_unavailable_not_auth() {
        let pe = classify_transport(&DropboxError::RangeNotHonored { attempts: 6 }).unwrap();
        assert!(
            matches!(
                pe,
                ProviderError::Unavailable {
                    provider: "Dropbox"
                }
            ),
            "RangeNotHonored must be Unavailable (non-auth), got {pe:?}"
        );
    }

    #[test]
    fn classify_generic_api_is_unavailable_not_auth() {
        // A generic API error must NOT route to InvalidCredentials (no sign-in prompt).
        let dbx = classify_transport(&DropboxError::Api {
            status: 200,
            detail: String::new(),
        })
        .unwrap();
        assert!(matches!(
            dbx,
            ProviderError::Unavailable {
                provider: "Dropbox"
            }
        ));
        let gdr = classify_transport(&GDriveError::Api {
            status: 500,
            detail: String::new(),
        })
        .unwrap();
        assert!(matches!(
            gdr,
            ProviderError::Unavailable {
                provider: "Google Drive"
            }
        ));
    }

    #[test]
    fn classify_dropbox_timeout() {
        let pe = classify_transport(&DropboxError::Timeout).unwrap();
        assert!(matches!(
            pe,
            ProviderError::Timeout {
                provider: "Dropbox"
            }
        ));
    }

    #[test]
    fn classify_gdrive_timeout() {
        let pe = classify_transport(&GDriveError::Timeout).unwrap();
        assert!(matches!(
            pe,
            ProviderError::Timeout {
                provider: "Google Drive"
            }
        ));
    }

    #[test]
    fn classify_dropbox_quota_exceeded() {
        let err = DropboxError::QuotaExceeded;
        let pe = classify_transport(&err).unwrap();
        assert!(matches!(
            pe,
            ProviderError::QuotaExceeded {
                provider: "Dropbox"
            }
        ));
    }
}
