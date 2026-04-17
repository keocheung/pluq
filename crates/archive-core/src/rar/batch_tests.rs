#[cfg(test)]
mod tests {
    use crate::archive_reader::{ArchiveReader, BatchResult};
    use crate::io::{ReadAt, ReadError};
    use crate::progress::ProgressReporter;
    use crate::rar::RarArchive;
    use crate::rar::tests::build_minimal_rar5_multi;
    use bytes::Bytes;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncRead, AsyncWrite};

    struct MockCountingReader {
        data: Vec<u8>,
        stream_calls: Arc<AtomicUsize>,
    }

    impl ReadAt for MockCountingReader {
        fn read_range(
            &self,
            offset: u64,
            len: u64,
        ) -> impl Future<Output = Result<Bytes, ReadError>> + Send {
            let start = offset as usize;
            let end = (offset + len) as usize;
            let res = if start < self.data.len() && end <= self.data.len() {
                Ok(Bytes::copy_from_slice(&self.data[start..end]))
            } else {
                Err(ReadError::OutOfBounds {
                    offset,
                    len,
                    size: self.data.len() as u64,
                })
            };
            async move { res }
        }

        fn read_stream(
            &self,
            offset: u64,
            len: u64,
        ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send
        {
            self.stream_calls.fetch_add(1, Ordering::Relaxed);
            let start = offset as usize;
            let end = (offset + len) as usize;
            let res = if start < self.data.len() && end <= self.data.len() {
                let slice = &self.data[start..end];
                Ok(Box::new(std::io::Cursor::new(slice.to_vec()))
                    as Box<dyn AsyncRead + Send + Unpin>)
            } else {
                Err(ReadError::OutOfBounds {
                    offset,
                    len,
                    size: self.data.len() as u64,
                })
            };
            async move { res }
        }

        fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send {
            let s = self.data.len() as u64;
            async move { Ok(s) }
        }
    }

    impl crate::io::LocalPath for MockCountingReader {}

    #[tokio::test]
    async fn extract_selected_coalesces_cloud_requests() {
        // Multi-entry archive.
        let archive_bytes = build_minimal_rar5_multi(&[
            ("file1.txt", b"content1", false),
            ("file2.txt", b"content2", false),
        ]);

        let stream_calls = Arc::new(AtomicUsize::new(0));
        let reader = MockCountingReader {
            data: archive_bytes,
            stream_calls: stream_calls.clone(),
        };

        let mut rar = RarArchive::open(reader).await.unwrap();
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
        rar.scan_metadata(progress.clone()).await.unwrap();

        let entries = rar.entries();
        assert_eq!(entries.len(), 2);

        let selected = vec![&entries[0], &entries[1]];

        // Use a counting sink to verify bytes.
        let batch: BatchResult = rar
            .extract_selected(
                &selected,
                |_| Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>),
                progress,
                None,
            )
            .await
            .unwrap();

        assert_eq!(batch.succeeded, 2);
        assert_eq!(batch.bytes_written, 16); // "content1" + "content2"

        // Verification of O(M) optimization:
        // 1 call for scan_metadata
        // 1 call for coalesced extract_selected
        // Total = 2 (if it were O(N), it would be 3).
        assert_eq!(stream_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn extract_selected_reports_solid_as_failed() {
        let archive_bytes = build_minimal_rar5_multi(&[
            ("plain.txt", b"plain", false),
            ("solid.txt", b"solid", true),
        ]);

        let reader = MockCountingReader {
            data: archive_bytes,
            stream_calls: Arc::new(AtomicUsize::new(0)),
        };

        let mut rar = RarArchive::open(reader).await.unwrap();
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
        rar.scan_metadata(progress.clone()).await.unwrap();

        let entries = rar.entries();
        let selected = vec![&entries[0], &entries[1]];

        let batch: BatchResult = rar
            .extract_selected(
                &selected,
                |_| Some(Box::new(tokio::io::sink()) as Box<dyn AsyncWrite + Send + Unpin>),
                progress,
                None,
            )
            .await
            .unwrap();

        assert_eq!(batch.succeeded, 1);
        assert_eq!(batch.failed.len(), 1);
        assert_eq!(batch.failed[0].0, "solid.txt");
        assert!(batch.failed[0].1.contains("solid"));
    }
}
