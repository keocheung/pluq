# `CoreError` — Tier 1 (Format Parsers)

> Spoke of `docs/ERRORS.md`. Read the switchboard first for the 4-tier
> overview and translation flow.

Core's error type. Produced by format parsers (Layer 2) and the facade
(Layer 3). Contains only format/archive concepts. I/O errors from `ReadAt`
are wrapped as `Io(ReadError)` — core does not inspect or reinterpret them.

---

## Type Definition

```rust
// Owned by archive-core.

/// How a wrong password or decryption failure was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordDetectionMethod {
    /// Fast: Decrypted header bytes didn't match expected patterns (LZMA/Deflate/etc).
    EarlyHeaderValidation,
    /// Slow: Full decompression finished, but the resulting CRC-32 was wrong.
    CrcMismatch,
    /// Technical: The bitstream itself was garbage, causing the decompressor to error out.
    DecompressionFailure,
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum CoreError {
    // --- I/O (from ReadAt calls) ---

    /// Wraps the I/O layer's error. Format parsers do not inspect the contents —
    /// they convert ReadError to CoreError::Io at the call boundary.
    /// The ReadError::Transport box (if present) is preserved inside.
    #[error("I/O error: {0}")]
    Io(ReadError),

    // --- Identification ---

    /// Not an archive or signature mismatch (e.g., "This is a JPEG").
    #[error("invalid archive format: {message}")]
    InvalidFormat { message: String },

    /// Known format, but version too new or variant not handled (e.g., "RAR v5").
    #[error("unsupported archive format: {format}")]
    UnsupportedFormat { format: String },

    /// We can read the format, but don't have the algorithm (e.g., "LZMA", "PPMd").
    #[error("unsupported compression: {algorithm}")]
    UnsupportedCompression { algorithm: String },

    /// Known format/compression, but using a specific feature (e.g., "Multi-volume ZIP").
    #[error("unsupported feature: {detail}")]
    UnsupportedFeature { detail: String },

    // --- Integrity & Processing ---

    /// Metadata is structurally broken (e.g., "ZIP Central Directory is missing bytes").
    #[error("corrupt header at offset {offset:#x}: {reason}")]
    CorruptHeader { offset: u64, reason: String },

    /// The compressed data violates the algorithm's rules (logical corruption).
    #[error("bitstream error: {detail}")]
    BitstreamError { detail: String },

    /// Final integrity check failed (data is wrong or password was incorrect).
    #[error("CRC mismatch for '{entry}': expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { entry: String, expected: u32, actual: u32 },

    // --- Security & Safety ---

    /// Protection against "../../etc/passwd" style attacks.
    #[error("path traversal detected: {path}")]
    PathTraversal { path: String },

    /// Protection against symlinks pointing to sensitive host files.
    #[error("symlink target escape: {path} -> {target}")]
    SymlinkTargetEscape { path: String, target: String },

    /// Protection against "ZIP Bombs" or memory exhaustion (e.g., 1GB LZMA dictionary).
    #[error("resource limit exceeded: {message}")]
    ResourceLimitExceeded { message: String },

    // --- Interaction ---

    /// Triggers a password prompt in the UI.
    #[error("password required")]
    PasswordRequired,

    /// Password was provided but failed (includes detection method for sniping UX).
    #[error("wrong password ({method})")]
    WrongPassword { method: PasswordDetectionMethod },

    /// Signal that the user (or a timeout) stopped the task.
    #[error("operation cancelled")]
    Cancelled,
}
```

---

## Key Design Points

- **No `Transport` variant.** The decorator tunnel lives inside
  `ReadError::Transport`, which arrives at core wrapped in
  `CoreError::Io(ReadError::Transport(box))`. Core never creates,
  inspects, or handles transport errors.
- **No raw `io::Error` or `reqwest::Error`.** All dependency errors are
  translated at their origin into `ReadError` (Layer 0) or `CoreError`
  variants (Layer 2). Nothing foreign leaks through.
- **`#[non_exhaustive]`** forces downstream `match` to include a catch-all
  arm. Safe to add variants in future versions.
- **`Io(ReadError)` replaces old `ReadFailed`.** Format parsers convert
  with `map_err(CoreError::Io)` — one call, no field extraction needed.

---

## Parser Conversion Pattern

What format parsers do with `ReadError` from `ReadAt` calls:

```rust
// In a format parser (header reads use read_range → Bytes):
let header_bytes = self.reader.read_range(offset, len).await
    .map_err(|e| match e {
        ReadError::OutOfBounds { offset, len, size } =>
            CoreError::CorruptHeader {
                offset,
                reason: format!("header read past end: {offset}+{len} > {size}"),
            },
        other => CoreError::Io(other),  // everything else wraps directly
    })?;

// For entry data extraction, use read_stream → AsyncRead:
let stream = self.reader.read_stream(data_offset, compressed_size).await
    .map_err(CoreError::Io)?;
// stream is piped through SyncIoBridge → decoder → sink
// (see architecture/layer1-codec.md § Async/Sync Boundary)
```

`OutOfBounds` is the **only** semantic upconversion at the parser
boundary: if the parser asked for bytes that don't exist, its offset math
was wrong, which means the header data it read earlier was corrupt.
