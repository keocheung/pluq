use archive_core::io::read_error::{NetworkPhase, ReadErrorKind, ReadFailure, SourceInfo};
use archive_core::{CoreError, ReadError};
use tracing::{debug, warn};

// ── ArchiveError ────────────────────────────────────────────────────────────────

/// User-facing errors produced by the state layer.
///
/// Core errors are *translated* into these variants — products never see
/// `CoreError` or `ReadError` directly. `Transport` errors are forwarded
/// blindly so decorators (auth, metering) can downcast their own types.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("file corrupted: {filename}")]
    FileCorrupted { filename: String },

    #[error("password needed")]
    PasswordNeeded,

    #[error("unsupported format: {detail}")]
    UnsupportedFormat { detail: String },

    #[error("security risk: {detail}")]
    SecurityRisk { detail: String },

    #[error("connection failed: {detail}")]
    ConnectionFailed { detail: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("cache invalid")]
    CacheInvalid,

    #[error("internal error: {detail}")]
    Internal { detail: String },

    /// Opaque error from a `ReadAt` decorator — forwarded blindly to the product.
    #[error("transport error: {0}")]
    Transport(Box<dyn std::error::Error + Send + Sync>),
}

// ── Error translation ─────────────────────────────────────────────────────────

/// Translate a core `CoreError` into a user-facing `ArchiveError`.
///
/// This is the single translation boundary: core errors never escape the
/// state layer. Transport errors are forwarded opaquely so decorators
/// (auth, metering, etc.) can downcast their own payloads.
pub fn translate_core_error(err: CoreError) -> ArchiveError {
    debug!(core_error = %err, "translating CoreError → ArchiveError");
    match err {
        // I/O errors: route by ReadError variant.
        CoreError::Io(ReadError::Transport(boxed)) => ArchiveError::Transport(boxed),
        CoreError::Io(ReadError::Failed(f)) => ArchiveError::ConnectionFailed {
            detail: build_user_message(&f),
        },
        CoreError::Io(ReadError::OutOfBounds { .. }) => ArchiveError::Internal {
            detail: "unexpected OutOfBounds at state layer".into(),
        },

        // Corruption.
        CoreError::CrcMismatch { entry, .. } => ArchiveError::FileCorrupted { filename: entry },
        CoreError::InvalidFormat { message } => ArchiveError::UnsupportedFormat { detail: message },
        CoreError::CorruptHeader { reason, .. } => ArchiveError::FileCorrupted {
            filename: format!("header: {reason}"),
        },
        CoreError::BitstreamError { detail } => ArchiveError::FileCorrupted {
            filename: format!("decompression: {detail}"),
        },

        // Encryption.
        CoreError::PasswordRequired => ArchiveError::PasswordNeeded,
        CoreError::WrongPassword { .. } => ArchiveError::PasswordNeeded,

        // Unsupported.
        CoreError::UnsupportedFormat { format } => {
            ArchiveError::UnsupportedFormat { detail: format }
        }
        CoreError::UnsupportedCompression { algorithm } => {
            ArchiveError::UnsupportedFormat { detail: algorithm }
        }
        CoreError::UnsupportedFeature { detail } => ArchiveError::UnsupportedFormat { detail },

        // Security.
        CoreError::ResourceLimitExceeded { message } => {
            warn!(message = %message, "resource limit exceeded");
            ArchiveError::SecurityRisk { detail: message }
        }
        CoreError::PathTraversal { ref path } => {
            warn!(path = %path, "path traversal detected");
            ArchiveError::SecurityRisk {
                detail: format!("dangerous path: {path}"),
            }
        }
        CoreError::SymlinkTargetEscape {
            ref path,
            ref target,
        } => {
            warn!(path = %path, target = %target, "symlink escape detected");
            ArchiveError::SecurityRisk {
                detail: format!("dangerous symlink: {path} -> {target}"),
            }
        }

        // Cancellation.
        CoreError::Cancelled => ArchiveError::Cancelled,

        // Catch-all for any future variants added to CoreError.
        other => ArchiveError::Internal {
            detail: other.to_string(),
        },
    }
}

// ── User message builder ──────────────────────────────────────────────────────

/// Build a user-facing message from a structured [`ReadFailure`].
///
/// Uses the structured `source`, `operation`, and `kind` fields to produce
/// a clear, actionable message without string matching on `diagnostic`.
pub fn build_user_message(f: &ReadFailure) -> String {
    let source_name = match &f.source {
        SourceInfo::LocalFile { path } => path.display().to_string(),
        SourceInfo::Http { url } => url.clone(),
    };
    match &f.kind {
        ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Connect,
        } => format!("Connection to '{source_name}' timed out. Check your internet."),
        ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Transfer,
        } => format!("Connection to '{source_name}' dropped during download."),
        ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Tls,
        } => format!("TLS handshake to '{source_name}' timed out."),
        ReadErrorKind::NetworkReset => {
            format!("Connection to '{source_name}' was reset unexpectedly.")
        }
        ReadErrorKind::TlsFailure { reason } => {
            format!("SSL error for '{source_name}': {reason}")
        }
        ReadErrorKind::DnsFailure { hostname } => {
            format!("Cannot reach '{hostname}'. Check your internet connection.")
        }
        ReadErrorKind::HttpStatus { code: 404, .. } => {
            format!("File not found at '{source_name}' (HTTP 404).")
        }
        ReadErrorKind::HttpStatus { code, .. } => {
            format!("Server error {code} from '{source_name}'.")
        }
        ReadErrorKind::RangeNotSupported => format!(
            "'{source_name}' does not support partial downloads. Cloud sniping unavailable."
        ),
        ReadErrorKind::PermissionDenied => {
            format!("Permission denied: cannot read '{source_name}'.")
        }
        ReadErrorKind::FileNotFound => format!("'{source_name}' was moved or deleted."),
        ReadErrorKind::DiskIoError { os_code } => format!(
            "Disk error reading '{source_name}'. The storage device may be failing. (code: {os_code:?})"
        ),
        ReadErrorKind::FileLocked => format!("'{source_name}' is locked by another program."),
        _ => format!("I/O error reading '{source_name}': {}", f.diagnostic),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use archive_core::io::read_error::{
        NetworkPhase, ReadErrorKind, ReadFailure, ReadOperation, SourceInfo,
    };
    use archive_core::{CoreError, PasswordDetectionMethod, ReadError};

    use super::*;

    fn make_failed(kind: ReadErrorKind) -> CoreError {
        CoreError::Io(ReadError::Failed(Box::new(ReadFailure {
            source: SourceInfo::Http {
                url: "https://example.com/file.zip".into(),
            },
            operation: ReadOperation::ReadRange {
                offset: 0,
                len: 100,
            },
            kind,
            diagnostic: "test diagnostic".into(),
        })))
    }

    #[test]
    fn transport_is_tunnelled() {
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("auth expired"));
        let err = CoreError::Io(ReadError::Transport(boxed));
        assert!(matches!(
            translate_core_error(err),
            ArchiveError::Transport(_)
        ));
    }

    #[test]
    fn out_of_bounds_becomes_internal() {
        let err = CoreError::Io(ReadError::OutOfBounds {
            offset: 0,
            len: 100,
            size: 50,
        });
        assert!(matches!(
            translate_core_error(err),
            ArchiveError::Internal { .. }
        ));
    }

    #[test]
    fn crc_mismatch_becomes_file_corrupted() {
        let err = CoreError::CrcMismatch {
            entry: "file.txt".into(),
            expected: 0xDEAD,
            actual: 0xBEEF,
        };
        assert!(matches!(
            translate_core_error(err),
            ArchiveError::FileCorrupted { filename } if filename == "file.txt"
        ));
    }

    #[test]
    fn password_required_becomes_password_needed() {
        assert!(matches!(
            translate_core_error(CoreError::PasswordRequired),
            ArchiveError::PasswordNeeded
        ));
    }

    #[test]
    fn wrong_password_becomes_password_needed() {
        assert!(matches!(
            translate_core_error(CoreError::WrongPassword {
                method: PasswordDetectionMethod::CrcMismatch
            }),
            ArchiveError::PasswordNeeded
        ));
    }

    #[test]
    fn unsupported_compression_becomes_unsupported_format() {
        let err = CoreError::UnsupportedCompression {
            algorithm: "LZMA2".into(),
        };
        assert!(matches!(
            translate_core_error(err),
            ArchiveError::UnsupportedFormat { detail } if detail == "LZMA2"
        ));
    }

    #[test]
    fn path_traversal_becomes_security_risk() {
        let err = CoreError::PathTraversal {
            path: "../etc/passwd".into(),
        };
        assert!(matches!(
            translate_core_error(err),
            ArchiveError::SecurityRisk { .. }
        ));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert!(matches!(
            translate_core_error(CoreError::Cancelled),
            ArchiveError::Cancelled
        ));
    }

    #[test]
    fn build_user_message_timeout_connect() {
        let msg = match translate_core_error(make_failed(ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Connect,
        })) {
            ArchiveError::ConnectionFailed { detail } => detail,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("timed out"), "got: {msg}");
    }

    #[test]
    fn build_user_message_file_not_found() {
        let err = CoreError::Io(ReadError::Failed(Box::new(ReadFailure {
            source: SourceInfo::LocalFile {
                path: PathBuf::from("/tmp/test.zip"),
            },
            operation: ReadOperation::Open,
            kind: ReadErrorKind::FileNotFound,
            diagnostic: "ENOENT".into(),
        })));
        let msg = match translate_core_error(err) {
            ArchiveError::ConnectionFailed { detail } => detail,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("moved or deleted"), "got: {msg}");
    }

    #[test]
    fn build_user_message_http_404() {
        let msg = match translate_core_error(make_failed(ReadErrorKind::HttpStatus {
            code: 404,
            headers: vec![],
            body: None,
        })) {
            ArchiveError::ConnectionFailed { detail } => detail,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("404"), "got: {msg}");
    }
}
