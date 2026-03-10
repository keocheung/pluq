use thiserror::Error;

/// Errors produced by archive operations.
///
/// Variants are structured so callers can programmatically distinguish them
/// without string matching. Use the helper methods (`is_recoverable()`,
/// `is_corruption()`, etc.) for broad categorisation.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Underlying I/O error (file, network, etc.).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Requested byte range exceeds the source size.
    #[error("out of bounds: offset {offset} + len {len} exceeds size {size}")]
    OutOfBounds { offset: u64, len: u64, size: u64 },

    /// The bytes do not represent a valid archive (bad signature, truncated structures, etc.).
    #[error("invalid archive format: {message}")]
    InvalidFormat { message: String },

    /// The archive uses a compression method the current build cannot handle.
    #[error("unsupported compression method: {method}")]
    UnsupportedMethod { method: String },

    /// CRC32 mismatch after decompression — data is corrupted or password is wrong.
    #[error("CRC mismatch for '{entry_name}': expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch {
        entry_name: String,
        expected: u32,
        actual: u32,
    },

    /// The archive (or entry) is encrypted and no password was supplied.
    #[error("password required")]
    PasswordRequired,

    /// A password was supplied but produced a CRC or MAC failure.
    #[error("wrong password or corrupted data")]
    WrongPassword,

    /// An entry path contains `..` or an absolute root that would escape the output directory.
    #[error("path traversal detected: {path}")]
    PathTraversal { path: String },

    /// A safety limit was hit (entry count, header size, zip-bomb ratio, LZMA dict size, etc.).
    #[error("resource limit exceeded: {message}")]
    ResourceLimitExceeded { message: String },

    /// The operation was cancelled by the caller (via `ProgressReporter::should_cancel()`).
    #[error("operation cancelled")]
    Cancelled,

    /// Catch-all for unexpected or format-specific errors not covered above.
    #[error("{0}")]
    Other(String),
}

impl ArchiveError {
    /// True for errors where retrying with different input might succeed
    /// (wrong password, cancellation).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::PasswordRequired | Self::WrongPassword | Self::Cancelled
        )
    }

    /// True for errors indicating data corruption (CRC mismatch, invalid format).
    pub fn is_corruption(&self) -> bool {
        matches!(self, Self::CrcMismatch { .. } | Self::InvalidFormat { .. })
    }

    /// True for encryption-related errors (no password, wrong password).
    pub fn is_encryption_error(&self) -> bool {
        matches!(self, Self::PasswordRequired | Self::WrongPassword)
    }

    /// True for path traversal security errors.
    pub fn is_security_error(&self) -> bool {
        matches!(self, Self::PathTraversal { .. })
    }

    /// True when the archive uses a method the build doesn't support.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::UnsupportedMethod { .. })
    }
}

// ── Password detection context ────────────────────────────────────────────────

/// How encryption was detected on an archive entry.
///
/// Carried in [`ArchiveError::WrongPassword`] (Step 3) to give the state layer
/// and product enough context to build a precise user-facing message.
///
/// Different detection methods have different confidence levels:
/// - `EarlyHeaderValidation` — near-certain (header check failed before any data read)
/// - `CrcMismatch` — high confidence (decryption produced wrong output)
/// - `DecompressionFailure` — moderate confidence (decrypt worked but decompressor choked)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // used in Step 3 when added to ArchiveError::WrongPassword
pub enum PasswordDetectionMethod {
    /// Encryption flag in the local file header was set and validated early,
    /// before any decompression was attempted.
    EarlyHeaderValidation,
    /// Decryption produced bytes that failed the CRC-32 integrity check.
    CrcMismatch,
    /// Decryption appeared to succeed but the decompressor rejected the bitstream.
    DecompressionFailure,
}
