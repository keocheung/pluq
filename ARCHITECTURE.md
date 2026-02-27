# ArchiveSniper — System Architecture

> **Source of truth** for how ArchiveSniper is structured.
> Last updated: 2026-02-27. Informed by deep research into the zesven (pure-Rust 7z) codebase and cross-examination of design decisions.
> See also: `CONTEXT.md` (project state).

---

## Mental Model

Five layers. Each has one job. Nothing leaks between them.

```
┌──────────────────────────────────────────────────────┐
│  Layer 4: Consumers                                  │
│  CLI, Server, UI                                     │
│  Knows ONLY: Archive, ArchiveEntry, ArchiveError     │
└──────────────────────┬───────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────┐
│  Layer 3: Archive Facade                             │
│  Archive::open(source) → auto-detects format         │
│  Unified API: entries(), extract(), create()         │
│  Dispatches to correct format parser                 │
└──────────────────────┬───────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────┐
│  Layer 2: Format Parsers (ArchiveReader trait)        │
│  zip/ │ sevenz/ │ rar/ │ tar/                        │
│  Each parser has:                                    │
│    public:  Vec<ArchiveEntry>    (universal fields)  │
│    private: Vec<FormatLocation>  (offsets, indexes)  │
│  Maps format-specific method IDs → shared codec enum │
└─────────┬────────────────────────┬───────────────────┘
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

**The four rules:**
1. Nothing leaks through boundaries — consumers never see ZipArchive, flate2, or LocalReadAt
2. Codec layer is shared — Deflate lives once, all format parsers use the same instance
3. ArchiveEntry has zero format-specific fields — format parsers hold location details privately
4. **Zero eager buffering** — no function in Layer 0 or Layer 2 may buffer more than 1 MiB of raw data into a `Vec<u8>`. Zesven uses `read_to_end()` (whole archive in RAM); we fetch exact byte ranges. A cloud archive can be 50 GB — never slurp it.

---

## Layer 0 — I/O Sources

### ReadAt Trait [LOCKED]

```rust
pub trait ReadAt: Send + Sync {
    fn read_range(&self, offset: u64, len: u64)
        -> impl Future<Output = Result<Bytes, ArchiveError>> + Send;
    fn size(&self)
        -> impl Future<Output = Result<u64, ArchiveError>> + Send;
}
```

**Implementations:**
- `LocalReadAt` — opens file, seeks, reads per call (stateless)
- `HttpReadAt` — HTTP Range header per call, with optional auth headers
- `Source` enum — static dispatch over Local/Http (no vtables)
- `MultiVolumeReadAt` [future] — joins multiple files into one virtual ReadAt
  - User passes first volume (e.g., `archive.7z.001`)
  - I/O layer discovers siblings by naming convention (`.002`, `.003`, ...)
  - Builds prefix-sum size table, translates global offsets to (volume_index, local_offset)
  - Format parser sees one seamless ReadAt, completely unaware of volume boundaries
- `SfxAdjustedReadAt` [future] — transparent offset adapter for self-extracting archives (.exe)
  - Scans first 1–16 MiB for 7z signature `37 7A BC AF 27 1C` (MZ/ELF/Mach-O heuristic first)
  - Full header validation after finding candidate bytes (false positives are real)
  - Wraps inner ReadAt with `sfx_offset`: every `read_range(offset, len)` becomes `read_range(sfx_offset + offset, len)`
  - Format parser is SFX-unaware — same clean adapter pattern as MultiVolumeReadAt

### HttpReadAt — Cloud Performance Requirements

**HTTP/2 with persistent connection pool.** `HttpReadAt` MUST use a `reqwest::Client` configured with HTTP/2 strictly enabled and a persistent connection pool. This avoids a TLS handshake (50–150 ms) per range request — critical when listing a 7z archive header requires 3+ sequential round trips.

**Request coalescing.** If the format parser requests two ranges within 64 KB of each other, merge them into one HTTP Range request. Downloading 64 KB of intermediate "gap" bytes is faster than paying 150 ms of latency for a second request.

**Dynamic auth (token refresh).** `HttpReadAt` MUST accept an `Arc<dyn TokenProvider>` rather than a static `HeaderMap`. On receiving HTTP 401 Unauthorized, transparently call `TokenProvider::refresh_token()` and retry — never surface a 401 to the format parser. Supports Dropbox/Drive OAuth tokens that expire every 60 minutes.

**Exponential backoff for rate limits.** HTTP 429 Too Many Requests MUST be handled at Layer 0 with exponential backoff + jitter. Never let a rate-limit bubble up to Layer 2 as `ArchiveError::Io`.

```rust
pub trait TokenProvider: Send + Sync {
    async fn get_token(&self) -> Result<HeaderValue, ArchiveError>;
    async fn refresh_token(&self) -> Result<HeaderValue, ArchiveError>;
}
```

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

Users buy MB credits for cloud sniping. Track every byte transferred.

```rust
pub struct MeteredReadAt<R: ReadAt> {
    inner: R,
    bytes_read: Arc<AtomicU64>,
}
```

- Wraps `HttpReadAt` (or any cloud source) — counts bytes in `read_range()`
- Local operations are NOT metered (free)
- `bytes_read.load()` at end of operation = billable amount
- Per-operation accounting (each extract/list call gets a fresh meter or an operation ID)
- The metering layer is a ReadAt decorator — completely transparent to format parsers

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
    _ => Err(ArchiveError::UnsupportedMethod { method }),
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

Useful for the UI: if an archive contains LZMA2 entries but the build doesn't include LZMA support, show "unsupported compression method" per-entry rather than failing entirely.

### Pipeline Assembly

For formats with codec chains (7z), the chain is built by nesting decoders:
```
Read direction:  packed bytes → [AES decrypt] → [LZMA2 decompress] → [BCJ reverse] → output
Write direction: raw bytes → [BCJ filter] → [LZMA2 compress] → [AES encrypt] → packed
```

Each stage wraps the previous one's output as a `Box<dyn Read>`.
For write, the simple linear path covers all real-world cases: filter → compress → (maybe encrypt).

### Async/Sync Boundary (spawn_blocking)

I/O is async (ReadAt returns futures, maps to HTTP Range headers).
Decompression is CPU-bound (heavy math, holds a thread) — it MUST run in `tokio::task::spawn_blocking`.

**Pattern for extraction:**
```
1. async: read_range(offset, len) → Bytes          // HTTP or disk
2. spawn_blocking: decompress(bytes) → Vec<u8>     // CPU-bound
3. async: write output / return to caller
```

**Key difference from zesven's model:** Zesven calls `read_to_end()` and pulls the entire archive into RAM before parsing. We must NOT do this — fetch only the specific byte ranges needed. A 50 GB cloud archive cannot live in RAM.

**CRC verification** runs in a separate `spawn_blocking` call after decompression — don't block the I/O thread on hashing. Corrupted files are deleted before returning the error.

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
    async fn extract(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError>;
    async fn test(&self, entry: &ArchiveEntry) -> Result<(), ArchiveError>;
}
```

`extraction_cost()` enables the "extraction tax" UX:
- **ZIP**: returns `entry.compressed_size` (every entry is independently seekable)
- **7z solid block**: returns `sum(compressed_sizes_of_all_preceding_entries_in_folder) + entry.compressed_size`
- Consumer shows: *"Sniping this 1 MB file requires downloading 500 MB. Proceed?"*

`test()` = decompress + verify CRC without writing to disk. Essential for integrity checking.

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
        _ => Err(ArchiveError::UnsupportedMethod { .. }),
    }
}

// 7z
fn sevenz_method(id: &[u8]) -> Result<CompressionMethod> {
    match id {
        [0x00] => Ok(CompressionMethod::Store),
        [0x21] => Ok(CompressionMethod::Lzma2),
        [0x04, 0x01, 0x08] => Ok(CompressionMethod::Deflate),
        _ => Err(ArchiveError::UnsupportedMethod { .. }),
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

Never show `ArchiveError::PasswordRequired` as a crash — surface it as a UI prompt.

#### BCJ2 Folders — Multiple Pack Streams

BCJ2 is structurally different from all other filters. It has **4 input streams** (Main, Call, Jump, Range) each stored as separate pack streams at different offsets in the archive. The `Folder` struct's `uses_bcj2()` flag signals this.

When extracting from a BCJ2 folder:
1. Check `folder.uses_bcj2()` — if true, use the BCJ2-specific decoder path
2. Issue up to 4 separate `read_range()` calls (one per pack stream) — ideally in parallel since ReadAt is stateless
3. Pass all 4 data blobs to the BCJ2 decoder

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

## Layer 3 — Archive Facade

### Archive Struct (exists, needs expansion)

```rust
pub struct Archive {
    inner: ArchiveInner,
}

enum ArchiveInner {
    Zip(ZipArchive<Source>),
    SevenZ(SevenZArchive<Source>),  // future
    Rar(RarArchive<Source>),        // future
    Tar(TarArchive<Source>),        // future
}
```

`Archive::open(source)` auto-detects source type AND format, then dispatches. Consumers never see what's inside.

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

### ArchiveError (needs expansion)

Current: `Io`, `OutOfBounds`, `Other(String)` — too coarse.
Target: structured variants with helper methods.

```rust
#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("out of bounds: offset {offset} + len {len} exceeds size {size}")]
    OutOfBounds { offset: u64, len: u64, size: u64 },

    #[error("invalid archive format: {message}")]
    InvalidFormat { message: String },

    #[error("unsupported compression method: {method}")]
    UnsupportedMethod { method: String },

    #[error("CRC mismatch for '{entry_name}': expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { entry_name: String, expected: u32, actual: u32 },

    #[error("password required")]
    PasswordRequired,

    #[error("wrong password or corrupted data")]
    WrongPassword,

    #[error("path traversal detected: {path}")]
    PathTraversal { path: String },

    #[error("resource limit exceeded: {message}")]
    ResourceLimitExceeded { message: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl ArchiveError {
    pub fn is_recoverable(&self) -> bool { /* WrongPassword, PasswordRequired, Cancelled */ }
    pub fn is_corruption(&self) -> bool { /* CrcMismatch, InvalidFormat */ }
    pub fn is_encryption_error(&self) -> bool { /* WrongPassword, PasswordRequired */ }
    pub fn is_security_error(&self) -> bool { /* PathTraversal */ }
    pub fn is_unsupported(&self) -> bool { /* UnsupportedMethod */ }
}
```

---

## Cross-Cutting Concerns

### CRC / Integrity Verification

**Rule: always verify after decompression, never silently return potentially corrupt bytes.**

- ZIP: CRC32 stored per-entry in Central Directory → verify after Deflate decompression
- 7z: CRC32 per-substream in SubStreamsInfo + per-folder in UnpackInfo
- RAR: CRC32 per-file
- TAR: weak header checksum only (no per-file CRC)

Return `ArchiveError::CrcMismatch` with entry name, expected, and actual values.
CRC mismatch is also the primary wrong-password detection mechanism for encrypted archives.

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
- Returns `ArchiveError::Cancelled` when true
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

---

## Implementation Priority (what to build next, while at ZIP stage)

These are cheapest to get right now and most expensive to retrofit later:

1. **Extract codec layer** — move Deflate out of zip/mod.rs into shared codec module
2. **Expand ArchiveError** — add typed variants and helper methods
3. **Add CRC verification** — parse crc32 from Central Directory, verify after decompression
4. **Add `modified_at`, `is_encrypted`, `compression_method` to ArchiveEntry** — ZIP has all of these
5. **Remove format-specific private fields from ArchiveEntry** — replace with `index: usize`
6. **Define ArchiveReader trait** — even if only ZIP implements it initially

These establish the abstractions that 7z, RAR, and TAR will build on.
