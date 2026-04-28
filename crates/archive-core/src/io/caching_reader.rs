use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::AsyncRead;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, trace};

use super::{LocalPath, ReadAt, ReadError};

/// Minimum fetch size: every read to the inner reader is at least this large.
const MIN_FETCH: u64 = 64 * 1024;

/// A caching [`ReadAt`] decorator that coalesces small reads into 64 KiB
/// fetches and serves repeated/overlapping reads from an in-memory cache.
///
/// Designed for HTTP-backed readers where each `read_range` costs a full
/// round trip. Archive metadata parsing typically makes many small reads
/// near the start and end of the file — this decorator collapses those
/// into ~2 fetches of 64 KiB each, with subsequent reads served from cache.
///
/// **`read_stream` is cache-aware but never populates the cache.** If the
/// stream range overlaps with cached blocks, the cached prefix/suffix is
/// served from memory and only the uncached middle is fetched from the
/// inner reader. Large extraction streams are never stored in cache.
pub struct CachingReadAt<T> {
    inner: T,
    cache: Mutex<BlockCache>,
    file_size: OnceCell<u64>,
}

impl<T: ReadAt> LocalPath for CachingReadAt<T> {}

impl<T: ReadAt> CachingReadAt<T> {
    pub fn new(inner: T) -> Self {
        debug!("caching layer active");
        Self {
            inner,
            cache: Mutex::new(BlockCache::new()),
            file_size: OnceCell::new(),
        }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Get (and cache) the file size from the inner reader.
    async fn ensure_size(&self) -> Result<u64, ReadError> {
        self.file_size
            .get_or_try_init(|| self.inner.size())
            .await
            .copied()
    }
}

impl<T: ReadAt> ReadAt for CachingReadAt<T> {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        if len == 0 {
            return Ok(Bytes::new());
        }

        // Fast path: full cache hit (lock held briefly, no I/O).
        {
            let cache = self.cache.lock().await;
            if let Some(data) = cache.try_get(offset, len) {
                debug!(offset, len, "cache hit");
                return Ok(data);
            }
        }

        // Cache miss — determine file size for bounds clamping.
        let file_size = self.ensure_size().await?;

        // Compute what to fetch, avoiding re-download of cached regions.
        let (fetch_offset, fetch_len) = {
            let cache = self.cache.lock().await;
            cache.compute_fetch(offset, len, file_size)
        };

        let saved = len.saturating_sub(fetch_len);
        debug!(
            offset,
            len, fetch_offset, fetch_len, saved, "cache miss, fetching from inner"
        );

        // Fetch from inner reader (no lock held during I/O).
        let fetched = self.inner.read_range(fetch_offset, fetch_len).await?;

        // Store in cache, merge with adjacent blocks, serve the original request.
        let mut cache = self.cache.lock().await;
        cache.insert(fetch_offset, fetched);

        Ok(cache
            .try_get(offset, len)
            .expect("cache must contain requested range after fetch"))
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        if len == 0 {
            return Ok(Box::new(tokio::io::empty()));
        }

        let req_end = offset + len;

        // Check cache for prefix/suffix overlap.
        let (prefix, suffix) = {
            let cache = self.cache.lock().await;
            (
                cache.get_prefix(offset, req_end),
                cache.get_suffix(offset, req_end),
            )
        };

        let prefix_len = prefix.len() as u64;
        let suffix_len = suffix.len() as u64;
        let stream_start = offset + prefix_len;
        let stream_end = req_end - suffix_len;

        debug!(
            offset,
            len,
            prefix_len,
            suffix_len,
            network_len = stream_end.saturating_sub(stream_start),
            "stream: cache/network breakdown"
        );

        let mut readers: VecDeque<Box<dyn AsyncRead + Send + Unpin>> = VecDeque::new();

        if !prefix.is_empty() {
            trace!(offset, len = prefix_len, "stream: serving cached prefix");
            readers.push_back(Box::new(io::Cursor::new(prefix)));
        }

        let stream_len = stream_end.saturating_sub(stream_start);
        if stream_len > 0 {
            trace!(
                offset = stream_start,
                len = stream_len,
                "stream: fetching middle from inner"
            );
            readers.push_back(self.inner.read_stream(stream_start, stream_len).await?);
        }

        if !suffix.is_empty() {
            trace!(
                offset = stream_end,
                len = suffix_len,
                "stream: serving cached suffix"
            );
            readers.push_back(Box::new(io::Cursor::new(suffix)));
        }

        // If only one reader, return it directly (common: no overlap).
        if readers.len() == 1 {
            return Ok(readers.pop_front().unwrap());
        }

        Ok(Box::new(ChainedReader { readers }))
    }

    async fn size(&self) -> Result<u64, ReadError> {
        self.ensure_size().await
    }
}

// ── Block cache ──────────────────────────────────────────────────────────────

struct CacheBlock {
    offset: u64,
    data: Bytes,
}

impl CacheBlock {
    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }

    fn contains(&self, offset: u64, len: u64) -> bool {
        offset >= self.offset && offset + len <= self.end()
    }

    fn slice(&self, offset: u64, len: u64) -> Bytes {
        let start = (offset - self.offset) as usize;
        self.data.slice(start..start + len as usize)
    }
}

struct BlockCache {
    blocks: Vec<CacheBlock>,
}

impl BlockCache {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Try to serve a request entirely from a single cached block.
    fn try_get(&self, offset: u64, len: u64) -> Option<Bytes> {
        self.blocks
            .iter()
            .find(|b| b.contains(offset, len))
            .map(|b| b.slice(offset, len))
    }

    /// Extract cached bytes at the start of [offset, req_end).
    fn get_prefix(&self, offset: u64, req_end: u64) -> Bytes {
        for block in &self.blocks {
            if block.offset <= offset && block.end() > offset {
                let available_end = block.end().min(req_end);
                let prefix_len = available_end - offset;
                return block.slice(offset, prefix_len);
            }
        }
        Bytes::new()
    }

    /// Extract cached bytes at the end of [offset, req_end).
    fn get_suffix(&self, offset: u64, req_end: u64) -> Bytes {
        for block in &self.blocks {
            if block.offset < req_end && block.end() >= req_end && block.offset > offset {
                let suffix_start = block.offset;
                let suffix_len = req_end - suffix_start;
                return block.slice(suffix_start, suffix_len);
            }
        }
        Bytes::new()
    }

    /// Compute the range to fetch from the inner reader.
    ///
    /// Trims the fetch to avoid re-downloading data that is already cached,
    /// and expands small fetches to at least [`MIN_FETCH`] bytes. Expansion
    /// grows away from cached regions so the new data is maximally useful.
    fn compute_fetch(&self, offset: u64, len: u64, file_size: u64) -> (u64, u64) {
        let req_end = (offset + len).min(file_size);

        // Find cache coverage on the left side of our request.
        let left_cache_end = self
            .blocks
            .iter()
            .filter(|b| b.offset <= offset && b.end() > offset)
            .map(|b| b.end())
            .max();

        // Find cache coverage on the right side of our request.
        let right_cache_start = self
            .blocks
            .iter()
            .filter(|b| b.offset < req_end && b.end() >= req_end)
            .map(|b| b.offset)
            .min();

        let gap_start = left_cache_end.unwrap_or(offset);
        let gap_end = right_cache_start.unwrap_or(req_end);

        if gap_start >= gap_end {
            // Fully covered (multi-block) — refetch the request range.
            // After insert+merge this becomes a single block.
            return (offset, req_end - offset);
        }

        let gap_len = gap_end - gap_start;

        // Gap already large enough — fetch exactly the gap.
        if gap_len >= MIN_FETCH {
            return (gap_start, gap_len);
        }

        // Expand to MIN_FETCH, growing away from cached regions.
        match (left_cache_end, right_cache_start) {
            (Some(_), Some(_)) => {
                // Cache on both sides — just fetch the gap as-is, even if < MIN_FETCH.
                (gap_start, gap_len)
            }
            (Some(_), None) => {
                // Cache on left — expand rightward.
                let end = (gap_start + MIN_FETCH).min(file_size);
                (gap_start, end - gap_start)
            }
            (None, Some(right_start)) => {
                // Cache on right — expand leftward.
                let start = right_start.saturating_sub(MIN_FETCH);
                (start, right_start - start)
            }
            (None, None) => {
                // No overlap — expand rightward first, then leftward if clamped.
                let end = (offset + MIN_FETCH).min(file_size);
                let start = if end - offset < MIN_FETCH {
                    end.saturating_sub(MIN_FETCH)
                } else {
                    offset
                };
                (start, end - start)
            }
        }
    }

    /// Insert a fetched block and merge with any overlapping/adjacent blocks.
    fn insert(&mut self, offset: u64, data: Bytes) {
        if data.is_empty() {
            return;
        }
        let end = offset + data.len() as u64;

        // Compute merged bounds across all overlapping/adjacent blocks.
        let mut merged_start = offset;
        let mut merged_end = end;
        for block in &self.blocks {
            if block.offset <= end && block.end() >= offset {
                merged_start = merged_start.min(block.offset);
                merged_end = merged_end.max(block.end());
            }
        }

        // Assemble merged data.
        let merged_len = (merged_end - merged_start) as usize;
        let mut merged = vec![0u8; merged_len];

        // Copy existing overlapping blocks first.
        for block in &self.blocks {
            if block.offset <= end && block.end() >= offset {
                let dst = (block.offset - merged_start) as usize;
                merged[dst..dst + block.data.len()].copy_from_slice(&block.data);
            }
        }

        // Copy new data on top (authoritative).
        let dst = (offset - merged_start) as usize;
        merged[dst..dst + data.len()].copy_from_slice(&data);

        // Remove old overlapping blocks, insert merged.
        self.blocks
            .retain(|b| !(b.offset <= end && b.end() >= offset));
        self.blocks.push(CacheBlock {
            offset: merged_start,
            data: Bytes::from(merged),
        });

        trace!(
            blocks = self.blocks.len(),
            total_cached = self.blocks.iter().map(|b| b.data.len()).sum::<usize>(),
            "cache updated"
        );
    }
}

// ── Chained async reader ─────────────────────────────────────────────────────

/// Chains multiple [`AsyncRead`] implementations sequentially.
///
/// Reads from the front reader until EOF, then advances to the next.
/// Used to splice cached prefix/suffix bytes around a network stream.
struct ChainedReader {
    readers: VecDeque<Box<dyn AsyncRead + Send + Unpin>>,
}

impl AsyncRead for ChainedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let Some(reader) = self.readers.front_mut() else {
                return Poll::Ready(Ok(()));
            };
            let before = buf.filled().len();
            ready!(Pin::new(reader).poll_read(cx, buf))?;
            if buf.filled().len() > before {
                return Poll::Ready(Ok(()));
            }
            // Current reader exhausted — advance.
            self.readers.pop_front();
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    /// In-memory ReadAt for testing — no network, no disk.
    struct MemReadAt {
        data: Bytes,
        /// Track how many read_range calls hit the inner reader.
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl MemReadAt {
        fn new(data: &[u8]) -> Self {
            Self {
                data: Bytes::copy_from_slice(data),
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl ReadAt for MemReadAt {
        async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let start = offset as usize;
            let end = (start + len as usize).min(self.data.len());
            if start > self.data.len() {
                return Err(ReadError::OutOfBounds {
                    offset,
                    len,
                    size: self.data.len() as u64,
                });
            }
            Ok(self.data.slice(start..end))
        }

        async fn read_stream(
            &self,
            offset: u64,
            len: u64,
        ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
            let data = self.read_range(offset, len).await?;
            Ok(Box::new(io::Cursor::new(data)))
        }

        async fn size(&self) -> Result<u64, ReadError> {
            Ok(self.data.len() as u64)
        }
    }

    /// Build a 128 KiB test buffer with known pattern (byte = offset % 251).
    fn test_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn small_read_is_expanded_and_cached() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // First read: 22 bytes near end — should expand to MIN_FETCH.
        let result = cached.read_range(199_978, 22).await.unwrap();
        assert_eq!(result.as_ref(), &data[199_978..200_000]);
        assert_eq!(cached.inner.calls(), 1);

        // Second read overlapping — should hit cache.
        let result = cached.read_range(199_990, 10).await.unwrap();
        assert_eq!(result.as_ref(), &data[199_990..200_000]);
        assert_eq!(cached.inner.calls(), 1); // no new fetch
    }

    #[tokio::test]
    async fn read_at_start_and_end_are_two_fetches() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Read near end.
        let r1 = cached.read_range(199_978, 22).await.unwrap();
        assert_eq!(r1.as_ref(), &data[199_978..200_000]);

        // Read at start.
        let r2 = cached.read_range(0, 30).await.unwrap();
        assert_eq!(r2.as_ref(), &data[0..30]);

        // Both should be 2 inner reads (one per region).
        assert_eq!(cached.inner.calls(), 2);

        // Subsequent reads in both regions hit cache.
        let r3 = cached.read_range(0, 100).await.unwrap();
        assert_eq!(r3.as_ref(), &data[0..100]);
        let r4 = cached.read_range(199_950, 50).await.unwrap();
        assert_eq!(r4.as_ref(), &data[199_950..200_000]);
        assert_eq!(cached.inner.calls(), 2); // still just 2
    }

    #[tokio::test]
    async fn partial_overlap_avoids_re_download() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Cache [0, 65536).
        cached.read_range(0, 100).await.unwrap();
        assert_eq!(cached.inner.calls(), 1);

        // Read that spans cached + uncached: [60000, 70000).
        let result = cached.read_range(60_000, 10_000).await.unwrap();
        assert_eq!(result.as_ref(), &data[60_000..70_000]);
        assert_eq!(cached.inner.calls(), 2);

        // The fetch should have been for [65536, ...) not [60000, ...).
        // Verify by reading [65000, 66000) — should be cached now.
        let result = cached.read_range(65_000, 1_000).await.unwrap();
        assert_eq!(result.as_ref(), &data[65_000..66_000]);
        assert_eq!(cached.inner.calls(), 2); // no new fetch
    }

    #[tokio::test]
    async fn size_is_cached() {
        let inner = MemReadAt::new(&[0; 1000]);
        let cached = CachingReadAt::new(inner);

        let s1 = cached.size().await.unwrap();
        let s2 = cached.size().await.unwrap();
        assert_eq!(s1, 1000);
        assert_eq!(s2, 1000);
    }

    #[tokio::test]
    async fn zero_length_read_returns_empty() {
        let inner = MemReadAt::new(&[0; 100]);
        let cached = CachingReadAt::new(inner);

        let result = cached.read_range(50, 0).await.unwrap();
        assert!(result.is_empty());
        assert_eq!(cached.inner.calls(), 0);
    }

    #[tokio::test]
    async fn read_stream_serves_cached_prefix() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Prime cache at start: [0, 65536).
        cached.read_range(0, 100).await.unwrap();
        let calls_after_prime = cached.inner.calls();

        // Stream [0, 100_000) — first 65536 bytes from cache, rest from inner.
        let mut stream = cached.read_stream(0, 100_000).await.unwrap();
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();

        assert_eq!(collected, &data[0..100_000]);
        // Inner should have been called for the stream portion [65536, 100_000).
        // Plus the original cache prime = calls_after_prime + 1.
        assert_eq!(cached.inner.calls(), calls_after_prime + 1);
    }

    #[tokio::test]
    async fn read_stream_serves_cached_suffix() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Prime cache at end: somewhere in [134464, 200_000).
        cached.read_range(199_978, 22).await.unwrap();
        let calls_after_prime = cached.inner.calls();

        // Grab the cached block's start offset for assertion.
        let cache = cached.cache.lock().await;
        let cached_block_start = cache.blocks[0].offset;
        drop(cache);

        // Stream from before the cached block to EOF.
        let stream_start = cached_block_start - 10_000;
        let stream_len = 200_000 - stream_start;
        let mut stream = cached.read_stream(stream_start, stream_len).await.unwrap();
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();

        assert_eq!(collected, &data[stream_start as usize..200_000]);
        // One new inner call for the uncached middle portion.
        assert_eq!(cached.inner.calls(), calls_after_prime + 1);
    }

    #[tokio::test]
    async fn read_stream_no_overlap_passes_through() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Stream without priming cache — pure passthrough.
        let mut stream = cached.read_stream(50_000, 10_000).await.unwrap();
        let mut collected = Vec::new();
        stream.read_to_end(&mut collected).await.unwrap();

        assert_eq!(collected, &data[50_000..60_000]);
    }

    #[tokio::test]
    async fn adjacent_blocks_are_merged() {
        let data = test_data(200_000);
        let inner = MemReadAt::new(&data);
        let cached = CachingReadAt::new(inner);

        // Read [0, 65536).
        cached.read_range(0, 100).await.unwrap();
        // Read [65536, 131072) — should merge into [0, 131072).
        cached.read_range(65_536, 100).await.unwrap();

        let cache = cached.cache.lock().await;
        assert_eq!(cache.blocks.len(), 1, "adjacent blocks should be merged");
        assert_eq!(cache.blocks[0].offset, 0);
        assert!(cache.blocks[0].data.len() >= 131_072);
    }
}
