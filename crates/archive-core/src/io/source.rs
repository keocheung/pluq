//! I/O source abstraction for selecting between local, HTTP, and provider backends.
//!
//! This enum provides static dispatch (no `dyn`) over the available
//! `ReadAt` implementations.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use bytes::Bytes;
use tokio::io::AsyncRead;
use tracing::info;

use super::{HttpReadAt, LocalReadAt, ReadAt};
use crate::io::ReadError;

type StreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send + 'a>>;

/// Object-safe async trait used to store a type-erased [`ReadAt`] in
/// [`Source::Provider`].
///
/// Exists because `ReadAt` uses RPITIT (`impl Future`) return types and is
/// therefore not object-safe. This trait bridges the gap with explicit
/// `Pin<Box<dyn Future>>` returns so that `Box<dyn DynReadAt>` compiles.
trait DynReadAt: Send + Sync {
    fn read_range_dyn<'a>(
        &'a self,
        offset: u64,
        len: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Bytes, ReadError>> + Send + 'a>>;

    fn read_stream_dyn<'a>(&'a self, offset: u64, len: u64) -> StreamFuture<'a>;

    fn size_dyn<'a>(&'a self)
    -> Pin<Box<dyn Future<Output = Result<u64, ReadError>> + Send + 'a>>;
}

impl<T: ReadAt> DynReadAt for T {
    fn read_range_dyn<'a>(
        &'a self,
        offset: u64,
        len: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Bytes, ReadError>> + Send + 'a>> {
        Box::pin(self.read_range(offset, len))
    }

    fn read_stream_dyn<'a>(&'a self, offset: u64, len: u64) -> StreamFuture<'a> {
        Box::pin(self.read_stream(offset, len))
    }

    fn size_dyn<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, ReadError>> + Send + 'a>> {
        Box::pin(self.size())
    }
}

/// An I/O source: local file, HTTP URL, or provider-supplied reader.
///
/// Created via [`Source::detect`] from a path or HTTP URL string, or via
/// [`Source::provider`] when injecting a custom reader from a product layer.
// `DynReadAt` is intentionally private — `Provider` is constructed only through
// `Source::provider()`, so external callers never need to name the type.
#[allow(private_interfaces)]
pub enum Source {
    /// A file on the local filesystem.
    Local(LocalReadAt),
    /// A file accessible via HTTP(S) range requests.
    Http(HttpReadAt),
    /// A provider-supplied ReadAt (Google Drive, Dropbox, etc.).
    /// Constructed by product layers (CLI, server) and passed into Archive.
    Provider(Box<dyn DynReadAt>),
}

impl Source {
    /// Returns the local file path if this source is a local file, `None` otherwise.
    ///
    /// Used by the RAR extraction pipeline to decide between the local shortcut
    /// (pass the path directly to unrar) and the cloud synthetic-archive approach.
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Source::Local(r) => Some(&r.path),
            Source::Http(_) | Source::Provider(_) => None,
        }
    }

    /// Create a `Source` by auto-detecting from a string.
    ///
    /// - Strings starting with `http://` or `https://` become `Source::Http`.
    /// - Everything else is treated as a local file path.
    pub fn detect(source: &str) -> Self {
        if source.starts_with("http://") || source.starts_with("https://") {
            info!(source = %source, "source detected as HTTP");
            let client = reqwest::Client::builder()
                .user_agent("archive-sniper")
                .build()
                .expect("failed to create HTTP client");
            Source::Http(HttpReadAt::new(source, client))
        } else {
            info!(source = %source, "source detected as local file");
            Source::Local(LocalReadAt::new(source))
        }
    }

    /// Wrap a custom [`ReadAt`] implementation as a `Source`.
    ///
    /// Use this when a product layer (CLI, server) needs to inject a decorated
    /// reader (e.g. a progress-reporting wrapper, Google Drive, Dropbox).
    pub fn provider<T: ReadAt + 'static>(reader: T) -> Self {
        Source::Provider(Box::new(reader))
    }
}

impl ReadAt for Source {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        match self {
            Source::Local(r) => r.read_range(offset, len).await,
            Source::Http(r) => r.read_range(offset, len).await,
            Source::Provider(r) => r.read_range_dyn(offset, len).await,
        }
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        match self {
            Source::Local(r) => r.read_stream(offset, len).await,
            Source::Http(r) => r.read_stream(offset, len).await,
            Source::Provider(r) => r.read_stream_dyn(offset, len).await,
        }
    }

    async fn size(&self) -> Result<u64, ReadError> {
        match self {
            Source::Local(r) => r.size().await,
            Source::Http(r) => r.size().await,
            Source::Provider(r) => r.size_dyn().await,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_local_path_returns_path_for_local() {
        let source = Source::detect("/tmp/test.rar");
        assert!(source.local_path().is_some());
    }

    #[test]
    fn source_local_path_returns_none_for_http() {
        let source = Source::detect("https://example.com/test.rar");
        assert!(source.local_path().is_none());
    }
}
