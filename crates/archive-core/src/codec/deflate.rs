//! DEFLATE decoder — thin wrapper around `flate2`.
//!
//! This is the only place in the codebase that imports `flate2` directly.
//! Format parsers use `build_decoder(CompressionMethod::Deflate, input)` instead.

use std::io::{self, Read};

use flate2::read::DeflateDecoder as Flate2DeflateDecoder;

use super::{CompressionMethod, Decoder};

/// DEFLATE decompressor backed by `flate2`.
pub struct DeflateDecoder<R: Read> {
    inner: Flate2DeflateDecoder<R>,
}

impl<R: Read + Send> DeflateDecoder<R> {
    pub fn new(input: R) -> Self {
        Self {
            inner: Flate2DeflateDecoder::new(input),
        }
    }
}

impl<R: Read + Send> Read for DeflateDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for DeflateDecoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::Deflate
    }
}
