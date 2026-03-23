//! Deflate64 (Enhanced Deflate) decoder — thin wrapper around `deflate64`.

use std::io::{self, BufReader, Read};

use super::{CompressionMethod, Decoder};

pub struct Deflate64Decoder<R: Read> {
    inner: ::deflate64::Deflate64Decoder<BufReader<R>>,
}

impl<R: Read + Send> Deflate64Decoder<R> {
    pub fn new(input: R) -> Self {
        Self {
            inner: ::deflate64::Deflate64Decoder::new(input),
        }
    }
}

impl<R: Read + Send> Read for Deflate64Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for Deflate64Decoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::Deflate64
    }
}
