# ArchiveSniper — System Architecture

> **Source of truth** for how ArchiveSniper is structured.
> Last updated: 2026-03-10. Informed by deep research into the zesven (pure-Rust 7z) codebase and cross-examination of design decisions.
> See also: `CONTEXT.md` (project state).

---

## Mental Model

Six layers. Each has one job. Nothing leaks between them.

```
┌──────────────────────────────────────────────────────────────────┐
│  Layer 4: Products (actual binaries)                              │
│                                                                    │
│  Server/SaaS       KMP Apps (Android/iOS/Desktop)    CLI           │
│                                                                    │
│  Each product:                                                     │
│    - Assembles ReadAt decorator stack from horizontal crates        │
│    - Passes composed ReadAt down to state layer                    │
│    - Opens StateError::Transport tunnel for decorator errors       │
│    - Defines own error type (ServerError, AppError, CliError)      │
│                                                                    │
│  Horizontal crates (optional, picked per product):                 │
│    archive-auth │ archive-retry │ metering (server-only)           │
│    Each provides ReadAt decorators + its own error type             │
│    Errors are boxed into ReadError::Transport by decorators        │
└──────────────────────┬───────────────────────────────────────────┘
                       │  decorated ReadAt passed down
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 3.5: State Layer (universal cache)                         │
│  Archive, ArchiveCache, EntryTree                                 │
│  Translates ArchiveError → StateError (core errors only)          │
│  Tunnels ReadError::Transport blindly (re-wraps for product)      │
│  See ERRORS.md for error flow                                     │
└──────────────────────┬───────────────────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 3: Archive Facade (stateless)                              │
│  ArchiveCore::open(reader) → auto-detects format                  │
│  Unified API: entries(), extract(sink), test()                    │
│  Dispatches to correct format parser                              │
│  Receives decorated ReadAt, unaware of decorators                 │
└──────────────────────┬───────────────────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 2: Format Parsers (ArchiveReader trait)                    │
│  zip/ │ sevenz/ │ rar/ │ tar/                                     │
│  Each parser has:                                                  │
│    public:  Vec<ArchiveEntry>    (universal fields)                │
│    private: Vec<FormatLocation>  (offsets, indexes)                │
│  Maps format-specific method IDs → shared codec enum               │
└─────────┬────────────────────────┬────────────────────────────────┘
          │                        │
┌─────────▼─────────┐  ┌──────────▼──────────────────┐
│  Layer 1:         │  │  Layer 1: Codec Layer        │
│  I/O Sources      │  │  CompressionMethod enum      │
│  ReadAt trait     │  │  build_decoder(method, in)   │
│  Local │ Http     │  │  build_encoder(method, out)  │
│  MultiVolume      │  │  Algorithms: Deflate, LZMA2, │
│  (future)         │  │    BZip2, ZSTD, Store        │
│                   │  │  Filters: BCJ, Delta         │
│                   │  │  Crypto: AES-CBC primitives   │
└───────────────────┘  └──────────────────────────────┘

Layer 0: Raw bytes (disk, network, memory)
```

**The five rules:**
1. Nothing leaks through boundaries — products never see `ArchiveCore`, `ZipArchive`, flate2, LocalReadAt, or ArchiveError. Products see `Archive` (state layer handle) + `StateError` + their own product-level error types.
2. Codec layer is shared — Deflate lives once, all format parsers use the same instance
3. ArchiveEntry has zero format-specific fields — format parsers hold location details privately
4. **Right tool for the job** — `ReadAt` has two read methods: `read_range()` returns `Bytes` (for bounded header/metadata reads), `read_stream()` returns `AsyncRead` (for large entry data extraction). Parsers use `read_range` for small reads. Extraction uses `read_stream` so a 4 GB entry never lives in RAM. `ArchiveReader::extract()` writes to a sink, not returns a buffer. Convenience methods (`extract_to_bytes()`) exist for small entries and tests.
5. **Core is stateless** — Layers 0–3 hold no session state, no caches, no auth tokens. Universal state (cache, session, entry tree) lives in the state layer (Layer 3.5). Product-specific state (auth tokens, quota counters, billing) lives in horizontal crates composed at the product level (Layer 4). See `ERRORS.md` for how errors flow across layers.
6. **I/O has its own error language** — `ReadAt` returns `ReadError` (not `ArchiveError`). `ReadError` is a standalone type covering both local filesystem and HTTP failures, with structured context for decorator routing and user-facing messages. Format parsers convert `ReadError → ArchiveError` at the boundary. See `ERRORS.md` § ReadError.

---

## Layer 0 — I/O Sources

### ReadAt Trait

```rust
pub trait ReadAt: Send + Sync {
    /// Read `len` bytes starting at `offset` into memory.
    /// Used by parsers for bounded header/metadata reads (EOCD, Central Directory, LFH).
    fn read_range(&self, offset: u64, len: u64)
        -> impl Future<Output = Result<Bytes, ReadError>> + Send;

    /// Stream `len` bytes starting at `offset` as an async reader.
    /// Used by extraction for potentially large entry data (compressed file content).
    /// Both production impls (LocalReadAt, HttpReadAt) must implement this — no default.
    fn read_stream(&self, offset: u64, len: u64)
        -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send;

    /// Total size of the underlying source in bytes.
    fn size(&self)
        -> impl Future<Output = Result<u64, ReadError>> + Send;
}
```

**Two methods, two jobs.** `read_range` returns `Bytes` — simple, direct, what parsers need for small bounded reads (headers are always < 1 MiB). `read_stream` returns an `AsyncRead` — what extraction needs for potentially huge entry data. A 4 GB stored ZIP entry streams through the decompressor into the output file with bounded memory. The caller never holds the entire payload in RAM.

**Returns `ReadError`, not `ArchiveError`.** The I/O layer has its own error vocabulary covering both local and remote failures, with structured context for decorator routing and user-facing messages. See § ReadError below.

Format parsers use `read_range()` for headers and `read_stream()` for entry data extraction.

**Implementations (both methods required, no defaults):**
- `LocalReadAt` — `read_range`: opens file, seeks, reads into buffer. `read_stream`: opens file, seeks, returns `file.take(len)` (bounded `AsyncRead` over the file handle).
- `HttpReadAt` — `read_range`: HTTP Range request, collects response via `.bytes()`. `read_stream`: HTTP Range request, returns the response body stream directly via `StreamReader` (zero copy).
- `Source` enum — static dispatch over Local/Http (no vtables), delegates both methods.
- `MultiVolumeReadAt` [future] — joins multiple files into one virtual ReadAt
  - User passes first volume (e.g., `archive.7z.001`)
  - I/O layer discovers siblings by naming convention (`.002`, `.003`, ...)
  - Builds prefix-sum size table, translates global offsets to (volume_index, local_offset)
  - Format parser sees one seamless ReadAt, completely unaware of volume boundaries
  - `ReadError::source` identifies which volume failed — "I/O error reading volume 3"
- `SfxAdjustedReadAt` [future] — transparent offset adapter for self-extracting archives (.exe)
  - Scans first 1–16 MiB for archive signature (MZ/ELF/Mach-O heuristic first)
  - Wraps inner ReadAt with `sfx_offset`: every `read_range(offset, len)` becomes `read_range(sfx_offset + offset, len)`
  - Format parser is SFX-unaware — same clean adapter pattern as MultiVolumeReadAt

### ReadError — I/O Error Language

`ReadError` is Layer 0's own error type. It covers both local filesystem and HTTP failures with structured context that serves two consumers: **decorators** (which need to route programmatically: retry? refresh token?) and **users** (who need a helpful message, not a stack trace).

See `ERRORS.md` § ReadError for the full type definitions. Summary of the design:

```rust
pub enum ReadError {
    /// An actual I/O failure — network or filesystem.
    /// Boxed because ReadFailure is 144 bytes (keeps ReadError at 32 bytes on the stack).
    Failed(Box<ReadFailure>),

    /// Opaque tunnel for decorator errors (Auth, Retry, Quota).
    /// Decorators inject here. Core carries blindly. Products open and downcast.
    Transport(Box<dyn Error + Send + Sync>),

    /// Requested bytes don't exist (HTTP 416, read past EOF).
    /// Format parsers treat this as CorruptHeader — their offset math was wrong.
    OutOfBounds { offset: u64, len: u64, size: u64 },
}

pub struct ReadFailure {
    pub source:     SourceInfo,      // LocalFile { path } | Http { url }
    pub operation:  ReadOperation,   // Open | SizeCheck | ReadRange { offset, len }
    pub kind:       ReadErrorKind,   // structured reason for routing + display
    pub diagnostic: String,          // raw error from underlying library (logs only)
}
```

`ReadErrorKind` has specific variants for every failure mode — network transport level (timeout, TLS, DNS), HTTP application level (status codes with headers + body for decorator inspection), and local filesystem (permission denied, file not found, disk I/O error, file locked). Each variant carries enough structured data to generate a genuinely helpful user message without string parsing.

**How format parsers convert `ReadError → ArchiveError`:**
- `ReadError::Failed(..)` → `ArchiveError::Io(ReadError::Failed(..))`
- `ReadError::Transport(box)` → `ArchiveError::Io(ReadError::Transport(box))` — tunnel preserved
- `ReadError::OutOfBounds { .. }` → `ArchiveError::CorruptHeader { .. }` — semantic upconversion

### HttpReadAt — Cloud Performance Requirements

**HTTP/2 with persistent connection pool.** `HttpReadAt` MUST use a `reqwest::Client` configured with HTTP/2 strictly enabled and a persistent connection pool. This avoids a TLS handshake (50–150 ms) per range request — critical when listing a 7z archive header requires 3+ sequential round trips.

**Request coalescing.** If the format parser requests two ranges within 64 KB of each other, merge them into one HTTP Range request. Downloading 64 KB of intermediate "gap" bytes is faster than paying 150 ms of latency for a second request.

**Core's `HttpReadAt` is stateless and dumb.** It accepts optional static headers (for pre-authenticated requests), makes one Range request, returns either buffered `Bytes` (`read_range`) or the response body as a streaming `AsyncRead` (`read_stream`). It does NOT handle auth refresh, retries, or rate limits — those are product-level concerns handled via `ReadAt` decorators from horizontal crates (see `ERRORS.md`). Errors are returned as structured `ReadError` variants (`HttpStatus`, `NetworkTimeout`, `RangeNotSupported`, etc.) — never as raw `reqwest::Error`.

**Auth, retry, and rate-limit handling** are implemented as `ReadAt` decorators from horizontal crates (e.g., `archive-auth`, `archive-retry`), assembled by the product (Layer 4) and passed down through the state layer into core. Core receives a composed `ReadAt` and is unaware of the decorator stack. See `ERRORS.md` § Decorator Construction.

**Mobile/KMP note:** On mobile platforms, OAuth may be handled entirely in Kotlin/Swift (using OkHttp, NSURLSession). The app gets a token, passes `Authorization: Bearer xyz` as a static header to `HttpReadAt::with_headers()`, and Rust never touches auth. The `archive-auth` crate is optional — products choose whether auth lives in Rust or the host language.

### Design Notes
- `ReadAt` maps directly to HTTP Range headers — this is why ArchiveSniper uses offset-based I/O instead of cursor-based Read+Seek
- `LocalReadAt` should wrap with buffering for sequential access patterns (TAR parsing) — currently opens a fresh handle per call

### Cross-Platform I/O [IMPORTANT]

ArchiveSniper must run on: Android, iOS, Windows, Linux, macOS (and potentially WASM/browser).
Platform-specific concerns are isolated to Layer 0 — everything above is portable Rust.

| Platform | ReadAt impl | Notes |
|----------|------------|-------|
| Desktop (Win/Mac/Linux) | `LocalReadAt` (tokio::fs) | Works as-is |
| Android | `AndroidContentReadAt` [future] | Content URIs, sandboxed storage |
| iOS | `IOSFileProviderReadAt` [future] | File provider API, sandboxed |
| Browser/WASM | `WasmReadAt` [future] | ArrayBuffer-backed, no real async |
| HTTP (all platforms) | `HttpReadAt` (reqwest/rustls) | Pure Rust TLS, works everywhere |

File path normalization: archive entries may contain `\` (Windows) or `/` (Unix). Always normalize to `/` internally, convert to platform separators on extraction.

### Bandwidth Metering (Credit System)

Metering is a product-level concern — only the server/SaaS product needs it. See § Horizontal Capabilities for `MeteredReadAt` and the decorator pattern. Local operations are never metered.

---

## Layer 1a — Codec Layer [TO BUILD]

Shared compression/decompression infrastructure used by ALL format parsers.

### CompressionMethod Enum

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

`#[non_exhaustive]` forces callers to handle the `_` case — safe to add methods later.

### FilterMethod Enum (separate from compression)

```rust
#[non_exhaustive]
pub enum FilterMethod {
    BcjX86,     // x86 executables
    BcjArm,     // ARM executables
    BcjArm64,   // ARM64 executables
    Delta { distance: u8 },  // Audio/image data
}
```

Filters are NOT compressors — they transform data to improve compression ratios.
Only used by 7z format parser. ZIP and TAR never touch filters.

### Decoder / Encoder Traits

```rust
pub trait Decoder: Read + Send {
    fn method(&self) -> CompressionMethod;
}

pub trait Encoder: Write + Send {
    fn method(&self) -> CompressionMethod;
    fn finish(self: Box<Self>) -> io::Result<()>;
}
```

`finish()` is required — some codecs buffer data and must flush on completion. `Drop` alone is not enough.

### Central Dispatch

```rust
pub fn build_decoder(
    method: CompressionMethod,
    input: impl Read + Send + 'static,
    properties: &[u8],
) -> Result<Box<dyn Decoder>, ArchiveError>
```

One function, all dispatch. Use `#[cfg(feature)]` on individual match arms — if a feature is disabled at compile time, the `_` fallthrough fires a clean `UnsupportedMethod` error rather than a panic or dead code warning:
```rust
match method {
    CompressionMethod::Store => Ok(Box::new(CopyDecoder::new(input))),
    #[cfg(feature = "deflate")]
    CompressionMethod::Deflate => Ok(Box::new(DeflateDecoder::new(input))),
    #[cfg(feature = "lzma")]
    CompressionMethod::Lzma2 => Ok(Box::new(Lzma2Decoder::new(input, properties)?)),
    _ => Err(ArchiveError::UnsupportedCompression { algorithm: method.to_string() }),
}
```

### CodecConfig — Memory Limits for Decoders

LZMA2 dict size is encoded in the coder properties byte. A 7z compressed with a 1 GB dictionary requires 1 GB of RAM to decompress, regardless of output size. On mobile or cloud servers this is fatal.

```rust
pub struct CodecConfig {
    /// Cap LZMA2 dictionary size. None = no cap (desktop). Some(64MB) for mobile/cloud.
    pub max_lzma_dict_size: Option<u64>,
}
```

Pass `CodecConfig` into `build_decoder()`. If the archive's LZMA2 properties request a dictionary larger than the cap → `ArchiveError::ResourceLimitExceeded`. Reasonable defaults: desktop = `None`, mobile/cloud server = `Some(128 MiB)`.

### Runtime Feature Check

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

Useful for the UI: if an archive contains LZMA2 entries but the build doesn't include LZMA support, show "unsupported compression" per-entry rather than failing entirely.

### Pipeline Assembly

For formats with codec chains (7z), the chain is built by nesting decoders:
```
Read direction:  packed bytes → [AES decrypt] → [LZMA2 decompress] → [BCJ reverse] → output
Write direction: raw bytes → [BCJ filter] → [LZMA2 compress] → [AES encrypt] → packed
```

Each stage wraps the previous one's output as a `Box<dyn Read>`.
For write, the simple linear path covers all real-world cases: filter → compress → (maybe encrypt).

### Async/Sync Boundary (spawn_blocking)

I/O is async (ReadAt returns futures). Header reads use `read_range` (returns `Bytes`). Entry data extraction uses `read_stream` (returns `AsyncRead`).
Decompression is CPU-bound (heavy math, holds a thread) — it MUST run in `tokio::task::spawn_blocking`.

**Pattern for extraction (streaming via `read_stream`):**
```
1. async: read_stream(offset, len) → Box<dyn AsyncRead>  // streaming HTTP body or file read
2. bridge: SyncIoBridge wraps AsyncRead into sync Read    // for the decompressor
3. spawn_blocking:                                        // CPU-bound
     decoder = build_decoder(method, bridged_reader)
     io::copy(decoder, sink)                              // stream through, bounded memory
     verify CRC
```

The decompressor reads incrementally from the bridged stream. The sink receives decompressed chunks as they're produced. At no point is the entire compressed or decompressed payload held in memory — even for a 4 GB entry.

**For header reads** (EOCD, central directory, local file headers), format parsers use `read_range()` which returns `Bytes` directly. These are always small (< 1 MiB) and safe to buffer.

**Key difference from zesven's model:** Zesven calls `read_to_end()` and pulls the entire archive into RAM before parsing. We fetch exact byte ranges and stream them through the pipeline.

**CRC verification** runs inside the same `spawn_blocking` — hash incrementally as decompressed bytes flow through. No second pass needed.

### External Crate Mapping

| Algorithm | Crate        | Pure Rust? | Cross-platform notes |
|-----------|-------------|------------|---------------------|
| Deflate   | `flate2`    | ✅ Yes (zlib-rs backend) | Works everywhere |
| BZip2     | `bzip2`     | ❌ Wraps C libbz2 | Needs C toolchain for mobile/WASM |
| LZMA/LZMA2| `lzma-rust2`| ✅ Yes | Works everywhere |
| ZSTD      | `zstd`      | ❌ Wraps C libzstd | Needs C toolchain for mobile/WASM |
| PPMd      | `ppmd-rust` | ✅ Yes | Works everywhere |

All wrapped behind the shared `Decoder`/`Encoder` traits — swapping crates is invisible to format parsers.

**Cross-compilation strategy:** Prefer pure Rust crates. For BZip2 and ZSTD, evaluate pure-Rust alternatives or add feature flags: `bzip2-c` (C backend, default on desktop) vs `bzip2-rs` (pure Rust, for mobile/WASM). The trait abstraction makes this swap invisible.

### Encryption

**AES-CBC primitives** live in the codec layer (shared).
**Key derivation and framing** are format-specific — they live in the format parser:
- ZIP (WinZip AES): PBKDF2-HMAC-SHA1, salt/verification in Local File Header extra field
- 7z: SHA-256 iterative KDF with UTF-16-LE password, salt/IV in coder properties
- Same underlying AES-256-CBC, completely different protocols around it

The extraction pipeline is always: **decrypt → decompress → (maybe reverse filter) → output**

---

## Layer 2 — Format Parsers

### ArchiveReader Trait [TO BUILD]

```rust
pub trait ArchiveReader {
    fn entries(&self) -> &[ArchiveEntry];
    fn entries_clean(&self) -> Vec<&ArchiveEntry>;

    /// Bytes that must be downloaded+decompressed to produce this entry.
    fn extraction_cost(&self, entry: &ArchiveEntry) -> u64;

    /// Extract entry, streaming decompressed bytes to `sink`. Returns bytes written.
    /// The base interface is always a sink — never returns Bytes directly.
    async fn extract<W: AsyncWrite + Send + Unpin>(
        &self, entry: &ArchiveEntry, sink: &mut W,
    ) -> Result<u64, ArchiveError>;

    /// Decompress + verify CRC, write nothing. Essential for integrity checking.
    async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError>;
}
```

**Sink-based extraction.** `extract()` writes decompressed bytes to any `AsyncWrite` — a file, an HTTP response body, a `Vec<u8>`, `/dev/null` (for test mode). The caller decides the destination, core never buffers the full output.

**Convenience for in-memory use** (CLI, small entries, tests) — lives on the state layer `Archive`, not on `ArchiveCore`:
```rust
impl Archive {  // state layer Archive
    pub async fn extract_to_bytes(&self, entry: &ArchiveEntry) -> Result<Bytes, StateError> {
        let mut buf = Vec::with_capacity(entry.uncompressed_size as usize);
        self.extract(entry, &mut buf).await?;
        Ok(Bytes::from(buf))
    }
}
```

`extraction_cost()` enables the "extraction tax" UX:
- **ZIP**: returns `entry.compressed_size` (every entry is independently seekable)
- **7z solid block**: returns `sum(compressed_sizes_of_all_preceding_entries_in_folder) + entry.compressed_size`
- State layer / UI shows: *"Sniping this 1 MB file requires downloading 500 MB. Proceed?"*

### Internal Location Data

Each format parser keeps location details PRIVATE — never on ArchiveEntry:

```rust
// ZIP: offset into the archive
struct ZipEntryLocation { offset: u64, method_id: u16, crc32: u32 }

// 7z: position within a solid block
struct SevenZEntryLocation { folder_index: usize, stream_index: usize, crc32: u32 }

// TAR: byte offset + stream-level compression
struct TarEntryLocation { data_offset: u64 }
```

Format parser holds `entries: Vec<ArchiveEntry>` + `locations: Vec<FormatLocation>` as parallel arrays. Same index maps entry → location.

### Format-Specific Method ID Translation

Each format translates its internal IDs to the shared `CompressionMethod` enum:

```rust
// ZIP
fn zip_method(id: u16) -> Result<CompressionMethod> {
    match id {
        0 => Ok(CompressionMethod::Store),
        8 => Ok(CompressionMethod::Deflate),
        12 => Ok(CompressionMethod::BZip2),
        14 => Ok(CompressionMethod::Lzma),
        93 => Ok(CompressionMethod::Zstd),
        _ => Err(ArchiveError::UnsupportedCompression { algorithm: id.to_string() }),
    }
}

// 7z
fn sevenz_method(id: &[u8]) -> Result<CompressionMethod> {
    match id {
        [0x00] => Ok(CompressionMethod::Store),
        [0x21] => Ok(CompressionMethod::Lzma2),
        [0x04, 0x01, 0x08] => Ok(CompressionMethod::Deflate),
        _ => Err(ArchiveError::UnsupportedCompression { algorithm: id.to_string() }),
    }
}
```

### The `has_stream` Flag

Entries in 7z (and RAR) can be "empty stream" — directories and anti-files that occupy zero compressed space. Don't use `uncompressed_size == 0` as a proxy for "no data" — some real files are 0 bytes. The `has_stream: bool` concept must be explicit in the format parser's internal data (not on ArchiveEntry, but in FormatLocation). Entries without streams are skipped entirely during the folder→entry mapping scan.

### Solid Block Handling (7z, RAR)

SubStreams = the slicing information that divides a decompressed solid block into individual files.
A Folder (solid block) decompresses to one blob. SubStreamsInfo says how many files are in it and their sizes.

**Extraction from solid block:**
1. Decompress the entire folder
2. Skip `sum(sizes[0..stream_index])` bytes (preceding files)
3. Read `sizes[stream_index]` bytes (target file)

**Sniping policy:** Solid archives are supported. When a user wants to snipe from a solid block, compute the cost (how many bytes must be decompressed and discarded to reach their file), inform them in user-friendly language, then proceed if they accept. Not anti-solid — just transparent about the cost.

### 7z Header Parsing — Cloud Sniping Implications

#### Encoded Headers (recursive)

A 7z Start Header points to a "Next Header" block. That block is either:
- **Plain** (`0x01`): parse directly → done
- **Encoded** (`0x17`): the header itself is compressed (and optionally encrypted). Must decompress it first to get the real header.

**The recursion:** encoded headers can themselves contain encoded headers. The spec requires:
- MUST support at least 1 level of encoding
- SHOULD support up to 4 levels (real archives exist with 2 levels)
- MUST reject archives with more than 4 levels (DoS protection against infinite recursion)

Parsing algorithm: loop until plain header is found, count levels, error on >4.

**Cloud sniping cost:** Each encoding level requires an additional HTTP round trip to fetch and decompress the header data before a file listing can be shown. A 2-level encoded archive needs 3 requests before the user sees any filenames.

#### Encrypted Headers

When the encoded header's coder chain includes AES, **filenames and metadata are hidden** until the password is provided. This is not an error — it's a feature. The correct UX response:
> "This archive has encrypted metadata. Enter password to view contents."

Password-required is not an error from the user's perspective — the state layer handles it by prompting for a password and retrying. See `ERRORS.md` for the full error translation flow.

#### BCJ2 Folders — Multiple Pack Streams

BCJ2 is structurally different from all other filters. It has **4 input streams** (Main, Call, Jump, Range) each stored as separate pack streams at different offsets in the archive. The `Folder` struct's `uses_bcj2()` flag signals this.

When extracting from a BCJ2 folder:
1. Check `folder.uses_bcj2()` — if true, use the BCJ2-specific decoder path
2. Issue up to 4 separate `read_stream()` calls (one per pack stream) — ideally in parallel since ReadAt is stateless
3. Pipe all 4 streams through the BCJ2 decoder

The ReadAt interface does NOT need to change. This is a format-parser concern, not an I/O concern.

#### External Properties (rare but real)

Some archives store file metadata (including names) outside the main header in `AdditionalStreamsInfo` (property ID `0x03`) with an `External == 0x01` flag. The parser must detect this flag and decompress the additional stream during `open()` to build a complete file listing. This adds one more HTTP round trip and decompression to the open cost.

**Liberal reader / strict writer rule:** Property IDs within a 7z header MUST be accepted in any order (many real-world tools violate the spec's ascending-order requirement). Always emit properties in ascending order when writing.

### Filename Encoding (The CP437 Problem)

Older ZIP files from Windows XP/7 use DOS codepage CP437, not UTF-8. `from_utf8_lossy` silently corrupts filenames with accented characters (é, å, 한글), breaking international archives.

**ZIP — General Purpose Bit 11:**
- Bit 11 = 1 → filename is UTF-8 (modern tools always set this)
- Bit 11 = 0 → legacy codepage; use `encoding_rs` crate to decode as CP437/Windows-1252

```rust
fn decode_zip_filename(bytes: &[u8], general_purpose_flags: u16) -> String {
    if general_purpose_flags & (1 << 11) != 0 {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned()
    }
}
```

**RAR/7z:** UTF-16LE throughout — no encoding ambiguity.
**TAR (pax headers):** UTF-8 with pax extension for long names; legacy GNU TAR is locale-dependent, treat as lossy UTF-8.

---

## Layer 3.5 — State Layer (Universal Cache) [TO BUILD]

The universal cache and session layer. Sits between products (Layer 4) and the stateless core (Layers 0–3). Holds **only** state that every product benefits from: cached metadata, session handles, virtual folder trees. Products interact with the state layer, never with core directly.

**What the state layer IS:** a universal cache that saves every product from duplicating work. Parsed entry lists, folder trees, fingerprint-based invalidation — all products (CLI, server, KMP apps) get this for free.

**What the state layer is NOT:** it is not the place for auth, billing, quotas, OAuth, or retry policies. Those are product-specific concerns that live in horizontal crates, assembled at the product level (Layer 4).

**Responsibilities:**
- Receives a `ReadAt` from the product above (possibly decorated — state layer doesn't know or care)
- Passes the `ReadAt` to `ArchiveCore::open()` in core
- Manages `Archive` (the product-facing handle wrapping a live `ArchiveCore`)
- Manages `ArchiveCache` (on-disk cross-session metadata cache with invalidation)
- Builds `EntryTree` (virtual folder structure derived from flat entry names)
- Translates `ArchiveError` → `StateError` for core errors it understands
- Tunnels `ReadError::Transport` (inside `ArchiveError::Io`) blindly as `StateError::Transport` — does not inspect or downcast
- Handles password prompts (encrypted archives are a universal concern)

**Key types** (all owned by state layer):
- `Archive` — product-facing handle to an open archive (wraps `ArchiveCore` + cache + entry tree)
- `StateError` — error type for product consumption (see `ERRORS.md`)
- `EntryTree` / `TreeNode` — virtual folder structure
- `ArchiveCache` / `ArchiveFingerprint` — cross-session metadata + location cache

**Crate placement:** `archive-state`, depends on `archive-core`. Pure cross-platform Rust. No dependency on any horizontal crate (auth, retry, metering). Platform-specific concerns (cache storage path) are injected via traits.

See `ERRORS.md` for the full error translation flow and tunnel mechanism.

---

## Horizontal Capabilities (Product-Level Crates)

Optional crates that products pick from to compose their `ReadAt` decorator stack. Each crate depends on `archive-core` (for the `ReadAt` trait and `ArchiveError`), but NOT on `archive-state`. Products assemble decorators, compose them into a `ReadAt` stack, and pass the result down to the state layer.

**`archive-auth`** — OAuth / token lifecycle decorator.
- `AuthReadAt<R: ReadAt>` — intercepts HTTP 401 responses, boxes `AuthError::TokenExpired` into `ArchiveError::Transport`, lets the product handle refresh.
- `AuthError` — `TokenExpired`, `RefreshFailed`, etc.
- Products that handle OAuth in the host language (e.g., Kotlin/Swift on mobile) can skip this crate entirely and pass pre-authenticated headers to `HttpReadAt::with_headers()`.

**`archive-retry`** — Retry / backoff decorator.
- `RetryReadAt<R: ReadAt>` — retries transient failures with configurable backoff.
- Products may want different retry aggressiveness (desktop vs mobile vs server).

**Metering / billing** — server-internal, not a shared crate.

```rust
// Example: MeteredReadAt (server-only)
pub struct MeteredReadAt<R: ReadAt> {
    inner: R,
    bytes_read: Arc<AtomicU64>,
}
```
- Wraps `HttpReadAt` (or any cloud source) — counts bytes in `read_range()` and `read_stream()`
- `bytes_read.load()` at end of operation = billable amount
- Per-operation accounting (each extract/list call gets a fresh meter or an operation ID)
- Only the server product uses this. Client apps don't meter.

**How decorators create tunneled errors:**

Each decorator intercepts errors from the `ReadAt` below it. If the error is something the decorator recognizes (e.g., HTTP 401), it boxes its own error type into `ArchiveError::Transport`. Core carries the box blindly. The state layer carries it blindly. The product at the top opens the box, downcasts to the types it knows about (from the horizontal crates it depends on), and handles or reports them.

See `ERRORS.md` § Transport Tunnel for the full multi-hop flow.

---

## Products (Layer 4)

Each product is an actual binary (or library consumed via FFI). It assembles the full stack: horizontal crates (if needed) → state layer → core.

**Server/SaaS** — serves the web version. The web site can't do direct HTTP range requests (CORS, no Range header control), so it goes through the server API. This is the only metered/freemium path.
- Assembles: `archive-core` + `archive-state` + `archive-auth` + `archive-retry` + metering (internal)
- Defines `ServerError` — extends `StateError` with `QuotaExceeded`, `AuthenticationFailed`, etc.
- Billing, quota accounting, OAuth for cloud providers (Dropbox, Drive) all live here.

**KMP Apps (Android / iOS / Desktop)** — standalone products. They are independent cloud snipers — connect directly to Dropbox/Drive, fetch ranges, extract. Buy once, no metering. Built with Compose Multiplatform, consuming the Rust core via UniFFI.
- Assembles: `archive-core` + `archive-state` + optionally `archive-auth` (or handle OAuth in Kotlin/Swift)
- Defines `AppError`
- Full local archive management (read + write) plus direct cloud sniping
- No dependency on the server for core functionality

**CLI** — thin shell for dev and power users.
- Assembles: `archive-core` + `archive-state`, minimal or no decorators
- Defines `CliError`

**Web site** — not a Rust product. Consumes the server's HTTP API. Metered/freemium.

---

## Layer 3 — Archive Facade (Stateless)

### ArchiveCore Struct (exists, needs expansion)

```rust
// Internal to archive-core. Products never see this type directly —
// they interact with Archive (the state layer handle) instead.
pub(crate) struct ArchiveCore {
    inner: ArchiveInner,
}

enum ArchiveInner {
    Zip(ZipArchive<Source>),
    SevenZ(SevenZArchive<Source>),  // future
    Rar(RarArchive<Source>),        // future
    Tar(TarArchive<Source>),        // future
}
```

`ArchiveCore::open(reader)` auto-detects format from the provided `ReadAt` and dispatches to the correct format parser. The `ReadAt` it receives may be decorated, but `ArchiveCore` is unaware — it only sees the `ReadAt` trait.

**Stateless:** `ArchiveCore` holds no session state, no caches, no auth tokens. It is constructed, used, and dropped. The state layer (`Archive`) owns it and decides when to create it, what `ReadAt` to give it, and what to do with the results.

---

## Key Types

### ArchiveEntry (needs redesign)

Current: has private ZIP-specific fields (`offset: u64`, `compression_method: u16`).
Target: zero format-specific fields.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    // Universal fields (always meaningful)
    pub name: String,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub is_directory: bool,
    pub is_encrypted: bool,
    pub modified_at: Option<SystemTime>,

    // Display-only (optional, for UI/listing)
    pub compression_method: Option<CompressionMethod>,

    // Internal index (not format-specific, just an ID)
    pub(crate) index: usize,
}
```

Format-specific location info (offsets, folder indexes, CRC values) stays inside the format parser's internal parallel array, keyed by `index`.

### ArchiveError

Error handling follows a layered translation model. Each layer defines errors for its consumer — no layer exposes a lower layer's error types. Errors are either handled (retried, prompted) and disappear, or translated into the consumer's vocabulary.

`ArchiveError` is core's error type. It has NO `Transport` variant — the decorator tunnel lives inside `ReadError::Transport`, which is carried within `ArchiveError::Io(ReadError)`. This keeps `ArchiveError` clean: every variant is genuinely an archive/format concept.

See **`ERRORS.md`** for the full error architecture: `ReadError` (I/O layer), `ArchiveError` (core), `StateError` (state layer), decorator error types, translation flows, and the injection pattern.

---

## Cross-Cutting Concerns

### CRC / Integrity Verification

**Rule: always verify after decompression, never silently return potentially corrupt bytes.**

- ZIP: CRC32 stored per-entry in Central Directory → verify after Deflate decompression
- 7z: CRC32 per-substream in SubStreamsInfo + per-folder in UnpackInfo
- RAR: CRC32 per-file
- TAR: weak header checksum only (no per-file CRC)

Return a CRC mismatch error with entry name, expected, and actual values.
CRC mismatch is also the primary wrong-password detection mechanism for encrypted archives.
See `ERRORS.md` for how CRC errors flow through layers.

### Progress Reporting [TO BUILD]

```rust
pub trait ProgressReporter: Send {
    fn on_entry_start(&mut self, name: &str, size: u64) {}
    fn on_entry_complete(&mut self, name: &str, success: bool) {}
    fn should_cancel(&self) -> bool { false }
}
```

- Independent of streaming — works with standard extraction
- `should_cancel()` replaces a separate cancellation channel
- Returns a cancellation error when true
- Wrap with `ThrottledProgress` (minimum interval between callbacks) for UI

### Multi-Volume Archives

Handled at the I/O layer via `MultiVolumeReadAt`:
- User passes first volume filename
- I/O layer discovers siblings by format-specific naming convention
- Builds prefix-sum size table for offset translation
- Format parser sees one seamless ReadAt

Volume naming conventions:
- 7z: `archive.7z.001`, `.002`, `.003`, ...
- ZIP: `archive.z01`, `.z02`, ..., `archive.zip`
- RAR: `archive.part1.rar`, `.part2.rar`, ...

### Caching (ZipIndex Pattern)

**Cache the metadata (entry list), not the data (file contents).**

```rust
pub struct ArchiveIndex {
    pub source: String,
    pub source_size: u64,
    pub entries: Vec<ArchiveEntry>,
    pub created_at: u64,
    // Staleness detection
    pub etag: Option<String>,        // HTTP ETag header
    pub last_modified: Option<String>, // HTTP Last-Modified header
}
```

On load: compare `source_size` against current archive size (one HEAD request for HTTP, one stat for local). If size changed → cache stale → re-scan. For HTTP: also check ETag/Last-Modified.

### Resource Limits

Apply at parse time (header reading) and extraction time. Checked before any decompression begins.

```rust
pub struct ResourceLimits {
    pub max_entries: usize,              // default: 1,000,000 — entry count bomb
    pub max_header_bytes: u64,           // default: 64 MiB   — oversized header DoS
    pub max_total_unpacked: u64,         // default: 1 TiB    — total extraction size
    pub max_entry_unpacked: u64,         // default: 64 GiB   — single entry size
    pub ratio_limit: Option<RatioLimit>, // default: 1000:1   — zip bomb ratio check
}

pub struct RatioLimit {
    pub max_ratio: u32,   // uncompressed / compressed threshold
    pub mode: LimitMode,
}

pub enum LimitMode {
    HardError,  // return Err (default — safe)
    Warn,       // surface warning to UI, continue
    Ignore,     // skip check (for advanced users)
}
```

**Ratio check implementation:** use `uncompressed > max_ratio * compressed` (multiplication, not division — avoids integer truncation errors on fractional ratios). Check for infinite ratio separately: `compressed == 0 && uncompressed > 0`.

**`LimitMode::Warn`** is useful for the extraction tax UI: show "this archive has suspicious compression ratio (800:1)" without blocking. The mode is per-limit so you can warn on ratio but hard-error on entry count.

**Streaming for solid blocks** [DEFERRED]: a 50 GB solid block shouldn't be buffered entirely in memory. Design the `ArchiveReader` trait to support streaming extraction mode, implement when 7z support is built.

---

## Design Principles (adopted from zesven philosophy)

1. **Safety first** — resource limits, input validation, and path traversal protection with two modes:
   - **Sanitize** (default): strip `../` and absolute path components, extract with sanitized name, warn user. Maximally compatible.
   - **Strict**: reject archive entirely if any entry contains path traversal. For high-security contexts.
   - Never add a "Relaxed/allow traversal" mode — there is no legitimate use case for writing outside the output directory.
2. **Correctness over speed** — always verify CRC, fail rather than guess
3. **Fail early, fail informatively, fail safely** — errors carry context (entry name, offset, method ID); no partial state on failure
4. **Accept liberal, emit conservative** — tolerate minor spec deviations when reading; produce maximally compatible output when writing
5. **Defer errors to extraction, not parse time** — parse the header even if you can't decompress all methods; let users at least list entries

---

## Format-Specific Write Notes

### 7z: Header Written at End

ZIP writes metadata inline with each entry (local header → data, repeat). You can stream-write a ZIP.

**7z is structurally inverted**: the header block (containing pack sizes, offsets, CRCs, method info for all entries) is written at the **end** of the file. You cannot finalize a 7z archive until the very last byte of compressed data is known.

**Implication for Vault / cloud archiving:** A 7z archive must be built entirely locally before upload. There is no "compress-and-stream-to-cloud-simultaneously" for 7z. ZIP can pipeline; 7z cannot. Factor this into the Vault feature design.

---

## User Decision Model (Write Side)

For creating archives, users get three choices:

### Choice 1: Compression Profile (human-readable, not algorithm names)

| User sees | Format + Algorithm | Tradeoff |
|---|---|---|
| **"Most Compatible"** (default) | ZIP + Deflate | Opens everywhere — WinRAR, 7-Zip, macOS, phone apps. Decent ratio. |
| **"Best Compression"** | 7z + LZMA2 | Smallest files. Slower. Needs 7-Zip or compatible tool to open. |
| **"Fast"** | ZIP + ZSTD or 7z + ZSTD | Fast encode/decode, good ratio. Needs modern tools. |

Users never see "Deflate" or "LZMA2". They see the tradeoff that matters to them.

### Choice 2: Password

Yes/no + passphrase. Encryption method is automatic per format (AES-256 for ZIP, AES-256-SHA256 for 7z).

### Choice 3: (nothing else for now)

That's it. Two choices. Advanced mode (future) exposes: raw algorithm, compression level (1-9), filter selection (BCJ for executables), solid block settings, split archive size.

---

## Timestamps

Each format stores timestamps differently:
- ZIP: 32-bit DOS packed field (2-second precision, years since 1980)
- 7z: Windows FILETIME (100-nanosecond precision, epoch 1601-01-01)
- TAR: Unix timestamp (seconds since 1970)
- RAR: Windows FILETIME

ArchiveEntry stores: `pub modified_at: Option<SystemTime>`
Conversion constant for 7z FILETIME → Unix: subtract `116444736000000000`, divide by `10_000_000`

