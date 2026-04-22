//! Batch-extraction integration tests for RAR4.
//!
//! Uses a synthetic 2-entry Store archive built in memory.
//! `CountingReadAt` returns `LocalPath::local_path() == None` which
//! forces the cloud batch path in `Rar4Archive::extract_selected`.

#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Mutex};

use archive_core::{ArchiveEntry, ArchiveReader, Rar4Archive};

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

fn build_rar4_store_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    const RAR4_SIG: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];

    let mut main_hdr = [0u8; 13];
    main_hdr[2] = 0x73;
    main_hdr[5] = 13;
    let crc = crc16_arc(&main_hdr[2..13]);
    main_hdr[0] = crc as u8;
    main_hdr[1] = (crc >> 8) as u8;

    let mut eoa = [0u8; 7];
    eoa[2] = 0x7B;
    eoa[4] = 0x40;
    eoa[5] = 7;
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
        let head_flags: u16 = 0x8000;

        let mut hdr = vec![0u8; head_size as usize];
        hdr[2] = 0x74;
        hdr[3] = head_flags as u8;
        hdr[4] = (head_flags >> 8) as u8;
        hdr[5] = head_size as u8;
        hdr[6] = (head_size >> 8) as u8;
        hdr[7..11].copy_from_slice(&pack_size.to_le_bytes());
        hdr[11..15].copy_from_slice(&unp_size.to_le_bytes());
        hdr[15] = 3;
        hdr[16..20].copy_from_slice(&file_crc.to_le_bytes());
        let ftime: u32 = 0x46C3_8120;
        hdr[20..24].copy_from_slice(&ftime.to_le_bytes());
        hdr[24] = 0x14;
        hdr[25] = 0x30;
        hdr[26] = name_len as u8;
        hdr[27] = (name_len >> 8) as u8;
        hdr[28] = 0x20;
        hdr[32..32 + name_len].copy_from_slice(name_bytes);

        let crc = crc16_arc(&hdr[2..head_size as usize]);
        hdr[0] = crc as u8;
        hdr[1] = (crc >> 8) as u8;

        archive.extend_from_slice(&hdr);
        archive.extend_from_slice(data);
    }

    archive.extend_from_slice(&eoa);
    archive
}

fn write_tempfile(bytes: &[u8]) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let f = tempfile::NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();
    std::fs::write(&path, bytes).unwrap();
    (f, path)
}

#[tokio::test]
async fn rar4_extract_selected_batches_into_single_cluster() {
    let file1 = b"Hello from file one!";
    let file2 = b"Second file content!";
    let bytes = build_rar4_store_archive(&[("file1.txt", file1), ("file2.txt", file2)]);
    let (_tmp, path) = write_tempfile(&bytes);

    let reader = common::CountingReadAt::new(&path);
    let stream_calls = reader.stream_calls.clone();

    let mut archive = Rar4Archive::open(reader, &archive_core::ResourceLimits::default())
        .await
        .expect("open RAR4");

    let progress: Arc<Mutex<dyn archive_core::ProgressReporter>> = Arc::new(Mutex::new(()));
    archive
        .scan_metadata(progress.clone(), &archive_core::ResourceLimits::default())
        .await
        .expect("scan_metadata");

    let entries: Vec<&ArchiveEntry> = archive
        .entries()
        .iter()
        .filter(|e| !e.is_directory)
        .collect();
    assert_eq!(entries.len(), 2, "fixture must have exactly 2 file entries");

    let expected_bytes: u64 = entries.iter().map(|e| e.uncompressed_size).sum();

    let calls_before = stream_calls.load(std::sync::atomic::Ordering::SeqCst);

    let result = archive
        .extract_selected(
            &entries,
            |_| Some(Box::new(tokio::io::sink()) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>),
            progress,
            None,
            &archive_core::ResourceLimits::default(),
        )
        .await
        .expect("extract_selected failed");

    let calls_after = stream_calls.load(std::sync::atomic::Ordering::SeqCst);
    let delta = calls_after - calls_before;

    assert_eq!(result.succeeded, 2, "both entries must succeed");
    assert_eq!(
        result.bytes_written, expected_bytes,
        "bytes_written must equal sum of uncompressed sizes"
    );
    assert!(
        delta < entries.len(),
        "expected fewer read_stream calls than entries after scan (got {delta} for {} entries — batching not active?)",
        entries.len()
    );
}
