//! Zstandard decoder — thin wrapper around `zstd`.

use std::io::{self, BufReader, Read};

use super::{CompressionMethod, Decoder};

pub struct ZstdDecoder<R: Read> {
    inner: ::zstd::stream::read::Decoder<'static, BufReader<R>>,
}

impl<R: Read + Send> ZstdDecoder<R> {
    pub fn new(input: R) -> io::Result<Self> {
        Ok(Self {
            inner: ::zstd::stream::read::Decoder::new(input)?,
        })
    }
}

impl<R: Read + Send> Read for ZstdDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for ZstdDecoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::Zstd
    }
}
