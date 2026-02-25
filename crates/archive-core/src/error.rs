use thiserror::Error;

/// Errors produced by archive operations.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Underlying I/O error (file, network, etc.).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Requested byte range exceeds the source size.
    #[error("out of bounds: offset {offset} + len {len} exceeds size {size}")]
    OutOfBounds { offset: u64, len: u64, size: u64 },

    /// Catch-all for format-specific or unexpected errors.
    #[error("{0}")]
    Other(String),
}
