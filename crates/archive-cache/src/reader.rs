//! Metadata recording/replay belongs to the state layer, not the parsers.
use std::sync::{Arc, Mutex};

use archive_core::{LocalPath, ReadAt, ReadError};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataBlock {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct State {
    blocks: Vec<MetadataBlock>,
    recording: bool,
    replaying: bool,
    remaining: u64,
}

/// Records only bounded reads during open. Extraction streams always pass through.
/// The inner reader and capture state are shared with the cache coordinator.
pub struct MetadataReader<R> {
    inner: Arc<R>,
    size: u64,
    state: Arc<Mutex<State>>,
}

impl<R> Clone for MetadataReader<R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            size: self.size,
            state: self.state.clone(),
        }
    }
}

impl<R> MetadataReader<R> {
    pub(crate) fn new(inner: R, size: u64) -> Self {
        Self {
            inner: Arc::new(inner),
            size,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub(crate) fn record(&self, budget: u64) {
        *self.state.lock().unwrap() = State {
            recording: true,
            remaining: budget,
            ..State::default()
        };
    }

    pub(crate) fn replay(&self, blocks: Vec<MetadataBlock>) -> std::io::Result<()> {
        for block in &blocks {
            if block
                .offset
                .checked_add(block.bytes.len() as u64)
                .is_none_or(|end| end > self.size)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "metadata block outside source",
                ));
            }
        }
        *self.state.lock().unwrap() = State {
            blocks,
            replaying: true,
            ..State::default()
        };
        Ok(())
    }

    /// Stop recording and release buffered metadata before extraction or RAR scanning.
    /// None means the capture exceeded its budget and must not be persisted.
    pub(crate) fn finish(&self) -> Option<Vec<MetadataBlock>> {
        let state = std::mem::take(&mut *self.state.lock().unwrap());
        (state.recording || state.replaying).then_some(state.blocks)
    }
}

impl<R: LocalPath> LocalPath for MetadataReader<R> {
    fn local_path(&self) -> Option<&std::path::Path> {
        self.inner.local_path()
    }
}

impl<R: ReadAt> ReadAt for MetadataReader<R> {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ReadError> {
        {
            let state = self.state.lock().unwrap();
            if state.replaying {
                for block in &state.blocks {
                    if let Some(start) = offset.checked_sub(block.offset) {
                        if let Some(end) = start.checked_add(len) {
                            if end <= block.bytes.len() as u64 {
                                return Ok(Bytes::copy_from_slice(
                                    &block.bytes[start as usize..end as usize],
                                ));
                            }
                        }
                    }
                }
                // Incomplete snapshots are retried from the source by the coordinator.
                return Err(ReadError::OutOfBounds {
                    offset,
                    len,
                    size: self.size,
                });
            }
        }
        let bytes = self.inner.read_range(offset, len).await?;
        let mut state = self.state.lock().unwrap();
        if state.recording {
            if bytes.len() as u64 > state.remaining {
                *state = State::default();
            } else {
                state.remaining -= bytes.len() as u64;
                state.blocks.push(MetadataBlock {
                    offset,
                    bytes: bytes.to_vec(),
                });
            }
        }
        Ok(bytes)
    }

    async fn read_stream(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError> {
        self.inner.read_stream(offset, len).await
    }

    async fn size(&self) -> Result<u64, ReadError> {
        Ok(self.size)
    }
}
