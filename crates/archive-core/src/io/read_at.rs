use bytes::Bytes;
use tokio::io::AsyncRead;

use crate::io::ReadError;

/// The core I/O abstraction for pluq.  **[LOCKED]**
///
/// Every archive parser consumes a `ReadAt`. The implementor decides
/// where the bytes actually come from — local disk, HTTP range requests,
/// an in-memory buffer, etc. The parser never knows or cares.
///
/// Two methods, two jobs:
/// - [`read_range`](Self::read_range) returns `Bytes` — simple, for bounded header/metadata reads.
/// - [`read_stream`](Self::read_stream) returns `AsyncRead` — for large entry data extraction.
///
/// Returns `ReadError`, not `CoreError`. Format parsers convert at their call boundaries.
pub trait ReadAt: Send + Sync {
    /// Read `len` bytes at `offset` into memory. for bounded header/metadata reads.
    ///
    /// Parsers use this for EOCD, Central Directory headers, and Local File Headers.
    /// These are always small (< 1 MiB) and safe to buffer.
    fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Bytes, ReadError>> + Send;

    /// Stream `len` bytes at `offset` as an async reader. For large entry data extraction.
    ///
    /// Used by extraction for potentially large compressed entry data (could be GBs).
    /// Both production impls (`LocalReadAt`, `HttpReadAt`) must implement this — no default.
    fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send;

    /// Total size of the underlying source in bytes.
    ///
    /// Required because some formats (e.g. ZIP) store metadata at the
    /// *end* of the file, so the parser needs to seek backwards from EOF.
    fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send;
}
