use thiserror::Error;

use crate::io::ReadError;

/// Errors produced by archive operations.
///
/// Variants are structured so callers can programmatically distinguish them
/// without string matching. Use the helper methods (`is_recoverable()`,
/// `is_corruption()`, etc.) for broad categorisation.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Underlying I/O or network error.
    ///
    /// Wraps a [`ReadError`] from the I/O layer — see that type for structured details
    /// including source info, operation context, and failure kind.
    #[error("I/O error: {0}")]
    Io(ReadError),

    /// The bytes do not represent a valid archive (bad signature, truncated structures, etc.).
    #[error("invalid archive format: {message}")]
    InvalidFormat { message: String },

    /// Known archive format but this build cannot handle it (RAR5, 7z encrypted, etc.).
    #[error("unsupported format: {format}")]
    UnsupportedFormat { format: String },

    /// The archive uses a compression algorithm the current build cannot decompress.
    #[error("unsupported compression: {algorithm}")]
    UnsupportedCompression { algorithm: String },

    /// Known format, but a specific feature within it is not yet supported.
    #[error("unsupported feature: {detail}")]
    UnsupportedFeature { detail: String },

    /// A structural header in the archive is corrupt (bad offsets, invalid sizes, etc.).
    #[error("corrupt header at offset {offset}: {reason}")]
    CorruptHeader { offset: u64, reason: String },

    /// Decompression algorithm reported a bitstream or block error.
    ///
    /// Distinct from [`CrcMismatch`](Self::CrcMismatch): the *decompressor itself* rejected
    /// the stream, not a post-decompression integrity check.
    #[error("bitstream error: {detail}")]
    BitstreamError { detail: String },

    /// CRC-32 mismatch after decompression — data is corrupted or the password is wrong.
    #[error("CRC mismatch for '{entry}': expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch {
        entry: String,
        expected: u32,
        actual: u32,
    },

    /// The archive (or entry) is encrypted and no password was supplied.
    #[error("password required")]
    PasswordRequired,

    /// A password was supplied but produced a CRC or MAC failure.
    #[error("wrong password or corrupted data")]
    WrongPassword { method: PasswordDetectionMethod },

    /// An entry path contains `..` or an absolute root that would escape the output directory.
    #[error("path traversal detected: {path}")]
    PathTraversal { path: String },

    /// A symlink target would escape the extraction directory.
    #[error("symlink escape detected: '{path}' → '{target}'")]
    SymlinkTargetEscape { path: String, target: String },

    /// A safety limit was hit (entry count, header size, zip-bomb ratio, LZMA dict size, etc.).
    #[error("resource limit exceeded: {message}")]
    ResourceLimitExceeded { message: String },

    /// The operation was cancelled by the caller (via `ProgressReporter::should_cancel()`).
    #[error("operation cancelled")]
    Cancelled,
}

impl From<ReadError> for ArchiveError {
    fn from(e: ReadError) -> Self {
        ArchiveError::Io(e)
    }
}

impl ArchiveError {
    /// True for errors where retrying with different input might succeed
    /// (wrong password, cancellation).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::PasswordRequired | Self::WrongPassword { .. } | Self::Cancelled
        )
    }

    /// True for errors indicating data corruption.
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::CrcMismatch { .. }
                | Self::InvalidFormat { .. }
                | Self::CorruptHeader { .. }
                | Self::BitstreamError { .. }
        )
    }

    /// True for encryption-related errors (no password, wrong password).
    pub fn is_encryption_error(&self) -> bool {
        matches!(self, Self::PasswordRequired | Self::WrongPassword { .. })
    }

    /// True for path-based security errors.
    pub fn is_security_error(&self) -> bool {
        matches!(
            self,
            Self::PathTraversal { .. } | Self::SymlinkTargetEscape { .. }
        )
    }

    /// True when the archive uses a compression or format the build doesn't support.
    pub fn is_unsupported(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedCompression { .. }
                | Self::UnsupportedFormat { .. }
                | Self::UnsupportedFeature { .. }
        )
    }
}

// ── Password detection context ────────────────────────────────────────────────

/// How encryption was detected on an archive entry.
///
/// Carried in [`ArchiveError::WrongPassword`] to give the state layer
/// and product enough context to build a precise user-facing message.
///
/// Different detection methods have different confidence levels:
/// - `EarlyHeaderValidation` — near-certain (header check failed before any data read)
/// - `CrcMismatch` — high confidence (decryption produced wrong output)
/// - `DecompressionFailure` — moderate confidence (decrypt worked but decompressor choked)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordDetectionMethod {
    /// Encryption flag in the local file header was set and validated early,
    /// before any decompression was attempted.
    EarlyHeaderValidation,
    /// Decryption produced bytes that failed the CRC-32 integrity check.
    CrcMismatch,
    /// Decryption appeared to succeed but the decompressor rejected the bitstream.
    DecompressionFailure,
}
