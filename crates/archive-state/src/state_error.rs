use archive_core::io::read_error::{
    NetworkPhase, ReadErrorKind, ReadFailure, SourceInfo,
};
use archive_core::{ArchiveError, ReadError};

// ── StateError ────────────────────────────────────────────────────────────────

/// User-facing errors produced by the state layer.
///
/// Core errors are *translated* into these variants — products never see
/// `ArchiveError` or `ReadError` directly. `Transport` errors are forwarded
/// blindly so decorators (auth, metering) can downcast their own types.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StateError {
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

/// Translate a core `ArchiveError` into a user-facing `StateError`.
///
/// This is the single translation boundary: core errors never escape the
/// state layer. Transport errors are forwarded opaquely so decorators
/// (auth, metering, etc.) can downcast their own payloads.
pub fn translate_core_error(err: ArchiveError) -> StateError {
    match err {
        // I/O errors: route by ReadError variant.
        ArchiveError::Io(ReadError::Transport(boxed)) => StateError::Transport(boxed),
        ArchiveError::Io(ReadError::Failed(f)) => StateError::ConnectionFailed {
            detail: build_user_message(&f),
        },
        ArchiveError::Io(ReadError::OutOfBounds { .. }) => StateError::Internal {
            detail: "unexpected OutOfBounds at state layer".into(),
        },

        // Corruption.
        ArchiveError::CrcMismatch { entry, .. } => StateError::FileCorrupted { filename: entry },
        ArchiveError::InvalidFormat { message } => StateError::UnsupportedFormat {
            detail: message,
        },
        ArchiveError::CorruptHeader { reason, .. } => StateError::FileCorrupted {
            filename: format!("header: {reason}"),
        },
        ArchiveError::BitstreamError { detail } => StateError::FileCorrupted {
            filename: format!("decompression: {detail}"),
        },

        // Encryption.
        ArchiveError::PasswordRequired => StateError::PasswordNeeded,
        ArchiveError::WrongPassword { .. } => StateError::PasswordNeeded,

        // Unsupported.
        ArchiveError::UnsupportedFormat { format } => StateError::UnsupportedFormat {
            detail: format,
        },
        ArchiveError::UnsupportedCompression { algorithm } => StateError::UnsupportedFormat {
            detail: algorithm,
        },
        ArchiveError::UnsupportedFeature { detail } => StateError::UnsupportedFormat { detail },

        // Security.
        ArchiveError::PathTraversal { path } => StateError::SecurityRisk {
            detail: format!("dangerous path: {path}"),
        },
        ArchiveError::SymlinkTargetEscape { path, target } => StateError::SecurityRisk {
            detail: format!("dangerous symlink: {path} -> {target}"),
        },

        // Cancellation.
        ArchiveError::Cancelled => StateError::Cancelled,

        // Catch-all for any future variants added to ArchiveError.
        other => StateError::Internal {
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
    use archive_core::{ArchiveError, PasswordDetectionMethod, ReadError};

    use super::*;

    fn make_failed(kind: ReadErrorKind) -> ArchiveError {
        ArchiveError::Io(ReadError::Failed(Box::new(ReadFailure {
            source: SourceInfo::Http {
                url: "https://example.com/file.zip".into(),
            },
            operation: ReadOperation::ReadRange { offset: 0, len: 100 },
            kind,
            diagnostic: "test diagnostic".into(),
        })))
    }

    #[test]
    fn transport_is_tunnelled() {
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("auth expired"));
        let err = ArchiveError::Io(ReadError::Transport(boxed));
        assert!(matches!(translate_core_error(err), StateError::Transport(_)));
    }

    #[test]
    fn out_of_bounds_becomes_internal() {
        let err = ArchiveError::Io(ReadError::OutOfBounds {
            offset: 0,
            len: 100,
            size: 50,
        });
        assert!(matches!(
            translate_core_error(err),
            StateError::Internal { .. }
        ));
    }

    #[test]
    fn crc_mismatch_becomes_file_corrupted() {
        let err = ArchiveError::CrcMismatch {
            entry: "file.txt".into(),
            expected: 0xDEAD,
            actual: 0xBEEF,
        };
        assert!(matches!(
            translate_core_error(err),
            StateError::FileCorrupted { filename } if filename == "file.txt"
        ));
    }

    #[test]
    fn password_required_becomes_password_needed() {
        assert!(matches!(
            translate_core_error(ArchiveError::PasswordRequired),
            StateError::PasswordNeeded
        ));
    }

    #[test]
    fn wrong_password_becomes_password_needed() {
        assert!(matches!(
            translate_core_error(ArchiveError::WrongPassword {
                method: PasswordDetectionMethod::CrcMismatch
            }),
            StateError::PasswordNeeded
        ));
    }

    #[test]
    fn unsupported_compression_becomes_unsupported_format() {
        let err = ArchiveError::UnsupportedCompression {
            algorithm: "LZMA2".into(),
        };
        assert!(matches!(
            translate_core_error(err),
            StateError::UnsupportedFormat { detail } if detail == "LZMA2"
        ));
    }

    #[test]
    fn path_traversal_becomes_security_risk() {
        let err = ArchiveError::PathTraversal {
            path: "../etc/passwd".into(),
        };
        assert!(matches!(
            translate_core_error(err),
            StateError::SecurityRisk { .. }
        ));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert!(matches!(
            translate_core_error(ArchiveError::Cancelled),
            StateError::Cancelled
        ));
    }

    #[test]
    fn build_user_message_timeout_connect() {
        let msg = match translate_core_error(make_failed(ReadErrorKind::NetworkTimeout {
            phase: NetworkPhase::Connect,
        })) {
            StateError::ConnectionFailed { detail } => detail,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("timed out"), "got: {msg}");
    }

    #[test]
    fn build_user_message_file_not_found() {
        let err = ArchiveError::Io(ReadError::Failed(Box::new(ReadFailure {
            source: SourceInfo::LocalFile {
                path: PathBuf::from("/tmp/test.zip"),
            },
            operation: ReadOperation::Open,
            kind: ReadErrorKind::FileNotFound,
            diagnostic: "ENOENT".into(),
        })));
        let msg = match translate_core_error(err) {
            StateError::ConnectionFailed { detail } => detail,
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
            StateError::ConnectionFailed { detail } => detail,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("404"), "got: {msg}");
    }
}
