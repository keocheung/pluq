use std::fmt;

/// Errors that occur during source resolution (before any archive I/O).
///
/// These are construction-time failures — a provider was detected but
/// setup failed (missing credentials, unparseable URL).
#[derive(Debug)]
pub enum SourceError {
    /// Provider detected but credentials are missing.
    NoCredentials {
        provider: &'static str,
        guidance: String,
    },
    /// URL matched a provider pattern but couldn't be parsed.
    InvalidUrl {
        url: String,
        reason: String,
    },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredentials { provider, guidance } => {
                write!(f, "{provider}: {guidance}")
            }
            Self::InvalidUrl { url, reason } => {
                write!(f, "invalid URL '{url}': {reason}")
            }
        }
    }
}

impl std::error::Error for SourceError {}
