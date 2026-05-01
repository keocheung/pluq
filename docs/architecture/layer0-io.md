# Layer 0 — I/O Sources

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

The bottom layer. Provides bytes to format parsers via the `ReadAt` trait.
Knows nothing about archive formats.

---

## `ReadAt` Trait

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

**Two methods, two jobs.** `read_range` returns `Bytes` — simple, direct,
what parsers need for small bounded reads (headers always < 1 MiB).
`read_stream` returns an `AsyncRead` — what extraction needs for
potentially huge entry data. A 4 GB stored ZIP entry streams through the
decompressor into the output file with bounded memory.

**Returns `ReadError`, not `CoreError`.** I/O has its own error
vocabulary. See `docs/errors/read-error.md`.

Format parsers use `read_range()` for headers and `read_stream()` for
entry data extraction.

---

## `LocalPath` Trait

Companion to `ReadAt`. Optional marker that returns `Option<&Path>` for
local-file shortcuts. Default impl returns `None`. `LocalReadAt` overrides
to return the file path. Used by the RAR extraction pipeline to decide
between the local shortcut (pass the path directly to `unrar`) and the
cloud synthetic-archive approach.

---

## Implementations

Both `read_range` and `read_stream` are required — no defaults.

- **`LocalReadAt`** —
  - `read_range`: opens file, seeks, reads into buffer.
  - `read_stream`: opens file, seeks, returns `file.take(len)` (bounded
    `AsyncRead` over the file handle).

- **`HttpReadAt`** —
  - `read_range`: HTTP Range request, collects response via `.bytes()`.
  - `read_stream`: HTTP Range request, returns the response body stream
    directly via `StreamReader` (zero copy).

- **`MultiVolumeReadAt`** *[future]* — joins multiple files into one
  virtual ReadAt.
  - User passes first volume (e.g., `archive.7z.001`).
  - I/O layer discovers siblings by naming convention (`.002`, `.003`, …).
  - Builds prefix-sum size table, translates global offsets to
    `(volume_index, local_offset)`.
  - Format parser sees one seamless ReadAt, completely unaware of volume
    boundaries.
  - `ReadError::source` identifies which volume failed — "I/O error
    reading volume 3".

- **`SfxAdjustedReadAt`** *[future]* — transparent offset adapter for
  self-extracting archives (.exe).
  - Scans first 1–16 MiB for archive signature (MZ/ELF/Mach-O heuristic
    first).
  - Wraps inner ReadAt with `sfx_offset`: every `read_range(offset, len)`
    becomes `read_range(sfx_offset + offset, len)`.
  - Format parser is SFX-unaware — same clean adapter pattern as
    `MultiVolumeReadAt`.

---

## `HttpReadAt` — Cloud Performance Requirements

**HTTP/2 with persistent connection pool.** `HttpReadAt` MUST use a
`reqwest::Client` configured with HTTP/2 strictly enabled and a persistent
connection pool. Avoids a TLS handshake (50–150 ms) per range request —
critical when listing a 7z archive header requires 3+ sequential round
trips.

**Core's `HttpReadAt` is stateless and dumb.** Accepts optional static
headers (for pre-authenticated requests), makes one Range request, returns
either buffered `Bytes` (`read_range`) or the response body as a streaming
`AsyncRead` (`read_stream`). It does NOT handle auth refresh, retries, or
rate limits — those are product-level concerns handled via `ReadAt`
decorators from horizontal crates (see `layer4-products.md` and
`docs/errors/transport-tunnel.md`). Errors returned as structured
`ReadError` variants — never raw `reqwest::Error`.

**Auth, retry, and rate-limit handling** are implemented as `ReadAt`
decorators from horizontal crates (e.g., `archive-auth`, `archive-retry`),
assembled by the product (Layer 4) and passed down through the state
layer into core. Core receives a composed `ReadAt` and is unaware of the
decorator stack.

**Mobile/KMP note:** On mobile, OAuth may be handled entirely in
Kotlin/Swift (using OkHttp, NSURLSession). The app gets a token, passes
`Authorization: Bearer xyz` as a static header to
`HttpReadAt::with_headers()`, and Rust never touches auth. The
`archive-auth` crate is optional — products choose whether auth lives in
Rust or the host language.

---

## Design Notes

- `ReadAt` maps directly to HTTP Range headers — this is why pluq
  uses offset-based I/O instead of cursor-based Read+Seek.
- `LocalReadAt` should wrap with buffering for sequential access patterns
  (TAR parsing) — currently opens a fresh handle per call.

---

## Cross-Platform I/O [important]

pluq must run on: Android, iOS, Windows, Linux, macOS (and
potentially WASM/browser). Platform-specific concerns are isolated to
Layer 0 — everything above is portable Rust.

| Platform | ReadAt impl | Notes |
|----------|------------|-------|
| Desktop (Win/Mac/Linux) | `LocalReadAt` (tokio::fs) | Works as-is |
| Android | `AndroidContentReadAt` *[future]* | Content URIs, sandboxed storage |
| iOS | `IOSFileProviderReadAt` *[future]* | File provider API, sandboxed |
| Browser/WASM | `WasmReadAt` *[future]* | ArrayBuffer-backed, no real async |
| HTTP (all platforms) | `HttpReadAt` (reqwest/rustls) | Pure Rust TLS, works everywhere |

File path normalization: archive entries may contain `\` (Windows) or `/`
(Unix). Always normalize to `/` internally, convert to platform separators
on extraction.

---

## Bandwidth Metering

Metering is a product-level concern — only the server/SaaS product needs
it. See `layer4-products.md` for `MeteredReadAt` and the decorator
pattern. Local operations are never metered.
