//! I/O source abstraction for selecting between local and HTTP backends.
//!
//! This enum provides static dispatch (no `dyn`) over the available
//! `ReadAt` implementations.

use std::path::Path;

use bytes::Bytes;
use tokio::io::AsyncRead;

use super::{HttpReadAt, LocalReadAt, ReadAt};
use crate::io::ReadError;

/// An I/O source that can be either a local file or an HTTP URL.
///
/// Created automatically by [`ArchiveCore::open`](crate::ArchiveCore::open) based
/// on the source string, or manually for fine-grained control.
pub enum Source {
    /// A file on the local filesystem.
    Local(LocalReadAt),
    /// A file accessible via HTTP(S) range requests.
    Http(HttpReadAt),
}

impl Source {
    /// Returns the local file path if this source is a local file, `None` for HTTP.
    ///
    /// Used by the RAR extraction pipeline to decide between the local shortcut
    /// (pass the path directly to unrar) and the cloud synthetic-archive approach.
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Source::Local(r) => Some(&r.path),
            Source::Http(_) => None,
        }
    }

    /// Create a `Source` by auto-detecting from a string.
    ///
    /// - Strings starting with `http://` or `https://` become `Source::Http`.
    /// - Everything else is treated as a local file path.
    pub fn detect(source: &str) -> Self {
        if source.starts_with("http://") || source.starts_with("https://") {
            let client = reqwest::Client::builder()
                .user_agent("archive-sniper")
                .build()
                .expect("failed to create HTTP client");
            Source::Http(HttpReadAt::new(source, client))
        } else {
            Source::Local(LocalReadAt::new(source))
        }
    }
}

impl ReadAt for Source {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        match self {
            Source::Local(r) => r.read_range(offset, len).await,
            Source::Http(r) => r.read_range(offset, len).await,
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
        }
    }

    async fn size(&self) -> Result<u64, ReadError> {
        match self {
            Source::Local(r) => r.size().await,
            Source::Http(r) => r.size().await,
        }
    }
}
