//! LZMA decoder — wraps `xz2` legacy LZMA ("alone") stream decoder.
//!
//! The caller must strip format-specific headers (e.g. the 4-byte ZIP LZMA header)
//! and pass the 5-byte LZMA properties via `build_decoder`'s `properties` parameter.
//!
//! Internally this reconstructs the "LZMA alone" container header
//! (`[5-byte props][8-byte uncompressed size = unknown]`) so that liblzma's
//! `lzma_alone_decoder` can process the raw bitstream.

use super::{CompressionMethod, Decoder};
use crate::CoreError;
use std::io::{self, Cursor, Read};
use xz2::stream::Stream;

pub struct LzmaDecoder<R: Read> {
    inner: xz2::read::XzDecoder<std::io::Chain<Cursor<Vec<u8>>, R>>,
}

impl<R: Read + Send> LzmaDecoder<R> {
    /// Create a new LZMA decoder.
    ///
    /// `properties` must be at least 5 bytes (the raw LZMA property byte + 4-byte dict size).
    pub fn new(
        input: R,
        properties: &[u8],
        codec_config: &crate::limits::CodecConfig,
    ) -> Result<Self, CoreError> {
        if properties.len() < 5 {
            return Err(CoreError::BitstreamError {
                detail: format!(
                    "LZMA properties too short: {} bytes (need 5)",
                    properties.len()
                ),
            });
        }

        // Bytes 1..5 encode the dictionary size as little-endian u32.
        let dict_size =
            u32::from_le_bytes([properties[1], properties[2], properties[3], properties[4]]) as u64;

        if let Some(cap) = codec_config.max_lzma_dict_size
            && dict_size > cap
        {
            return Err(CoreError::ResourceLimitExceeded {
                message: format!(
                    "LZMA dictionary size {dict_size} bytes exceeds configured cap {cap} bytes"
                ),
            });
        }

        // Reconstruct the "LZMA alone" format header:
        // [5-byte properties] [8-byte uncompressed size = unknown (0xFFFF...FFFF)]
        let mut header = Vec::with_capacity(13);
        header.extend_from_slice(&properties[..5]);
        header.extend_from_slice(&u64::MAX.to_le_bytes());

        let chained = Cursor::new(header).chain(input);

        let memlimit = codec_config.max_lzma_dict_size.unwrap_or(u64::MAX);
        let stream = Stream::new_lzma_decoder(memlimit).map_err(|e| CoreError::BitstreamError {
            detail: format!("LZMA decoder init: {e}"),
        })?;

        Ok(Self {
            inner: xz2::read::XzDecoder::new_stream(chained, stream),
        })
    }
}

impl<R: Read + Send> Read for LzmaDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send> Decoder for LzmaDecoder<R> {
    fn method(&self) -> CompressionMethod {
        CompressionMethod::Lzma
    }
}
