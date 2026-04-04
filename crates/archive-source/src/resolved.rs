use std::path::Path;

use archive_core::{CachingReadAt, HttpReadAt, LocalPath, LocalReadAt, ReadAt, ReadError};
use archive_gdrive::GoogleDriveReadAt;
use bytes::Bytes;
use tokio::io::AsyncRead;

/// A fully-constructed, ready-to-use reader.
///
/// Each variant is the final reader — remote variants are already wrapped
/// in `CachingReadAt`. Static dispatch, no type erasure.
///
/// Created by [`resolve()`](crate::resolve).
pub enum ResolvedSource {
    /// A file on the local filesystem.
    Local(LocalReadAt),
    /// A file accessible via HTTP(S) range requests.
    Http(CachingReadAt<HttpReadAt>),
    /// A Google Drive file accessed via the Drive API.
    GDrive(CachingReadAt<GoogleDriveReadAt>),
    // Phase 4: Dropbox(CachingReadAt<DropboxReadAt>),
}

impl LocalPath for ResolvedSource {
    fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(r) => Some(r.path()),
            _ => None,
        }
    }
}

impl ReadAt for ResolvedSource {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        match self {
            Self::Local(r) => r.read_range(offset, len).await,
            Self::Http(r) => r.read_range(offset, len).await,
            Self::GDrive(r) => r.read_range(offset, len).await,
        }
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        match self {
            Self::Local(r) => r.read_stream(offset, len).await,
            Self::Http(r) => r.read_stream(offset, len).await,
            Self::GDrive(r) => r.read_stream(offset, len).await,
        }
    }

    async fn size(&self) -> Result<u64, ReadError> {
        match self {
            Self::Local(r) => r.size().await,
            Self::Http(r) => r.size().await,
            Self::GDrive(r) => r.size().await,
        }
    }
}
