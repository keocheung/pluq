use std::path::Path;

use archive_core::fingerprint::ArchiveFingerprint;

use archive_core::{CachingReadAt, HttpReadAt, LocalPath, LocalReadAt, ReadAt, ReadError};
use archive_dropbox::DropboxReadAt;
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
    /// A file on the local filesystem. (Reader, Original Path)
    Local(LocalReadAt, String),
    /// A file accessible via HTTP(S) range requests. (Reader, Original URL)
    Http(CachingReadAt<HttpReadAt>, String),
    /// A Google Drive file accessed via the Drive API. (Reader, Original URL, File ID)
    GDrive(CachingReadAt<GoogleDriveReadAt>, String, String),
    /// A Dropbox shared file accessed via the Dropbox API. (Reader, Original URL)
    Dropbox(CachingReadAt<DropboxReadAt>, String),
}

impl ResolvedSource {
    /// Suggest a folder name for extraction based on the source metadata.
    pub fn suggested_name(&self) -> Option<String> {
        match self {
            Self::Local(_, path) => crate::suggest_name_from_str(path),
            Self::Http(_, url) => crate::suggest_name_from_str(url),
            Self::GDrive(_, url, _) => crate::suggest_name_from_str(url),
            Self::Dropbox(_, url) => crate::suggest_name_from_str(url),
        }
    }

    pub fn fingerprint(&self) -> ArchiveFingerprint {
        match self {
            Self::Local(_, path) => ArchiveFingerprint {
                source: path.clone(),
                source_size: 0, // filled in by caller after .size().await
                etag: None,
                last_modified: None,
                mtime: std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
            },
            Self::Http(r, url) => {
                let meta = r.inner().captured_metadata().unwrap_or_default();
                ArchiveFingerprint {
                    source: url.clone(),
                    source_size: meta.size.unwrap_or(0),
                    etag: meta.etag,
                    last_modified: meta.last_modified,
                    mtime: None,
                }
            }
            Self::GDrive(r, url, _) => {
                let meta = r.inner().captured_metadata().unwrap_or_default();
                ArchiveFingerprint {
                    source: url.clone(),
                    source_size: meta.size.unwrap_or(0),
                    etag: meta.etag,
                    last_modified: meta.last_modified,
                    mtime: None,
                }
            }
            Self::Dropbox(r, url) => {
                let meta = r.inner().captured_metadata().unwrap_or_default();
                ArchiveFingerprint {
                    source: url.clone(),
                    source_size: meta.size.unwrap_or(0),
                    etag: meta.etag,
                    last_modified: meta.last_modified,
                    mtime: None,
                }
            }
        }
    }
}

impl LocalPath for ResolvedSource {
    fn local_path(&self) -> Option<&Path> {
        match self {
            Self::Local(r, _) => Some(r.path()),
            _ => None,
        }
    }
}

impl ReadAt for ResolvedSource {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        match self {
            Self::Local(r, _) => r.read_range(offset, len).await,
            Self::Http(r, _) => r.read_range(offset, len).await,
            Self::GDrive(r, _, _) => r.read_range(offset, len).await,
            Self::Dropbox(r, _) => r.read_range(offset, len).await,
        }
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        match self {
            Self::Local(r, _) => r.read_stream(offset, len).await,
            Self::Http(r, _) => r.read_stream(offset, len).await,
            Self::GDrive(r, _, _) => r.read_stream(offset, len).await,
            Self::Dropbox(r, _) => r.read_stream(offset, len).await,
        }
    }

    async fn size(&self) -> Result<u64, ReadError> {
        match self {
            Self::Local(r, _) => r.size().await,
            Self::Http(r, _) => r.size().await,
            Self::GDrive(r, _, _) => r.size().await,
            Self::Dropbox(r, _) => r.size().await,
        }
    }
}
