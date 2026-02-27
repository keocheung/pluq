//! Shared compression/decompression infrastructure used by ALL format parsers.
//!
//! # Design
//!
//! - [`CompressionMethod`] is the canonical enum for all algorithms. Format parsers
//!   translate their internal IDs (ZIP u16, 7z byte-sequence) to this enum.
//! - [`Decoder`] / [`Encoder`] are shared traits so format parsers never import `flate2`
//!   or `lzma-rust2` directly.
//! - [`build_decoder`] is the single dispatch point. Add a new algorithm by adding a
//!   match arm here — format parsers see no change.
//! - Algorithms not yet implemented return [`ArchiveError::UnsupportedMethod`] rather
//!   than panicking or linking dead code.

mod deflate;
mod store;

pub use deflate::DeflateDecoder;
pub use store::StoreDecoder;

use std::io::{self, Read, Write};

use crate::ArchiveError;

// ── CompressionMethod ──────────────────────────────────────────────────────

/// Canonical identifier for a compression algorithm.
///
/// Shared across all format parsers (ZIP, 7z, RAR, TAR). Each format translates
/// its internal method IDs to this enum so the codec layer stays format-agnostic.
///
/// `#[non_exhaustive]` — callers must handle a `_` arm. New algorithms can be
/// added without breaking downstream code.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompressionMethod {
    /// No compression — bytes are stored verbatim. Always available.
    Store,
    /// DEFLATE — used by ZIP, TAR.GZ. Fastest/most compatible.
    Deflate,
    /// BZip2 — used by ZIP, 7z, TAR.BZ2.
    BZip2,
    /// LZMA — legacy variant, used by older 7z archives.
    Lzma,
    /// LZMA2 — modern variant, the primary 7z algorithm.
    Lzma2,
    /// Zstandard — modern ZIP and 7z archives.
    Zstd,
    /// PPMd — statistical model, rare in practice.
    Ppmd,
}

impl CompressionMethod {
    /// Returns `true` if this method is compiled in and can be used right now.
    ///
    /// Useful for per-entry UI display: show "unsupported compression" rather
    /// than failing the whole listing when one entry uses an unavailable codec.
    pub fn is_available(&self) -> bool {
        match self {
            Self::Store => true,
            Self::Deflate => true, // flate2 always present
            Self::BZip2 => false,  // TODO: add bzip2 crate
            Self::Lzma => false,   // TODO: add lzma-rust2 crate
            Self::Lzma2 => false,  // TODO: add lzma-rust2 crate
            Self::Zstd => false,   // TODO: add zstd crate
            Self::Ppmd => false,   // TODO: add ppmd-rust crate
        }
    }

    /// Human-readable name for display in listings.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Store => "Store",
            Self::Deflate => "Deflate",
            Self::BZip2 => "BZip2",
            Self::Lzma => "LZMA",
            Self::Lzma2 => "LZMA2",
            Self::Zstd => "Zstd",
            Self::Ppmd => "PPMd",
        }
    }
}

impl std::fmt::Display for CompressionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Decoder / Encoder traits ───────────────────────────────────────────────

/// A streaming decompressor.
///
/// Wraps a compressed byte source (`impl Read`) and produces raw bytes.
/// Runs synchronously (CPU-bound) — callers must use `tokio::task::spawn_blocking`
/// when feeding data from async I/O.
pub trait Decoder: Read + Send {
    fn method(&self) -> CompressionMethod;
}

/// A streaming compressor.
///
/// Wraps a compressed byte sink (`impl Write`) and accepts raw bytes.
/// `finish()` MUST be called — some codecs buffer data and flush on completion.
/// `Drop` alone is not sufficient.
pub trait Encoder: Write + Send {
    fn method(&self) -> CompressionMethod;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

// ── build_decoder ──────────────────────────────────────────────────────────

/// Build the appropriate decoder for `method`, reading compressed bytes from `input`.
///
/// This is the single dispatch point for all decompression. Format parsers
/// call this instead of instantiating codec types directly.
///
/// `properties` carries codec-specific configuration (e.g. LZMA2 dictionary size).
/// Pass `&[]` for codecs that don't need properties (Store, Deflate).
///
/// # Errors
/// Returns [`ArchiveError::UnsupportedMethod`] if the method is not compiled in.
pub fn build_decoder<'a>(
    method: CompressionMethod,
    input: impl Read + Send + 'a,
    _properties: &[u8],
) -> Result<Box<dyn Decoder + 'a>, ArchiveError> {
    match method {
        CompressionMethod::Store => Ok(Box::new(StoreDecoder::new(input))),
        CompressionMethod::Deflate => Ok(Box::new(DeflateDecoder::new(input))),
        other => Err(ArchiveError::UnsupportedMethod {
            method: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    // ── is_available() ────────────────────────────────────────────────────────

    #[test]
    fn store_and_deflate_are_available() {
        assert!(CompressionMethod::Store.is_available());
        assert!(CompressionMethod::Deflate.is_available());
    }

    #[test]
    fn unimplemented_codecs_are_not_available() {
        assert!(!CompressionMethod::BZip2.is_available());
        assert!(!CompressionMethod::Lzma.is_available());
        assert!(!CompressionMethod::Lzma2.is_available());
        assert!(!CompressionMethod::Zstd.is_available());
        assert!(!CompressionMethod::Ppmd.is_available());
    }

    // ── build_decoder() ───────────────────────────────────────────────────────

    #[test]
    fn build_decoder_store_passes_through() {
        let input = b"hello, world!";
        let mut decoder = build_decoder(CompressionMethod::Store, &input[..], &[]).unwrap();
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn build_decoder_deflate_decompresses() {
        use flate2::Compression;
        use flate2::write::DeflateEncoder;
        use std::io::Write;

        // Compress with flate2, then decompress via our decoder.
        let input = b"Archive Sniper codec test - DEFLATE round-trip";
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(input).unwrap();
        let compressed = enc.finish().unwrap();

        let mut decoder = build_decoder(CompressionMethod::Deflate, &compressed[..], &[]).unwrap();
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn build_decoder_returns_unsupported_for_missing_codecs() {
        let dummy: &[u8] = &[];
        for method in [
            CompressionMethod::BZip2,
            CompressionMethod::Lzma,
            CompressionMethod::Lzma2,
            CompressionMethod::Zstd,
            CompressionMethod::Ppmd,
        ] {
            let result = build_decoder(method, dummy, &[]);
            assert!(
                matches!(result, Err(ArchiveError::UnsupportedMethod { .. })),
                "{method} should return UnsupportedMethod, got: {:?}",
                result.err()
            );
        }
    }
}
