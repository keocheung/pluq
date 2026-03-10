# ArchiveSniper — Error Architecture

> **Source of truth** for how errors flow across layers.
> Last updated: 2026-03-10.
> See also: `ARCHITECTURE.md` (system architecture), `CONTEXT.md` (project state).

---

## Philosophy

**Each layer defines errors for its consumer.** No layer exposes a lower layer's error types. Errors are either **handled** (retry, prompt, recover — the error disappears) or **translated** into the consumer's vocabulary. Never wrapped.

**If you can handle it, handle it.** An error only travels upward when the current layer genuinely cannot deal with it. Many errors never reach the UI because an intermediate layer resolved them.

**The unhandleable falls to a catch-all.** If no layer has a specific response for an error, it becomes a generic "something went wrong" with enough detail for logging/debugging. The user gets a human message, not a stack trace.

**Errors carry context for their audience.** I/O errors carry source paths/URLs and structured failure reasons so decorators can route programmatically AND products can generate helpful user messages — without string parsing.

---

## The Four Error Tiers

```
╔════════════════════════════════════════════════════════════════════╗
║  Layer 4 (Products) sees: StateError                                ║
║    - Handles universal errors directly                              ║
║    - Opens StateError::Transport for decorator errors               ║
║    - Defines own error type (ServerError, AppError, etc.)           ║
╠════════════════════════════════════════════════════════════════════╣
║  Layer 3.5 (State Layer)                                            ║
║    - Receives ArchiveError from core                                ║
║    - Handles/translates core errors it understands                  ║
║    - Finds ReadError::Transport inside ArchiveError::Io             ║
║      and tunnels it blindly as StateError::Transport                ║
║    - Creates its own errors (cache, session state)                  ║
╠════════════════════════════════════════════════════════════════════╣
║  Layers 2–3 (Format Parsers + Facade) produce: ArchiveError         ║
║    - Every variant is a format/archive concept                      ║
║    - Io(ReadError) wraps I/O errors from ReadAt calls               ║
║    - No Transport variant — tunnel lives inside ReadError            ║
╠════════════════════════════════════════════════════════════════════╣
║  Layer 0 (I/O) produces: ReadError                                  ║
║    - Standalone I/O error language (local + remote)                 ║
║    - Structured context for routing (decorators) AND display (UI)   ║
║    - Transport tunnel: decorators inject opaque boxed errors here    ║
╚════════════════════════════════════════════════════════════════════╝
```

---

## Tier 0: `ReadError` (I/O Layer)

The I/O layer's own error language. Returned by all `ReadAt` methods (`read_range()`, `read_stream()`, `size()`). Covers both local filesystem and HTTP failures. Designed for two consumers:

1. **Decorators** — need structured `ReadErrorKind` to route programmatically (retry? refresh token? give up?)
2. **Users** (via products) — need `source`, `operation`, and `kind` to generate helpful messages

```rust
// Owned by archive-core (Layer 0).

pub enum ReadError {
    /// An actual I/O failure — network or filesystem.
    /// Boxed because ReadFailure is 144 bytes (keeps ReadError at 32 bytes on the stack).
    Failed(Box<ReadFailure>),

    /// Opaque tunnel for decorator errors (Auth, Retry, Quota).
    /// Decorators inject their own boxed error types here.
    /// Core and state layer carry this blindly. Products open and downcast.
    Transport(Box<dyn Error + Send + Sync>),

    /// Requested range exceeds source size (HTTP 416, read past EOF).
    /// Format parsers convert this to ArchiveError::CorruptHeader —
    /// if the parser asked for nonexistent bytes, the header data was wrong.
    OutOfBounds { offset: u64, len: u64, size: u64 },
}
```

### ReadFailure — full context for every I/O error

```rust
pub struct ReadFailure {
    /// Which source failed. Critical when MultiVolumeReadAt is involved:
    /// "I/O error reading volume 3 (archive.part3.rar)" instead of
    /// "something failed somewhere."
    pub source: SourceInfo,

    /// What the I/O layer was doing when it failed.
    pub operation: ReadOperation,

    /// Structured failure reason. Decorators match on this for routing.
    /// Products use this + source + operation to build user messages.
    pub kind: ReadErrorKind,

    /// Raw diagnostic from the underlying library (reqwest, tokio::fs, etc.).
    /// For logs and debugging only — never shown to users directly.
    pub diagnostic: String,
}

pub enum SourceInfo {
    LocalFile { path: PathBuf },
    Http      { url: String },
}

pub enum ReadOperation {
    /// Initial open / connection establishment.
    Open,
    /// HEAD request or file stat to determine total size.
    SizeCheck,
    /// Actual byte-range read.
    ReadRange { offset: u64, len: u64 },
}
```

### ReadErrorKind — the routing + display language

Every variant carries enough structured data so that:
- **Decorators** can match and decide without parsing strings
- **Products** can build specific, helpful user messages

```rust
pub enum ReadErrorKind {
    // ── Network transport (TCP/TLS level — no HTTP status code) ──────────

    /// TCP connection or data transfer timed out.
    /// User message: "Connection to Dropbox timed out. Check your internet."
    NetworkTimeout { phase: NetworkPhase },

    /// TCP connection was reset during data transfer.
    /// User message: "Connection dropped while downloading. [Retry]"
    NetworkReset,

    /// TLS handshake failed (cert expired, self-signed, hostname mismatch).
    /// User message: "SSL certificate error for dropbox.com: {reason}"
    TlsFailure { reason: String },

    /// DNS resolution failed — likely offline or bad hostname.
    /// User message: "Cannot reach 'example.com'. Check your internet connection."
    DnsFailure { hostname: String },

    // ── HTTP application level ───────────────────────────────────────────

    /// Server returned an HTTP error status.
    /// Decorators inspect `code` for routing:
    ///   - 401 → AuthReadAt refreshes token
    ///   - 429/503 → RetryReadAt checks Retry-After header, backs off
    ///   - 404 → nobody handles, fatal
    /// `body` often contains XML/JSON from cloud providers explaining why.
    /// User message: "Dropbox returned error 403: {parsed body or code meaning}"
    HttpStatus {
        code:    u16,
        headers: Vec<(String, String)>,
        body:    Option<Bytes>,
    },

    /// Server returned 200 OK instead of 206 Partial Content.
    /// Cloud sniping is impossible — this URL does not support range requests.
    /// User message: "This server does not support partial downloads."
    RangeNotSupported,

    // ── Local filesystem ─────────────────────────────────────────────────

    /// OS denied read access to the file.
    /// User message: "Permission denied: cannot read '{path}'"
    PermissionDenied,

    /// File no longer exists (deleted or moved after we started).
    /// User message: "'{path}' was moved or deleted while reading."
    FileNotFound,

    /// Hardware or OS-level I/O error (bad sector, failing drive).
    /// Retrying will not help. `os_code` for diagnostics.
    /// User message: "Disk error reading '{path}'. The storage device may be failing."
    DiskIoError { os_code: Option<i32> },

    /// Another process holds an exclusive lock (common on Windows).
    /// Brief retry may resolve this.
    /// User message: "'{path}' is locked by another program. Close it and retry."
    FileLocked,

    /// System call interrupted (EINTR). Safe to retry immediately.
    Interrupted,
}

pub enum NetworkPhase {
    /// Never reached the server — TCP connect failed or timed out.
    Connect,
    /// Reached the server, TLS handshake failed.
    Tls,
    /// Connection was established, data transfer was interrupted.
    Transfer,
}
```

### How decorators use ReadErrorKind

Decorators implement `ReadAt`. They call `self.inner.read_range()`, get `Result<_, ReadError>`, inspect the kind, and decide:

```rust
// RetryReadAt — retry transient failures with backoff.
fn is_retryable(kind: &ReadErrorKind) -> bool {
    matches!(kind,
        ReadErrorKind::NetworkTimeout { .. }
        | ReadErrorKind::NetworkReset
        | ReadErrorKind::Interrupted
        | ReadErrorKind::FileLocked
        | ReadErrorKind::HttpStatus { code: 429 | 503, .. }
    )
}

fn retry_delay(kind: &ReadErrorKind) -> Duration {
    match kind {
        ReadErrorKind::HttpStatus { code: 429, ref headers, .. } =>
            parse_retry_after(headers).unwrap_or(Duration::from_secs(5)),
        ReadErrorKind::Interrupted => Duration::ZERO,
        ReadErrorKind::FileLocked => Duration::from_millis(200),
        _ => Duration::from_secs(2),  // default backoff
    }
}
```

```rust
// AuthReadAt — intercept 401, attempt token refresh.
// Shows read_range; read_stream follows the same pattern.
impl ReadAt for AuthReadAt {
    async fn read_range(&self, offset: u64, len: u64)
        -> Result<Bytes, ReadError>
    {
        match self.inner.read_range(offset, len).await {
            Ok(bytes) => Ok(bytes),
            Err(ReadError::Failed(ref f))
                if matches!(f.kind, ReadErrorKind::HttpStatus { code: 401, .. }) =>
            {
                // Try refreshing the token
                match self.token_provider.refresh().await {
                    Ok(new_token) => {
                        self.update_headers(new_token);
                        self.inner.read_range(offset, len).await  // retry with new token
                    }
                    Err(e) => Err(ReadError::Transport(
                        Box::new(AuthError::RefreshFailed { detail: e.to_string() })
                    )),
                }
            }
            Err(other) => Err(other),  // pass through
        }
    }

    async fn read_stream(&self, offset: u64, len: u64)
        -> Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>
    {
        // Same 401-intercept pattern as read_range.
        // Retries the connection; once a stream is returned, mid-stream errors
        // are the caller's problem (retry at that level is not possible).
        match self.inner.read_stream(offset, len).await {
            Ok(stream) => Ok(stream),
            Err(ReadError::Failed(ref f))
                if matches!(f.kind, ReadErrorKind::HttpStatus { code: 401, .. }) =>
            {
                match self.token_provider.refresh().await {
                    Ok(new_token) => {
                        self.update_headers(new_token);
                        self.inner.read_stream(offset, len).await
                    }
                    Err(e) => Err(ReadError::Transport(
                        Box::new(AuthError::RefreshFailed { detail: e.to_string() })
                    )),
                }
            }
            Err(other) => Err(other),
        }
    }
}
```

---

## Tier 0.5: Decorator Errors (Horizontal Crates)

Created by `ReadAt` decorators from horizontal crates (e.g., `archive-auth`, `archive-retry`). Each horizontal crate defines its **own error type** — there is no single shared enum. Each crate is independent.

```rust
// Owned by archive-auth crate.
#[non_exhaustive]
enum AuthError {
    TokenExpired,
    RefreshFailed { detail: String },
}

// Owned by server crate (not shared).
#[non_exhaustive]
enum QuotaError {
    Hit { used: u64, limit: u64 },
}

// Owned by archive-retry crate.
#[non_exhaustive]
enum RetryError {
    Exhausted { attempts: u32 },
}
```

Each error type implements `std::error::Error + Send + Sync`. Decorators box them into `ReadError::Transport`. The box travels through core (as `ArchiveError::Io(ReadError::Transport(box))`) and the state layer (as `StateError::Transport(box)`) untouched. The product at the top opens the box and downcasts to the types it knows from its own dependencies.

---

## Tier 1: `ArchiveError` (Core)

Core's error type. Produced by format parsers (Layer 2) and the facade (Layer 3). Contains only format/archive concepts. I/O errors from `ReadAt` are wrapped as `Io(ReadError)` — core does not inspect or reinterpret them.

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
pub enum ArchiveError {
    // --- I/O (from ReadAt calls) ---

    /// Wraps the I/O layer's error. Format parsers do not inspect the contents —
    /// they convert ReadError to ArchiveError::Io at the call boundary.
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

**Key design points:**

- **No `Transport` variant.** The decorator tunnel lives inside `ReadError::Transport`, which arrives at core wrapped in `ArchiveError::Io(ReadError::Transport(box))`. Core never creates, inspects, or handles transport errors.
- **No raw `io::Error` or `reqwest::Error`.** All dependency errors are translated at their origin into `ReadError` (Layer 0) or `ArchiveError` variants (Layer 2). Nothing foreign leaks through.
- **`#[non_exhaustive]`** forces downstream `match` to include a catch-all arm. Safe to add variants in future versions.
- **`Io(ReadError)` replaces old `ReadFailed`.** Format parsers convert with `map_err(ArchiveError::Io)` — one call, no field extraction needed.

**What format parsers do with `ReadError` from `ReadAt` calls:**

```rust
// In a format parser (header reads use read_range → Bytes):
let header_bytes = self.reader.read_range(offset, len).await
    .map_err(|e| match e {
        ReadError::OutOfBounds { offset, len, size } =>
            ArchiveError::CorruptHeader {
                offset,
                reason: format!("header read past end: {offset}+{len} > {size}"),
            },
        other => ArchiveError::Io(other),  // everything else wraps directly
    })?;

// For entry data extraction, use read_stream → AsyncRead:
let stream = self.reader.read_stream(data_offset, compressed_size).await
    .map_err(ArchiveError::Io)?;
// stream is piped through SyncIoBridge → decoder → sink (see ARCHITECTURE.md § Async/Sync Boundary)
```

---

## Tier 2: `StateError` (State Layer)

The state layer's error type. Designed for product consumption. Contains only **universal** concepts that every product needs. No auth, billing, or quota errors — those are product-specific and travel through the `Transport` tunnel.

```rust
// Owned by archive-state.

#[non_exhaustive]
pub enum StateError {
    /// A file in the archive is damaged / failed integrity check.
    FileCorrupted { filename: String },

    /// Password is needed to access the archive or entry.
    PasswordNeeded,

    /// The archive format or compression method isn't supported.
    UnsupportedFormat { detail: String },

    /// The archive contains potentially dangerous paths.
    SecurityRisk { detail: String },

    /// Network or I/O communication failed.
    /// Contains a user-friendly message built from ReadFailure context.
    ConnectionFailed { detail: String },

    /// The operation was cancelled by the user.
    Cancelled,

    /// Cached data is invalid or unreadable (non-fatal, triggers re-parse).
    CacheInvalid,

    /// An unexpected internal error. Logged for debugging.
    Internal { detail: String },

    /// Opaque tunnel for decorator errors that passed through core untouched.
    /// The product opens this box and downcasts to types it knows.
    Transport(Box<dyn std::error::Error + Send + Sync>),
}
```

**Key design points:**

- **No `Archive(ArchiveError)` wrapping.** Core errors are translated, not wrapped. `CrcMismatch` becomes `FileCorrupted`. `PasswordRequired` becomes `PasswordNeeded` (or gets handled via prompt and disappears).
- **No product-specific variants.** `AuthenticationFailed` and `QuotaExceeded` do not exist here. They belong to product-level error types (`ServerError`, `AppError`). Products downcast from `StateError::Transport` to find them.
- **`ConnectionFailed` is built from `ReadFailure` context.** The state layer reads `source`, `operation`, and `kind` from the `ReadFailure` inside `ArchiveError::Io(ReadError::Failed(..))` and constructs a helpful user-facing message. The raw `ReadError` is consumed — products never see it.
- **Transport tunnel via Io unwrap.** State layer finds `ArchiveError::Io(ReadError::Transport(box))` and promotes it to `StateError::Transport(box)`. Same blind forwarding, one extra pattern match.
- **State-layer-only errors** like `CacheInvalid` never touch core. They're created and handled entirely within the state layer, or passed to the product if it should know.

---

## The Translation Flow

### State layer: translate core errors, tunnel decorator errors, build user messages

```rust
// Pseudocode — the state layer's error translation logic.

fn call_core(&self, operation) -> Result<T, StateError> {
    match self.archive.do_operation(operation).await {
        Ok(val) => Ok(val),

        // --- I/O errors: extract ReadError, route by variant ---

        Err(ArchiveError::Io(ReadError::Transport(err_box))) =>
            // TUNNEL: decorator error. Blind forward to product.
            Err(StateError::Transport(err_box)),

        Err(ArchiveError::Io(ReadError::Failed(f))) =>
            // BUILD USER MESSAGE from structured ReadFailure context.
            Err(StateError::ConnectionFailed {
                detail: build_user_message(&f)
            }),

        Err(ArchiveError::Io(ReadError::OutOfBounds { .. })) =>
            // Should have been converted to CorruptHeader by parser.
            // If it reaches here, it's a bug.
            Err(StateError::Internal {
                detail: "unexpected OutOfBounds at state layer".into()
            }),

        // --- Core errors: translate or handle ---

        Err(ArchiveError::CrcMismatch { entry, .. }) =>
            Err(StateError::FileCorrupted { filename: entry }),

        Err(ArchiveError::PasswordRequired) => {
            // HANDLE: prompt user for password, retry with password.
            // If user provides correct password → Ok(val), error disappears.
            // If user cancels → Err(StateError::Cancelled)
            // If wrong password → Err(StateError::PasswordNeeded)
        }

        Err(ArchiveError::WrongPassword { method }) => {
            // HANDLE: detect if fast failure (EarlyHeaderValidation) vs slow
            Err(StateError::PasswordNeeded)
        }

        Err(ArchiveError::PathTraversal { path }) =>
            Err(StateError::SecurityRisk {
                detail: format!("dangerous path: {path}")
            }),

        Err(ArchiveError::SymlinkTargetEscape { path, target }) =>
            Err(StateError::SecurityRisk {
                detail: format!("dangerous symlink: {path} -> {target}")
            }),

        Err(ArchiveError::UnsupportedCompression { algorithm }) =>
            Err(StateError::UnsupportedFormat { detail: algorithm }),

        Err(ArchiveError::UnsupportedFormat { format }) =>
            Err(StateError::UnsupportedFormat { detail: format }),

        Err(ArchiveError::InvalidFormat { message }) =>
            Err(StateError::UnsupportedFormat { detail: message }),

        Err(ArchiveError::UnsupportedFeature { detail }) =>
            Err(StateError::UnsupportedFormat { detail }),

        Err(ArchiveError::BitstreamError { detail }) =>
            Err(StateError::FileCorrupted { filename: format!("decompression: {detail}") }),

        Err(ArchiveError::CorruptHeader { reason, .. }) =>
            Err(StateError::FileCorrupted { filename: format!("header: {reason}") }),

        Err(ArchiveError::Cancelled) =>
            Err(StateError::Cancelled),

        Err(other) =>
            Err(StateError::Internal { detail: other.to_string() }),
    }
}

/// Build a user-facing message from structured ReadFailure context.
fn build_user_message(f: &ReadFailure) -> String {
    let source_name = match &f.source {
        SourceInfo::LocalFile { path } => path.display().to_string(),
        SourceInfo::Http { url } => url.clone(),
    };
    match &f.kind {
        ReadErrorKind::NetworkTimeout { phase: NetworkPhase::Connect } =>
            format!("Connection to '{}' timed out. Check your internet.", source_name),
        ReadErrorKind::NetworkTimeout { phase: NetworkPhase::Transfer } =>
            format!("Connection to '{}' dropped during download.", source_name),
        ReadErrorKind::TlsFailure { reason } =>
            format!("SSL error for '{}': {}", source_name, reason),
        ReadErrorKind::DnsFailure { hostname } =>
            format!("Cannot reach '{}'. Check your internet connection.", hostname),
        ReadErrorKind::HttpStatus { code: 404, .. } =>
            format!("File not found at '{}' (HTTP 404).", source_name),
        ReadErrorKind::HttpStatus { code, .. } =>
            format!("Server error {} from '{}'.", code, source_name),
        ReadErrorKind::RangeNotSupported =>
            format!("'{}' does not support partial downloads. Cloud sniping unavailable.", source_name),
        ReadErrorKind::PermissionDenied =>
            format!("Permission denied: cannot read '{}'.", source_name),
        ReadErrorKind::FileNotFound =>
            format!("'{}' was moved or deleted.", source_name),
        ReadErrorKind::DiskIoError { os_code } =>
            format!("Disk error reading '{}'. The storage device may be failing. (code: {:?})", source_name, os_code),
        ReadErrorKind::FileLocked =>
            format!("'{}' is locked by another program.", source_name),
        _ =>
            format!("I/O error reading '{}': {}", source_name, f.diagnostic),
    }
}
```

### Product layer: open the tunnel, handle what you know

The product assembled the decorator stack. It knows what error types are possible because it depends on those horizontal crates. It opens `StateError::Transport`, tries to downcast, handles or reports.

```rust
// Pseudocode — product-level error handling (e.g., server).

fn handle_state_result<T>(&self, result: Result<T, StateError>) -> Result<T, ServerError> {
    match result {
        Ok(val) => Ok(val),

        // --- OPEN THE TUNNEL: downcast to types I know about ---

        Err(StateError::Transport(err_box)) => {
            // I composed AuthReadAt, so I know AuthError exists
            if let Some(auth) = err_box.downcast_ref::<AuthError>() {
                match auth {
                    AuthError::TokenExpired => {
                        // HANDLE: refresh token, retry
                        // if works: Ok(val) — error disappears
                        // if fails: Err(ServerError::AuthenticationFailed)
                    }
                    AuthError::RefreshFailed { .. } =>
                        Err(ServerError::AuthenticationFailed),
                }
            }
            // I composed MeteredReadAt, so I know QuotaError exists
            else if let Some(quota) = err_box.downcast_ref::<QuotaError>() {
                match quota {
                    QuotaError::Hit { used, limit } =>
                        Err(ServerError::QuotaExceeded { used: *used, limit: *limit }),
                }
            }
            // Unknown — nothing I composed produced this. Log and abort.
            else {
                Err(ServerError::Internal {
                    detail: format!("unexpected transport error: {err_box}")
                })
            }
        }

        // --- STATE ERRORS: translate or forward ---

        Err(StateError::FileCorrupted { filename }) =>
            Err(ServerError::FileCorrupted { filename }),
        Err(StateError::PasswordNeeded) =>
            Err(ServerError::PasswordNeeded),
        // ... remaining variants ...
        Err(other) =>
            Err(ServerError::Internal { detail: other.to_string() }),
    }
}
```

**Key insight:** A CLI that doesn't compose any decorators will never receive `StateError::Transport`. It can map that variant straight to `Internal` — it's structurally unreachable. The tunnel only carries what you put into it.

---

## Error Flow Summary

```
Layer 0 (I/O)           Layer 2 (Parser)          Layer 3.5 (State)        Layer 4 (Product)
ReadError               ArchiveError              StateError               Product error / UI
-----------             ------------              ----------               ------------------

ReadError::Failed       → Io(ReadError::Failed)   → ConnectionFailed       → "Connection timed out"
  kind: NetworkTimeout                               (user message from        (shown to user)
  source: Http{url}                                   ReadFailure context)
  diagnostic: "..."

ReadError::Failed       → Io(ReadError::Failed)   → ConnectionFailed       → "Permission denied:
  kind: PermissionDenied                              (user message)            cannot read '...'"
  source: LocalFile{path}

ReadError::OutOfBounds  → CorruptHeader           → FileCorrupted          → "File is damaged"
  (semantic upconversion)

                          InvalidFormat            → UnsupportedFormat      → "Not an archive"
                          UnsupportedFormat        → UnsupportedFormat      → "Can't read format"
                          UnsupportedCompression   → UnsupportedFormat      → "Can't decompress"
                          UnsupportedFeature       → UnsupportedFormat      → "Feature missing"

                          CorruptHeader            → FileCorrupted          → "File is damaged"
                          BitstreamError           → FileCorrupted          → "Extraction failed"
                          CrcMismatch              → FileCorrupted          → "Integrity check failed"

                          PasswordRequired         → [handle: prompt]       → (disappears)
                                                   → PasswordNeeded         → password dialog
                          WrongPassword            → [handle: retry]        → (disappears)

                          PathTraversal            → SecurityRisk           → "Dangerous archive"
                          SymlinkTargetEscape      → SecurityRisk           → "Dangerous link"
                          ResourceLimitExceeded    → Internal               → "Resource limit hit"

                          Cancelled                → Cancelled              → (operation stopped)

--- Decorator tunnel (Transport box journey) ---

AuthDecorator sees 401:
  ReadError::Transport(AuthError::TokenExpired)
  → ArchiveError::Io(ReadError::Transport(box))
  → StateError::Transport(box)          [blind forward]
  → Product opens: downcast to AuthError → refresh token → (disappears)
                                                          or "Sign in again"

MeteredDecorator:
  ReadError::Transport(QuotaError::Hit)
  → ArchiveError::Io(ReadError::Transport(box))
  → StateError::Transport(box)          [blind forward]
  → Product opens: QuotaExceeded → "Buy more credits"

KEY:
  "→"             = TRANSLATION (new type, old consumed)
  "[handle: ...]" = error handled here, may never reach product
  "(disappears)"  = retry/prompt succeeded, product never sees it
  "[blind forward]" = layer re-wraps without inspecting the box
```

---

## Decorator Construction (Product Assembles the Stack)

The **product** constructs the decorator stack using horizontal crates, then passes the composed `ReadAt` to the state layer. The state layer passes it to core. Neither the state layer nor core knows about decorators.

```rust
// Server product builds the stack:

fn open_cloud_archive(&self, url: &str) -> Result<Archive, ServerError> {
    // Bottom: core's dumb HTTP reader (streams response, returns ReadError)
    let http = HttpReadAt::new(url);

    // Decorate: product-level concerns from horizontal crates
    let authed  = AuthReadAt::new(http, self.token_provider.clone());   // archive-auth
    let retried = RetryReadAt::new(authed, self.retry_policy.clone());  // archive-retry
    let metered = MeteredReadAt::new(retried, self.quota_tracker.clone()); // server-internal

    // Pass composed ReadAt to state layer — returns Archive (the product-facing handle)
    let archive = self.state.open(metered).await?;  // returns Result<Archive, StateError>

    Ok(archive)
}
```

```
What core sees:          impl ReadAt ──── "give me bytes / give me a stream"
What's actually there:   MeteredReadAt → RetryReadAt → AuthReadAt → HttpReadAt
                         (server)        (retry crate)  (auth crate)  (core)

Error path:  HttpReadAt returns ReadError::Failed(kind: HttpStatus{401})
             AuthReadAt intercepts → refreshes token → retries
             If refresh fails → ReadError::Transport(AuthError::RefreshFailed)
             RetryReadAt sees Transport → not retryable, passes through
             MeteredReadAt sees Transport → not its concern, passes through
             Parser wraps as ArchiveError::Io(ReadError::Transport(box))
             State layer → StateError::Transport(box)
             Server opens box → downcast to AuthError → ServerError::AuthFailed
```

A KMP app's stack might look completely different:

```rust
// KMP app — OAuth handled in Kotlin, passed as static header
fn open_cloud_archive(&self, url: &str, bearer_token: &str) -> Result<Archive, AppError> {
    let http = HttpReadAt::new(url)
        .with_headers(vec![("Authorization", format!("Bearer {bearer_token}"))]);

    // No auth decorator, no metering — just hand to state layer
    let archive = self.state.open(http).await?;
    Ok(archive)
}
```

---

## Platform Notes

Error types are pure Rust enums — fully cross-platform. Platform-specific concerns:

- **Desktop (KMP):** `StateError` crosses FFI via UniFFI into Kotlin. Product-level errors (`AppError`) are defined in Kotlin. The UniFFI surface only exposes `StateError` — clean, no phantom variants for quota/auth that can't occur on local-only usage.
- **iOS/Android (KMP):** same as desktop — Compose Multiplatform UI, Rust core via UniFFI. OAuth may be handled entirely in Kotlin/Swift, bypassing Rust-side auth decorators.
- **Server:** pure Rust, no FFI. `ServerError` extends `StateError` with billing/auth variants.
- **CLI:** pure Rust. `CliError` is minimal — most `StateError` variants map directly. `StateError::Transport` is structurally unreachable (CLI doesn't compose decorators).

The state layer's logic (translation, caching, user-message building) is pure cross-platform Rust. Decorators and product-level error handling are composed at the product level — each product picks what it needs.
