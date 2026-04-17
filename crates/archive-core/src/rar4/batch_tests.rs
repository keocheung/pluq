#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use tokio::io::{AsyncRead, AsyncWrite};

    use crate::archive_reader::{ArchiveReader, BatchResult};
    use crate::io::{LocalPath, ReadAt, ReadError};
    use crate::progress::ProgressReporter;
    use crate::rar4::Rar4Archive;

    // ── MockCountingReader ─────────────────────────────────────────────────────

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
            let res = if start <= self.data.len() && end <= self.data.len() {
                Ok(Bytes::copy_from_slice(&self.data[start..end]))
            } else {
                Err(ReadError::OutOfBounds { offset, len, size: self.data.len() as u64 })
            };
            async move { res }
        }

        fn read_stream(
            &self,
            offset: u64,
            len: u64,
        ) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send {
            self.stream_calls.fetch_add(1, Ordering::Relaxed);
            let start = offset as usize;
            let end = (offset + len) as usize;
            let res = if start <= self.data.len() && end <= self.data.len() {
                Ok(Box::new(std::io::Cursor::new(self.data[start..end].to_vec()))
                    as Box<dyn AsyncRead + Send + Unpin>)
            } else {
                Err(ReadError::OutOfBounds { offset, len, size: self.data.len() as u64 })
            };
            async move { res }
        }

        fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send {
            let s = self.data.len() as u64;
            async move { Ok(s) }
        }
    }

    impl LocalPath for MockCountingReader {}

    // ── RAR4 Store archive builder ─────────────────────────────────────────────

    /// CRC-16/ARC (polynomial 0xA001, reflected) — same algorithm as RAR4 block headers.
    fn crc16_arc(data: &[u8]) -> u16 {
        let mut crc: u16 = 0;
        for &byte in data {
            crc ^= byte as u16;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xA001;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc
    }

    /// Build a minimal RAR4 archive with multiple Store (method 0x30) entries.
    fn build_minimal_rar4_multi(entries: &[(&str, &[u8])]) -> Vec<u8> {
        const RAR4_SIG: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];

        // Main archive header: HEAD_CRC(2) + HEAD_TYPE(1) + HEAD_FLAGS(2) + HEAD_SIZE(2) + reserved(6)
        let mut main_hdr = [0u8; 13];
        main_hdr[2] = 0x73; // HEAD_TYPE
        main_hdr[5] = 13;   // HEAD_SIZE lo
        let crc = crc16_arc(&main_hdr[2..13]);
        main_hdr[0] = crc as u8;
        main_hdr[1] = (crc >> 8) as u8;

        // End-of-archive block: HEAD_CRC(2) + HEAD_TYPE(1) + HEAD_FLAGS(2) + HEAD_SIZE(2)
        let mut eoa = [0u8; 7];
        eoa[2] = 0x7B; // HEAD_TYPE
        eoa[4] = 0x40; // HEAD_FLAGS hi (0x4000)
        eoa[5] = 7;    // HEAD_SIZE lo
        let crc = crc16_arc(&eoa[2..7]);
        eoa[0] = crc as u8;
        eoa[1] = (crc >> 8) as u8;

        let mut archive = RAR4_SIG.to_vec();
        archive.extend_from_slice(&main_hdr);

        for (name, data) in entries {
            let name_bytes = name.as_bytes();
            let name_len = name_bytes.len();
            let pack_size = data.len() as u32;
            let unp_size = data.len() as u32;
            let file_crc = crc32fast::hash(data);
            let head_size = (32 + name_len) as u16;
            // FLAG_LONG_BLOCK (0x8000): parse_block_header reads PACK_SIZE as add_size.
            let head_flags: u16 = 0x8000;

            let mut hdr = vec![0u8; head_size as usize];
            hdr[2] = 0x74;                                       // HEAD_TYPE
            hdr[3] = head_flags as u8;                           // HEAD_FLAGS lo
            hdr[4] = (head_flags >> 8) as u8;                    // HEAD_FLAGS hi
            hdr[5] = head_size as u8;                            // HEAD_SIZE lo
            hdr[6] = (head_size >> 8) as u8;                     // HEAD_SIZE hi
            hdr[7..11].copy_from_slice(&pack_size.to_le_bytes()); // PACK_SIZE
            hdr[11..15].copy_from_slice(&unp_size.to_le_bytes()); // UNP_SIZE
            hdr[15] = 3;                                         // HOST_OS = Unix
            hdr[16..20].copy_from_slice(&file_crc.to_le_bytes()); // FILE_CRC
            let ftime: u32 = 0x46C3_8120;
            hdr[20..24].copy_from_slice(&ftime.to_le_bytes());   // FTIME
            hdr[24] = 0x14;                                      // UNP_VER = 2.0
            hdr[25] = 0x30;                                      // METHOD = Store
            hdr[26] = name_len as u8;                            // NAME_SIZE lo
            hdr[27] = (name_len >> 8) as u8;                     // NAME_SIZE hi
            hdr[28] = 0x20;                                      // ATTR (Archive bit)
            hdr[32..32 + name_len].copy_from_slice(name_bytes);  // filename

            let crc = crc16_arc(&hdr[2..head_size as usize]);
            hdr[0] = crc as u8;
            hdr[1] = (crc >> 8) as u8;

            archive.extend_from_slice(&hdr);
            archive.extend_from_slice(data);
        }

        archive.extend_from_slice(&eoa);
        archive
    }

    // ── Tests ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn extract_selected_coalesces_cloud_requests() {
        let archive_bytes = build_minimal_rar4_multi(&[
            ("file1.txt", b"content1"),
            ("file2.txt", b"content2"),
        ]);

        let stream_calls = Arc::new(AtomicUsize::new(0));
        let reader = MockCountingReader {
            data: archive_bytes,
            stream_calls: stream_calls.clone(),
        };

        let mut rar4 = Rar4Archive::open(reader).await.unwrap();
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
        rar4.scan_metadata(progress.clone()).await.unwrap();

        let entries = rar4.entries();
        assert_eq!(entries.len(), 2);

        let selected = vec![&entries[0], &entries[1]];

        let batch: BatchResult = rar4
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

        // 1 call for scan_metadata + 1 call for coalesced extract_selected = 2 total.
        // O(N) would be 3 (1 scan + 2 per-entry).
        assert_eq!(stream_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn extract_selected_empty_is_noop() {
        let archive_bytes =
            build_minimal_rar4_multi(&[("file1.txt", b"hello")]);

        let reader = MockCountingReader {
            data: archive_bytes,
            stream_calls: Arc::new(AtomicUsize::new(0)),
        };

        let mut rar4 = Rar4Archive::open(reader).await.unwrap();
        let progress: Arc<Mutex<dyn ProgressReporter>> = Arc::new(Mutex::new(()));
        rar4.scan_metadata(progress.clone()).await.unwrap();

        let batch: BatchResult = rar4
            .extract_selected(
                &[],
                |_| None,
                progress,
                None,
            )
            .await
            .unwrap();

        assert_eq!(batch.succeeded, 0);
        assert_eq!(batch.bytes_written, 0);
        assert!(batch.failed.is_empty());
    }
}
