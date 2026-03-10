use bytes::Bytes;

use crate::io::ReadError;

/// The core I/O abstraction for Archive Sniper.  **[LOCKED]**
///
/// Every archive parser consumes a `ReadAt`. The implementor decides
/// where the bytes actually come from — local disk, HTTP range requests,
/// an in-memory buffer, etc. The parser never knows or cares.
///
/// Returns [`ReadError`] — the I/O layer's own error type. Format parsers
/// convert to [`ArchiveError`](crate::ArchiveError) at their call boundaries
/// (typically with `.map_err(ArchiveError::Io)?`).
pub trait ReadAt: Send + Sync {
    /// Read `len` bytes starting at byte `offset`.
    fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Bytes, ReadError>> + Send;

    /// Total size of the underlying source in bytes.
    ///
    /// Required because some formats (e.g. ZIP) store metadata at the
    /// *end* of the file, so the parser needs to seek backwards from EOF.
    fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send;
}
