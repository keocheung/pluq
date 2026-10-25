use archive::{Archive, ArchiveConfig, ArchiveError};
use archive_core::ProgressReporter;
use archive_core::{ArchiveFingerprint, ReadAt};
use archive_source::{ResolvedSource, SourceConfig};
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

use crate::cache::CacheData;
use crate::reader::{MetadataBlock, MetadataReader};
use crate::CachedArchive;

async fn restore<R: ReadAt + archive_core::LocalPath + 'static>(
    data: CacheData,
    reader: &MetadataReader<R>,
    config: &ArchiveConfig,
) -> Option<Archive<MetadataReader<R>>> {
    match data {
        CacheData::ZipMetadata { blocks } => {
            reader.replay(blocks).ok()?;
            let result = Archive::open_reader_uncached(reader.clone(), config).await;
            reader.finish();
            match result {
                Ok(archive::Opened::Ready(archive)) if archive.is_zip() => Some(archive),
                _ => None,
            }
        }
        CacheData::Parsed {
            entries,
            format_state,
        } => {
            // Parsed ZIP snapshots must never bypass the current decoder.
            if matches!(format_state, archive_core::FormatCacheState::Zip { .. }) {
                return None;
            }
            Some(Archive::from_cache(
                format_state,
                entries,
                reader.clone(),
                config.clone(),
            ))
        }
    }
}

pub(crate) fn save_cache(
    archive: &CachedArchive,
    blocks: Option<Vec<MetadataBlock>>,
    fingerprint: &ArchiveFingerprint,
    path: &std::path::Path,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let data = if archive.is_zip() {
        let Some(blocks) = blocks else { return Ok(()) };
        CacheData::ZipMetadata { blocks }
    } else {
        CacheData::Parsed {
            entries: archive.entries().to_vec(),
            format_state: archive.format_cache_state(),
        }
    };
    let cache = crate::ArchiveCache {
        schema_version: crate::SCHEMA_VERSION,
        fingerprint: fingerprint.clone(),
        data,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    cache.save(path)
}

pub enum Opened {
    Ready(CachedArchive),
    NeedsScan(PendingArchive),
}

impl Opened {
    pub async fn ready(self) -> Result<CachedArchive, ArchiveError> {
        match self {
            Opened::Ready(a) => Ok(a),
            Opened::NeedsScan(p) => p.scan(Arc::new(Mutex::new(()))).await,
        }
    }
}

pub struct PendingArchive {
    inner: archive::PendingArchive<MetadataReader<ResolvedSource>>,
    fingerprint: ArchiveFingerprint,
    source: String,
}

impl PendingArchive {
    pub fn scan_info(&self) -> &archive_core::ScanInfo {
        self.inner.scan_info()
    }

    pub async fn scan(
        self,
        progress: Arc<Mutex<dyn ProgressReporter>>,
    ) -> Result<CachedArchive, ArchiveError> {
        let archive = self.inner.scan(progress).await?;
        if let Some(cache_path) = crate::cache_path_for(&self.source) {
            let _ = save_cache(&archive, None, &self.fingerprint, &cache_path);
        }
        Ok(archive)
    }

    pub async fn scan_and_extract<Filter, SinkFn>(
        self,
        filter: Filter,
        sink_factory: SinkFn,
        progress: Arc<Mutex<dyn ProgressReporter>>,
        password: Option<&[u8]>,
    ) -> Result<(CachedArchive, archive_core::BatchResult), ArchiveError>
    where
        Filter: Fn(&archive_core::ArchiveEntry) -> bool + Send + Sync + 'static,
        SinkFn: FnMut(&archive_core::ArchiveEntry) -> Option<Box<dyn std::io::Write + Send>>
            + Send
            + 'static,
    {
        let (archive, batch) = self
            .inner
            .scan_and_extract(filter, sink_factory, progress, password)
            .await?;
        if let Some(cache_path) = crate::cache_path_for(&self.source) {
            let _ = save_cache(&archive, None, &self.fingerprint, &cache_path);
        }
        Ok((archive, batch))
    }
}

pub async fn open(
    source: &str,
    config: &SourceConfig,
    archive_config: ArchiveConfig,
) -> Result<Opened, ArchiveError> {
    let resolved =
        archive_source::resolve(source, config).map_err(|e| ArchiveError::ConnectionFailed {
            detail: format!("source resolution: {e}"),
        })?;

    let source_size = resolved
        .size()
        .await
        .map_err(|e| archive::translate_core_error(archive_core::CoreError::from(e)))?;

    let mut fingerprint = resolved.fingerprint();
    fingerprint.source_size = source_size;
    debug!(
        source,
        size = fingerprint.source_size,
        etag = ?fingerprint.etag,
        last_modified = ?fingerprint.last_modified,
        "resolved source fingerprint"
    );

    let reader = MetadataReader::new(resolved, fingerprint.source_size);
    // ZIP hits replay metadata through the current parser; RAR hits restore scan results.
    if let Some(cache_path) = crate::cache_path_for(source) {
        match crate::ArchiveCache::load(&cache_path) {
            Ok(cache) => match cache.invalidity_reason(&fingerprint) {
                None => {
                    if let Some(archive) = restore(cache.data, &reader, &archive_config).await {
                        info!(source, "archive cache HIT — restored from cached metadata");
                        return Ok(Opened::Ready(archive));
                    }
                    info!(source, "archive cache INVALID — cached metadata could not be parsed; retrying source");
                }
                Some(reason) => {
                    info!(
                        source,
                        cache_path = %cache_path.display(),
                        reason,
                        "archive cache INVALID — re-parsing from source (fingerprint mismatch)"
                    );
                }
            },
            Err(e) => {
                debug!(
                    source,
                    cache_path = %cache_path.display(),
                    error = %e,
                    "archive cache MISS — absent/unreadable, parsing from source"
                );
            }
        }
    } else {
        debug!(source, "no cache path for source — parsing from source");
    }

    info!(
        source,
        "opening archive via uncached parse (reading central directory from the source)"
    );
    // Central directory + EOCD search window + ZIP64 records + format sniff.
    // Stop capture if a future parser needs more; never persist a partial snapshot.
    reader.record(
        archive_config
            .limits
            .max_header_bytes
            .saturating_add(256 * 1024),
    );
    let result = archive::Archive::open_reader_uncached(reader.clone(), &archive_config).await;
    let blocks = reader.finish();
    let opened = result?;

    match opened {
        archive::Opened::Ready(archive) => {
            if let Some(cache_path) = crate::cache_path_for(source) {
                let _ = save_cache(&archive, blocks, &fingerprint, &cache_path);
            }
            Ok(Opened::Ready(archive))
        }
        archive::Opened::NeedsScan(inner) => Ok(Opened::NeedsScan(PendingArchive {
            inner,
            fingerprint,
            source: source.to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_core::{LocalPath, ReadError};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncRead;

    struct Source {
        bytes: Bytes,
        reads: Arc<AtomicUsize>,
    }
    impl LocalPath for Source {}
    impl ReadAt for Source {
        async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.bytes.slice(offset as usize..(offset + len) as usize))
        }
        async fn read_stream(
            &self,
            offset: u64,
            len: u64,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
            Ok(Box::new(std::io::Cursor::new(
                self.read_range(offset, len).await?,
            )))
        }
        async fn size(&self) -> Result<u64, ReadError> {
            Ok(self.bytes.len() as u64)
        }
    }

    // A stored entry with an unflagged CP932 filename, optionally ZIP64 EOCD.
    fn zip_bytes(zip64: bool) -> Vec<u8> {
        let name = b"\x82\xa9\x82\xc2\x82\xc4\x97\x45\x8e\xd2\x82\xbe\x82\xc1\x82\xbd\x8d\xc8\x92\x42\x82\xd6.txt";
        let payload = b"payload";
        let crc = crc32fast::hash(payload);
        let mut bytes = vec![0; 30];
        bytes[..4].copy_from_slice(b"PK\x03\x04");
        bytes[14..18].copy_from_slice(&crc.to_le_bytes());
        bytes[18..22].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[22..26].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(payload);
        // Keep the payload away from the EOCD search window to test sparse snapshots.
        bytes.resize(100_000, 0);
        let cd_offset = bytes.len();
        let mut cd = vec![0; 46];
        cd[..4].copy_from_slice(b"PK\x01\x02");
        cd[16..20].copy_from_slice(&crc.to_le_bytes());
        cd[20..24].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        cd[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        cd[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(name);
        bytes.extend_from_slice(&cd);
        if zip64 {
            let zip64_offset = bytes.len() as u64;
            let mut record = [0; 56];
            record[..4].copy_from_slice(b"PK\x06\x06");
            record[4..12].copy_from_slice(&44u64.to_le_bytes());
            record[24..32].copy_from_slice(&1u64.to_le_bytes());
            record[32..40].copy_from_slice(&1u64.to_le_bytes());
            record[40..48].copy_from_slice(&(cd.len() as u64).to_le_bytes());
            record[48..56].copy_from_slice(&(cd_offset as u64).to_le_bytes());
            bytes.extend_from_slice(&record);
            let mut locator = [0; 20];
            locator[..4].copy_from_slice(b"PK\x06\x07");
            locator[8..16].copy_from_slice(&zip64_offset.to_le_bytes());
            locator[16..20].copy_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&locator);
        }
        let mut eocd = [0; 22];
        eocd[..4].copy_from_slice(b"PK\x05\x06");
        let count = if zip64 { u16::MAX } else { 1 };
        eocd[8..10].copy_from_slice(&count.to_le_bytes());
        eocd[10..12].copy_from_slice(&count.to_le_bytes());
        eocd[12..16].copy_from_slice(&(cd.len() as u32).to_le_bytes());
        eocd[16..20].copy_from_slice(&(cd_offset as u32).to_le_bytes());
        bytes.extend_from_slice(&eocd);
        bytes
    }

    async fn snapshot(
        zip64: bool,
    ) -> (MetadataReader<Source>, Arc<AtomicUsize>, Vec<MetadataBlock>) {
        let bytes = Bytes::from(zip_bytes(zip64));
        let size = bytes.len() as u64;
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = MetadataReader::new(
            Source {
                bytes,
                reads: reads.clone(),
            },
            size,
        );
        reader.record(1024 * 1024);
        let archive = Archive::open_reader_uncached(reader.clone(), &ArchiveConfig::default())
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();
        assert_eq!(archive.entries()[0].name, "かつて勇者だった妻達へ.txt");
        let blocks = reader.finish().unwrap();
        reads.store(0, Ordering::SeqCst);
        (reader, reads, blocks)
    }

    #[tokio::test]
    async fn zip_and_zip64_disk_snapshots_reparse_without_source_reads_and_extract() {
        for zip64 in [false, true] {
            let (reader, reads, blocks) = snapshot(zip64).await;
            let cache = crate::ArchiveCache {
                schema_version: crate::SCHEMA_VERSION,
                fingerprint: ArchiveFingerprint {
                    source: "sample.zip".into(),
                    source_size: reader.size().await.unwrap(),
                    etag: None,
                    last_modified: None,
                    mtime: None,
                },
                data: CacheData::ZipMetadata { blocks },
                created_at: 0,
            };
            let tmp = tempfile::NamedTempFile::new().unwrap();
            cache.save(tmp.path()).unwrap();
            let json = std::fs::read_to_string(tmp.path()).unwrap();
            assert!(!json.contains("entries"));
            assert!(!json.contains("format_state"));
            let loaded = crate::ArchiveCache::load(tmp.path()).unwrap();
            let archive = restore(loaded.data, &reader, &ArchiveConfig::default())
                .await
                .unwrap();
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            assert_eq!(archive.entries()[0].name, "かつて勇者だった妻達へ.txt");
            let mut out = Vec::new();
            archive
                .extract(
                    &archive.entries()[0],
                    &mut out,
                    Arc::new(Mutex::new(())),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(out, b"payload");
            assert!(reads.load(Ordering::SeqCst) > 0);
            assert!(
                reader.finish().is_none(),
                "extraction must not capture payload bytes"
            );
        }
    }

    #[tokio::test]
    async fn replay_uses_current_limits_and_rejects_incomplete_or_corrupt_snapshots() {
        let (reader, reads, blocks) = snapshot(false).await;
        let mut limited = ArchiveConfig::default();
        limited.limits.max_entries = 0;
        assert!(restore(
            CacheData::ZipMetadata {
                blocks: blocks.clone()
            },
            &reader,
            &limited
        )
        .await
        .is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(restore(
            CacheData::ZipMetadata { blocks: vec![] },
            &reader,
            &ArchiveConfig::default()
        )
        .await
        .is_none());
        let invalid = MetadataBlock {
            offset: u64::MAX,
            bytes: vec![1],
        };
        assert!(restore(
            CacheData::ZipMetadata {
                blocks: vec![invalid]
            },
            &reader,
            &ArchiveConfig::default()
        )
        .await
        .is_none());
        let corrupt = blocks
            .into_iter()
            .map(|mut b| {
                b.bytes.fill(0);
                b
            })
            .collect();
        assert!(restore(
            CacheData::ZipMetadata { blocks: corrupt },
            &reader,
            &ArchiveConfig::default()
        )
        .await
        .is_none());
        // A failed replay leaves the reader usable for an uncached retry.
        let archive = Archive::open_reader_uncached(reader, &ArchiveConfig::default())
            .await
            .unwrap()
            .ready()
            .await
            .unwrap();
        assert_eq!(archive.entries().len(), 1);
        assert!(reads.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn capture_budget_discards_partial_snapshot() {
        let (reader, _, _) = snapshot(false).await;
        reader.record(1);
        reader.read_range(0, 8).await.unwrap();
        assert!(reader.finish().is_none());
    }
    #[tokio::test]
    async fn encoding_override_reuses_raw_cache_and_overrides_utf8_flag() {
        let (reader, reads, blocks) = snapshot(false).await;
        let wrong = ArchiveConfig {
            filename_encoding: "windows-1252".parse().unwrap(),
            ..Default::default()
        };
        let archive = restore(
            CacheData::ZipMetadata {
                blocks: blocks.clone(),
            },
            &reader,
            &wrong,
        )
        .await
        .unwrap();
        assert_ne!(archive.entries()[0].name, "かつて勇者だった妻達へ.txt");

        let auto = restore(
            CacheData::ZipMetadata {
                blocks: blocks.clone(),
            },
            &reader,
            &ArchiveConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(auto.entries()[0].name, "かつて勇者だった妻達へ.txt");

        // Simulate an archive falsely declaring its CP932 bytes to be UTF-8.
        let mut flagged = blocks;
        for block in &mut flagged {
            for pos in 0..block.bytes.len().saturating_sub(10) {
                if &block.bytes[pos..pos + 4] == b"PK\x01\x02" {
                    block.bytes[pos + 9] |= 8;
                }
            }
        }
        let forced = ArchiveConfig {
            filename_encoding: "cp932".parse().unwrap(),
            ..Default::default()
        };
        let archive = restore(CacheData::ZipMetadata { blocks: flagged }, &reader, &forced)
            .await
            .unwrap();
        assert_eq!(archive.entries()[0].name, "かつて勇者だった妻達へ.txt");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
}
