# `ArchiveError` — Tier 2 (State Layer)

> Spoke of `docs/ERRORS.md`. Read the switchboard first for the 4-tier
> overview and translation flow.

The state layer's error type. Designed for product consumption. Contains
only **universal** concepts that every product needs. No auth, billing,
or quota errors — those are product-specific and travel through the
`Transport` tunnel.

---

## Type Definition

```rust
// Owned by archive.

#[non_exhaustive]
pub enum ArchiveError {
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

---

## Key Design Points

- **No `Archive(CoreError)` wrapping.** Core errors are translated, not
  wrapped. `CrcMismatch` becomes `FileCorrupted`. `PasswordRequired`
  becomes `PasswordNeeded` (or gets handled via prompt and disappears).
- **No product-specific variants.** `AuthenticationFailed` and
  `QuotaExceeded` do not exist here. They belong to product-level error
  types (`ServerError`, `AppError`). Products downcast from
  `ArchiveError::Transport` to find them.
- **`ConnectionFailed` is built from `ReadFailure` context.** The state
  layer reads `source`, `operation`, and `kind` from the `ReadFailure`
  inside `CoreError::Io(ReadError::Failed(..))` and constructs a helpful
  user-facing message. The raw `ReadError` is consumed — products never
  see it.
- **Transport tunnel via Io unwrap.** State layer finds
  `CoreError::Io(ReadError::Transport(box))` and promotes it to
  `ArchiveError::Transport(box)`. Same blind forwarding, one extra
  pattern match.
- **State-layer-only errors** like `CacheInvalid` never touch core.
  They're created and handled entirely within the state layer, or passed
  to the product if it should know.

---

## State-Layer Translation (reference impl)

```rust
// Pseudocode — the state layer's error translation logic.

fn call_core(&self, operation) -> Result<T, ArchiveError> {
    match self.archive.do_operation(operation).await {
        Ok(val) => Ok(val),

        // --- I/O errors: extract ReadError, route by variant ---

        Err(CoreError::Io(ReadError::Transport(err_box))) =>
            // TUNNEL: decorator error. Blind forward to product.
            Err(ArchiveError::Transport(err_box)),

        Err(CoreError::Io(ReadError::Failed(f))) =>
            // BUILD USER MESSAGE from structured ReadFailure context.
            Err(ArchiveError::ConnectionFailed {
                detail: build_user_message(&f)
            }),

        Err(CoreError::Io(ReadError::OutOfBounds { .. })) =>
            // Should have been converted to CorruptHeader by parser.
            // If it reaches here, it's a bug.
            Err(ArchiveError::Internal {
                detail: "unexpected OutOfBounds at state layer".into()
            }),

        // --- Core errors: translate or handle ---

        Err(CoreError::CrcMismatch { entry, .. }) =>
            Err(ArchiveError::FileCorrupted { filename: entry }),

        Err(CoreError::PasswordRequired) => {
            // HANDLE: prompt user for password, retry with password.
            // If user provides correct password → Ok(val), error disappears.
            // If user cancels → Err(ArchiveError::Cancelled)
            // If wrong password → Err(ArchiveError::PasswordNeeded)
        }

        Err(CoreError::WrongPassword { method }) => {
            // HANDLE: detect if fast failure (EarlyHeaderValidation) vs slow
            Err(ArchiveError::PasswordNeeded)
        }

        Err(CoreError::PathTraversal { path }) =>
            Err(ArchiveError::SecurityRisk {
                detail: format!("dangerous path: {path}")
            }),

        Err(CoreError::SymlinkTargetEscape { path, target }) =>
            Err(ArchiveError::SecurityRisk {
                detail: format!("dangerous symlink: {path} -> {target}")
            }),

        Err(CoreError::UnsupportedCompression { algorithm }) =>
            Err(ArchiveError::UnsupportedFormat { detail: algorithm }),

        Err(CoreError::UnsupportedFormat { format }) =>
            Err(ArchiveError::UnsupportedFormat { detail: format }),

        Err(CoreError::InvalidFormat { message }) =>
            Err(ArchiveError::UnsupportedFormat { detail: message }),

        Err(CoreError::UnsupportedFeature { detail }) =>
            Err(ArchiveError::UnsupportedFormat { detail }),

        Err(CoreError::BitstreamError { detail }) =>
            Err(ArchiveError::FileCorrupted { filename: format!("decompression: {detail}") }),

        Err(CoreError::CorruptHeader { reason, .. }) =>
            Err(ArchiveError::FileCorrupted { filename: format!("header: {reason}") }),

        Err(CoreError::Cancelled) =>
            Err(ArchiveError::Cancelled),

        Err(other) =>
            Err(ArchiveError::Internal { detail: other.to_string() }),
    }
}
```

---

## `build_user_message` — ReadFailure → human text

```rust
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
