# `ReadError` — Tier 0 (I/O Layer)

> Spoke of `docs/ERRORS.md`. Read the switchboard first for the 4-tier
> overview and translation flow.

The I/O layer's own error language. Returned by every `ReadAt` method
(`read_range()`, `read_stream()`, `size()`). Covers both local filesystem
and HTTP failures. Designed for two consumers:

1. **Decorators** — need structured `ReadErrorKind` to route programmatically
   (retry? refresh token? give up?).
2. **Users** (via products) — need `source`, `operation`, and `kind` to
   generate helpful messages.

---

## Type Definitions

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
    /// Format parsers convert this to CoreError::CorruptHeader —
    /// if the parser asked for nonexistent bytes, the header data was wrong.
    OutOfBounds { offset: u64, len: u64, size: u64 },
}
```

### `ReadFailure` — full context for every I/O error

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

---

## `ReadErrorKind` — Routing + Display Language

Every variant carries enough structured data so that:

- **Decorators** can match and decide without parsing strings.
- **Products** can build specific, helpful user messages.

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

---

## Decorator Routing Examples

Decorators implement `ReadAt`. They call `self.inner.read_range()`, get
`Result<_, ReadError>`, inspect the kind, and decide.

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

See `transport-tunnel.md` for how `ReadError::Transport` boxes flow up
through the layers and get downcast at the product level.
