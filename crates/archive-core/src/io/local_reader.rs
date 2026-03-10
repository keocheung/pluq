use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::ReadAt;
use super::read_error::{ReadErrorKind, ReadFailure, ReadOperation, SourceInfo};
use crate::io::ReadError;

/// A [`ReadAt`] backed by a local file on disk.
///
/// Each call to [`read_range`](ReadAt::read_range) opens, seeks, and reads
/// independently — no long-lived file handle is held.
pub struct LocalReadAt {
    path: PathBuf,
}

impl LocalReadAt {
    /// Create a new `LocalReadAt` for the file at `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

/// Convert a `std::io::Error` from a local file operation into a [`ReadError`].
fn io_err_to_read(e: std::io::Error, path: &Path, op: ReadOperation) -> ReadError {
    ReadError::Failed(Box::new(ReadFailure {
        source: SourceInfo::LocalFile {
            path: path.to_path_buf(),
        },
        operation: op,
        kind: ReadErrorKind::DiskIoError {
            os_code: e.raw_os_error(),
        },
        diagnostic: e.to_string(),
    }))
}

impl ReadAt for LocalReadAt {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        let mut file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|e| io_err_to_read(e, &self.path, ReadOperation::Open))?;

        let file_size = file
            .metadata()
            .await
            .map_err(|e| io_err_to_read(e, &self.path, ReadOperation::SizeCheck))?
            .len();

        if offset.saturating_add(len) > file_size {
            return Err(ReadError::OutOfBounds {
                offset,
                len,
                size: file_size,
            });
        }

        if len == 0 {
            return Ok(Bytes::new());
        }

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| io_err_to_read(e, &self.path, ReadOperation::ReadRange { offset, len }))?;

        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)
            .await
            .map_err(|e| io_err_to_read(e, &self.path, ReadOperation::ReadRange { offset, len }))?;

        Ok(Bytes::from(buf))
    }

    async fn size(&self) -> Result<u64, ReadError> {
        let meta = tokio::fs::metadata(&self.path)
            .await
            .map_err(|e| io_err_to_read(e, &self.path, ReadOperation::SizeCheck))?;
        Ok(meta.len())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    /// Helper: create a temp file with known content.
    fn temp_file(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn read_full_contents() {
        let data = b"hello archive sniper";
        let f = temp_file(data);
        let reader = LocalReadAt::new(f.path());

        let result = reader.read_range(0, data.len() as u64).await.unwrap();
        assert_eq!(result.as_ref(), data);
    }

    #[tokio::test]
    async fn read_with_offset() {
        let data = b"hello archive sniper";
        let f = temp_file(data);
        let reader = LocalReadAt::new(f.path());

        // Read "archive" starting at offset 6
        let result = reader.read_range(6, 7).await.unwrap();
        assert_eq!(result.as_ref(), b"archive");
    }

    #[tokio::test]
    async fn read_zero_bytes() {
        let f = temp_file(b"something");
        let reader = LocalReadAt::new(f.path());

        let result = reader.read_range(0, 0).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn out_of_bounds_returns_error() {
        let data = b"short";
        let f = temp_file(data);
        let reader = LocalReadAt::new(f.path());

        let err = reader
            .read_range(0, data.len() as u64 + 1)
            .await
            .unwrap_err();

        assert!(
            matches!(err, ReadError::OutOfBounds { .. }),
            "expected OutOfBounds, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn size_returns_file_length() {
        let data = b"0123456789";
        let f = temp_file(data);
        let reader = LocalReadAt::new(f.path());

        assert_eq!(reader.size().await.unwrap(), 10);
    }
}
