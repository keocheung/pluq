//! BZip2 decoder — thin wrapper around the `bzip2` crate.
//!
//! This is the only place in the codebase that imports `bzip2` directly.
//! Format parsers use `build_decoder(CompressionMethod::BZip2, input)` instead.

use std::io::{self, Read};

use super::{CompressionMethod, Decoder};

/// BZip2 decompressor backed by the `bzip2` crate.
pub struct BZip2Decoder<R: Read> {
    inner: ::bzip2::read::BzDecoder<R>,
}

impl<R: Read + Send> BZip2Decoder<R> {
    pub fn new(input: R) -> Self {
        Self {
            inner: ::bzip2::read::BzDecoder::new(input),
        }
    }
}

impl<R: Read + Send> Read for BZip2Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for BZip2Decoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::BZip2
    }
}
