//! Store (no-op) decoder — passes bytes through unchanged.

use std::io::{self, Read};

use super::{CompressionMethod, Decoder};

/// A pass-through decoder for `CompressionMethod::Store`.
///
/// Wraps any `impl Read` and forwards bytes verbatim. Zero allocation overhead.
pub struct StoreDecoder<R: Read> {
    inner: R,
}

impl<R: Read + Send> StoreDecoder<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: Read + Send> Read for StoreDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for StoreDecoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::Store
    }
}
