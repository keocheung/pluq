/// `ReadAt` wrapper that counts `read_stream` and `read_range` invocations.
///
/// `LocalPath::local_path()` returns `None` so RAR/RAR4 cloud batch paths
/// activate even when the underlying data is a local file.
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use tokio::io::AsyncRead;

use archive_core::{LocalPath, LocalReadAt, ReadAt, ReadError};

pub struct CountingReadAt {
    path: PathBuf,
    pub stream_calls: Arc<AtomicUsize>,
    pub range_calls: Arc<AtomicUsize>,
}

impl CountingReadAt {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            stream_calls: Arc::new(AtomicUsize::new(0)),
            range_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

}

impl ReadAt for CountingReadAt {
    fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Bytes, ReadError>> + Send {
        self.range_calls.fetch_add(1, Ordering::Relaxed);
        let path = self.path.clone();
        async move {
            let r = LocalReadAt::new(&path);
            r.read_range(offset, len).await
        }
    }

    fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send {
        self.stream_calls.fetch_add(1, Ordering::Relaxed);
        let path = self.path.clone();
        async move {
            let r = LocalReadAt::new(&path);
            r.read_stream(offset, len).await
        }
    }

    fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send {
        let path = self.path.clone();
        async move {
            let r = LocalReadAt::new(&path);
            r.size().await
        }
    }
}

impl LocalPath for CountingReadAt {}
