use std::fmt;
use std::path::PathBuf;

use bytes::Bytes;

// ── Top-level error ───────────────────────────────────────────────────────────

/// Error returned by [`ReadAt`](super::ReadAt) implementations.
///
/// This is the I/O layer's own error language — independent of [`ArchiveError`](crate::ArchiveError).
/// Format parsers convert `ReadError → ArchiveError` at their call boundaries.
///
/// # Variants
/// - [`Failed`](Self::Failed): structured failure with source, operation, and kind context.
/// - [`Transport`](Self::Transport): opaque decorator error forwarded blindly.
/// - [`OutOfBounds`](Self::OutOfBounds): read range exceeds source size.
#[derive(Debug)]
pub enum ReadError {
    /// A structured I/O failure with full diagnostic context.
    Failed(ReadFailure),
    /// An opaque error from a [`ReadAt`](super::ReadAt) decorator (e.g. auth, metering).
    ///
    /// Carried blindly through the layers — only the originating decorator can downcast it.
    Transport(Box<dyn std::error::Error + Send + Sync>),
    /// Requested byte range exceeds the source size.
    OutOfBounds { offset: u64, len: u64, size: u64 },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Failed(failure) => write!(f, "{failure}"),
            ReadError::Transport(e) => write!(f, "transport error: {e}"),
            ReadError::OutOfBounds { offset, len, size } => write!(
                f,
                "out of bounds: offset {offset} + len {len} exceeds size {size}"
            ),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Failed(f) => f.source(),
            ReadError::Transport(e) => Some(e.as_ref()),
            ReadError::OutOfBounds { .. } => None,
        }
    }
}

// ── Structured failure ────────────────────────────────────────────────────────

/// Full context for a structured I/O failure.
///
/// Carries enough information to:
/// - Build a human-readable message without string matching.
/// - Route to the correct [`StateError`](crate::archive_error) variant.
/// - Log with meaningful source and operation context.
#[derive(Debug)]
pub struct ReadFailure {
    /// Where the bytes were being read from.
    pub source: SourceInfo,
    /// What operation was being performed when the failure occurred.
    pub operation: ReadOperation,
    /// The specific kind of failure.
    pub kind: ReadErrorKind,
    /// Internal diagnostic string (for logging, not user-facing).
    pub diagnostic: String,
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failed during {:?}: {:?} — {}",
            self.source, self.operation, self.kind, self.diagnostic
        )
    }
}

impl std::error::Error for ReadFailure {}

// ── Source info ───────────────────────────────────────────────────────────────

/// Where a [`ReadAt`](super::ReadAt) is reading from.
///
/// Used in error messages and for routing to user-friendly descriptions.
#[derive(Debug, Clone)]
pub enum SourceInfo {
    /// A file on the local filesystem.
    LocalFile { path: PathBuf },
    /// A remote resource accessed via HTTP(S).
    Http { url: String },
}

impl fmt::Display for SourceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceInfo::LocalFile { path } => write!(f, "local file '{}'", path.display()),
            SourceInfo::Http { url } => write!(f, "HTTP '{url}'"),
        }
    }
}

// ── Operation ─────────────────────────────────────────────────────────────────

/// The I/O operation that was being performed when a failure occurred.
#[derive(Debug, Clone)]
pub enum ReadOperation {
    /// Opening the source (file open, initial HTTP connection).
    Open,
    /// Checking the total size of the source.
    SizeCheck,
    /// Reading a specific byte range.
    ReadRange { offset: u64, len: u64 },
}

// ── Error kind ────────────────────────────────────────────────────────────────

/// The specific kind of I/O failure, used for routing to user-facing messages.
///
/// Each variant carries enough structured context that the state layer can
/// build a clear user message without string matching on `diagnostic`.
#[derive(Debug)]
pub enum ReadErrorKind {
    /// Network connection timed out.
    NetworkTimeout { phase: NetworkPhase },
    /// Network connection was reset (e.g. ECONNRESET, EPIPE).
    NetworkReset,
    /// TLS handshake or certificate validation failed.
    TlsFailure { reason: String },
    /// DNS resolution failed for the hostname.
    DnsFailure { hostname: String },
    /// Server responded with an unexpected HTTP status code.
    HttpStatus {
        code: u16,
        headers: Vec<(String, String)>,
        body: Option<Bytes>,
    },
    /// Server does not support HTTP Range requests (no `206 Partial Content`).
    RangeNotSupported,
    /// Permission denied to read the source (EACCES, EPERM, 403).
    PermissionDenied,
    /// The source file or URL was not found (ENOENT, 404).
    FileNotFound,
    /// Generic disk I/O error.
    DiskIoError { os_code: Option<i32> },
    /// File is locked by another process.
    FileLocked,
    /// Read was interrupted (EINTR) unexpectedly.
    Interrupted,
}

// ── Network phase ─────────────────────────────────────────────────────────────

/// The network phase during which a timeout occurred.
#[derive(Debug, Clone, Copy)]
pub enum NetworkPhase {
    /// TCP connection establishment.
    Connect,
    /// TLS handshake.
    Tls,
    /// Data transfer.
    Transfer,
}
