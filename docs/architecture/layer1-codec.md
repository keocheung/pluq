# Layer 1 — Codecs

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

Shared compression/decompression infrastructure used by ALL format parsers.
One implementation per algorithm. Format parsers translate their internal
method IDs to the shared `CompressionMethod` enum, then call dispatch.

---

## `CompressionMethod` Enum

```rust
#[non_exhaustive]
pub enum CompressionMethod {
    Store,      // No compression (copy)
    Deflate,    // ZIP, 7z, TAR.GZ
    BZip2,      // ZIP, 7z, TAR.BZ2
    Lzma,       // 7z
    Lzma2,      // 7z, TAR.XZ
    Zstd,       // Modern ZIP, 7z, TAR.ZST
    Ppmd,       // 7z
}
```

`#[non_exhaustive]` forces callers to handle the `_` case — safe to add
methods later.

---

## `FilterMethod` Enum (separate from compression)

```rust
#[non_exhaustive]
pub enum FilterMethod {
    BcjX86,     // x86 executables
    BcjArm,     // ARM executables
    BcjArm64,   // ARM64 executables
    Delta { distance: u8 },  // Audio/image data
}
```

Filters are NOT compressors — they transform data to improve compression
ratios. Only used by 7z format parser. ZIP and TAR never touch filters.

---

## `Decoder` / `Encoder` Traits

```rust
pub trait Decoder: Read + Send {
    fn method(&self) -> CompressionMethod;
}

pub trait Encoder: Write + Send {
    fn method(&self) -> CompressionMethod;
    fn finish(self: Box<Self>) -> io::Result<()>;
}
```

`finish()` is required — some codecs buffer data and must flush on
completion. `Drop` alone is not enough.

---

## Central Dispatch

```rust
pub fn build_decoder(
    method: CompressionMethod,
    input: impl Read + Send + 'static,
    properties: &[u8],
) -> Result<Box<dyn Decoder>, CoreError>
```

One function, all dispatch. Use `#[cfg(feature)]` on individual match
arms — if a feature is disabled at compile time, the `_` fallthrough fires
a clean `UnsupportedMethod` error rather than a panic or dead code warning:

```rust
match method {
    CompressionMethod::Store => Ok(Box::new(CopyDecoder::new(input))),
    #[cfg(feature = "deflate")]
    CompressionMethod::Deflate => Ok(Box::new(DeflateDecoder::new(input))),
    #[cfg(feature = "lzma")]
    CompressionMethod::Lzma2 => Ok(Box::new(Lzma2Decoder::new(input, properties)?)),
    _ => Err(CoreError::UnsupportedCompression { algorithm: method.to_string() }),
}
```

---

## `CodecConfig` — Memory Limits for Decoders

LZMA2 dict size is encoded in the coder properties byte. A 7z compressed
with a 1 GB dictionary requires 1 GB of RAM to decompress, regardless of
output size. On mobile or cloud servers this is fatal.

```rust
pub struct CodecConfig {
    /// Cap LZMA2 dictionary size. None = no cap (desktop). Some(64MB) for mobile/cloud.
    pub max_lzma_dict_size: Option<u64>,
}
```

Pass `CodecConfig` into `build_decoder()`. If the archive's LZMA2
properties request a dictionary larger than the cap →
`CoreError::ResourceLimitExceeded`. Reasonable defaults: desktop = `None`,
mobile/cloud server = `Some(128 MiB)`.

---

## Runtime Feature Check

```rust
impl CompressionMethod {
    pub fn is_available(&self) -> bool {
        match self {
            Self::Store => true,  // always available
            Self::Deflate => cfg!(feature = "deflate"),
            Self::Lzma | Self::Lzma2 => cfg!(feature = "lzma"),
            Self::Zstd => cfg!(feature = "zstd"),
            _ => false,
        }
    }
}
```

Useful for the UI: if an archive contains LZMA2 entries but the build
doesn't include LZMA support, show "unsupported compression" per-entry
rather than failing entirely.

---

## Pipeline Assembly

For formats with codec chains (7z), the chain is built by nesting decoders:

```text
Read direction:  packed bytes → [AES decrypt] → [LZMA2 decompress] → [BCJ reverse] → output
Write direction: raw bytes → [BCJ filter] → [LZMA2 compress] → [AES encrypt] → packed
```

Each stage wraps the previous one's output as a `Box<dyn Read>`. For
write, the simple linear path covers all real-world cases:
filter → compress → (maybe encrypt).

---

## Async/Sync Boundary (`spawn_blocking`)

I/O is async (ReadAt returns futures). Header reads use `read_range`
(returns `Bytes`). Entry data extraction uses `read_stream` (returns
`AsyncRead`). Decompression is CPU-bound (heavy math, holds a thread) —
it MUST run in `tokio::task::spawn_blocking`.

**Pattern for extraction (streaming via `read_stream`):**

```text
1. async: read_stream(offset, len) → Box<dyn AsyncRead>  // streaming HTTP body or file read
2. bridge: SyncIoBridge wraps AsyncRead into sync Read    // for the decompressor
3. spawn_blocking:                                        // CPU-bound
     decoder = build_decoder(method, bridged_reader)
     io::copy(decoder, sink)                              // stream through, bounded memory
     verify CRC
```

The decompressor reads incrementally from the bridged stream. The sink
receives decompressed chunks as they're produced. At no point is the
entire compressed or decompressed payload held in memory — even for a
4 GB entry.

**For header reads** (EOCD, central directory, local file headers), format
parsers use `read_range()` which returns `Bytes` directly. Always small
(< 1 MiB) and safe to buffer.

**Key difference from zesven's model:** Zesven calls `read_to_end()` and
pulls the entire archive into RAM before parsing. We fetch exact byte
ranges and stream them through the pipeline.

**CRC verification** runs inside the same `spawn_blocking` — hash
incrementally as decompressed bytes flow through. No second pass needed.

**Mid-stream error recovery:** `read_stream` implementations embed
`ReadError` inside `io::Error` before returning the `AsyncRead`. When
`SyncIoBridge` surfaces an `io::Error` during decompression, the
extraction code recovers the original `ReadError` via `into_inner()` +
`downcast::<ReadError>()`. If the downcast succeeds, the error is a
stream I/O failure and becomes `CoreError::Io(read_error)`. If it fails,
the error originated from the decompressor itself and becomes
`CoreError::BitstreamError`. This preserves structured error context
(`SourceInfo`, `ReadErrorKind`) across the async→sync boundary.

---

## External Crate Mapping

| Algorithm | Crate        | Pure Rust? | Cross-platform notes |
|-----------|-------------|------------|---------------------|
| Deflate   | `flate2`    | ✅ Yes (zlib-rs backend) | Works everywhere |
| BZip2     | `bzip2`     | ❌ Wraps C libbz2 | Needs C toolchain for mobile/WASM |
| LZMA      | `xz2`       | ❌ Wraps C liblzma | Needs C toolchain |
| LZMA2     | `lzma-rust2`| ✅ Yes             | Works everywhere  |
| ZSTD      | `zstd`      | ❌ Wraps C libzstd | Needs C toolchain for mobile/WASM |
| PPMd      | `ppmd-rust` | ✅ Yes | Works everywhere |

All wrapped behind the shared `Decoder`/`Encoder` traits — swapping crates
is invisible to format parsers.

**Cross-compilation strategy:** Prefer pure Rust crates. For BZip2 and
ZSTD, evaluate pure-Rust alternatives or add feature flags: `bzip2-c` (C
backend, default on desktop) vs `bzip2-rs` (pure Rust, for mobile/WASM).
The trait abstraction makes this swap invisible.

---

## Encryption

**AES-CBC primitives** live in the codec layer (shared).
**Key derivation and framing** are format-specific — they live in the
format parser:

- ZIP (WinZip AES): PBKDF2-HMAC-SHA1, salt/verification in Local File
  Header extra field
- 7z: SHA-256 iterative KDF with UTF-16-LE password, salt/IV in coder
  properties
- Same underlying AES-256-CBC, completely different protocols around it

The extraction pipeline is always:
**decrypt → decompress → (maybe reverse filter) → output**.
