# ArchiveSniper — Error Architecture

> **Source of truth** for how errors flow across layers.
> Last updated: 2026-03-04.
> See also: `ARCHITECTURE.md` (system architecture), `CONTEXT.md` (project state).

---

## Philosophy

**Each layer defines errors for its consumer.** No layer exposes a lower layer's error types. Errors are either **handled** (retry, prompt, recover — the error disappears) or **translated** into the consumer's vocabulary. Never wrapped.

**If you can handle it, handle it.** An error only travels upward when the current layer genuinely cannot deal with it. Many errors never reach the UI because an intermediate layer resolved them.

**The unhandleable falls to a catch-all.** If no layer has a specific response for an error, it becomes a generic "something went wrong" with enough detail for logging/debugging. The user gets a human message, not a stack trace.

---

## The Three Error Tiers

```
╔════════════════════════════════════════════════════════════╗
║  Layer 4 (Products) sees: StateError                       ║
║    - Handles universal errors directly                     ║
║    - Opens StateError::Transport tunnel for decorator errs ║
║    - Defines own error type (ServerError, AppError, etc.)  ║
╠════════════════════════════════════════════════════════════╣
║  Layer 3.5 (State Layer)                                   ║
║    - Receives ArchiveError from core                       ║
║    - Handles/translates core errors it understands         ║
║    - Tunnels Transport BLINDLY (re-wraps, does not open)   ║
║    - Creates its own errors (cache, session state)         ║
╠════════════════════════════════════════════════════════════╣
║  Layers 0–3 (Core) produce ONLY: ArchiveError              ║
║    - Every variant is a format/I/O concept                 ║
║    - Transport tunnel carries opaque decorator errors      ║
║    - Core does not create or inspect Transport             ║
╚════════════════════════════════════════════════════════════╝
```

---

## Tier 1: Decorator Errors (Horizontal Crates)

Created by `ReadAt` decorators from horizontal crates (e.g., `archive-auth`, `archive-retry`). These decorators are assembled by the product (Layer 4), not by the state layer. Each horizontal crate defines its **own error type**. There is no single shared enum — each crate is independent.

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

Each error type implements `std::error::Error + Send + Sync`. They are boxed into `ArchiveError::Transport` by the decorator that creates them. Core carries the box blindly. The state layer carries it blindly. The product at the top opens the box, downcasts to the types it knows from its own dependencies.

**How they enter the tunnel:**

```rust
// A decorator intercepts a lower-level error and replaces it
// with its own boxed error inside the Transport tunnel.

// In archive-auth crate:
impl ReadAt for AuthReadAt {
    async fn read_range(&self, offset: u64, len: u64) -> Result<Bytes, ArchiveError> {
        match self.inner.read_range(offset, len).await {
            Ok(bytes) => Ok(bytes),
            Err(ArchiveError::ReadFailed { ref detail, .. }) if is_http_401(detail) => {
                Err(ArchiveError::Transport(
                    Box::new(AuthError::TokenExpired)
                ))
            }
            Err(other) => Err(other), // pass through
        }
    }
}
```

---

## Tier 2: `ArchiveError` (Core)

Core's error type. Designed for the state layer's consumption. Contains only format/I/O concepts that core understands. The `Transport` variant is an opaque tunnel — core carries it without inspecting it.

```rust
// Owned by archive-core.

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// A byte-level read failed. Translates io::Error / HTTP errors into core vocabulary.
    #[error("read failed at offset {offset} len {len}: {detail}")]
    ReadFailed { offset: u64, len: u64, detail: String },

    /// Archive structure is invalid or unrecognizable.
    #[error("invalid archive format: {message}")]
    InvalidFormat { message: String },

    /// Decompressed data doesn't match expected checksum.
    #[error("CRC mismatch for '{entry_name}': expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { entry_name: String, expected: u32, actual: u32 },

    /// Entry is encrypted and no password was provided.
    #[error("password required")]
    PasswordRequired,

    /// Password was provided but decryption/verification failed.
    #[error("wrong password or corrupted data")]
    WrongPassword,

    /// Compression algorithm not supported by this build.
    #[error("unsupported compression method: {method}")]
    UnsupportedMethod { method: String },

    /// Entry path attempts to escape the output directory.
    #[error("path traversal detected: {path}")]
    PathTraversal { path: String },

    /// Safety limit exceeded (entry count, dict size, ratio, etc.)
    #[error("resource limit exceeded: {message}")]
    ResourceLimitExceeded { message: String },

    /// Opaque tunnel for state-layer / transport errors.
    /// Core does not create or inspect this variant — it only bubbles it up.
    #[error("transport error: {0}")]
    Transport(Box<dyn std::error::Error + Send + Sync>),

    /// Operation was cancelled (via ProgressReporter::should_cancel).
    #[error("operation cancelled")]
    Cancelled,
}
```

**Key design points:**

- **No `Io(std::io::Error)` variant.** Raw `io::Error` is translated into `ReadFailed` at the point of failure, with context (offset, length, human-readable detail). The raw OS error is consumed, not forwarded.
- **`#[non_exhaustive]`** forces downstream `match` to include a catch-all arm. Safe to add variants in future versions.
- **`Transport` is a tunnel, not a category.** Core never creates `Transport` errors. Decorators (horizontal crate code implementing `ReadAt`) create them. Core's format parsers see `Transport` errors from `read_range()` calls and bubble them up untouched.

**What core does with errors it receives from `ReadAt`:**

- `ReadFailed` → can't handle, bubbles up
- `Transport(box)` → can't handle, bubbles up (doesn't inspect the box)
- Core **creates** its own variants: `InvalidFormat`, `CrcMismatch`, `PasswordRequired`, etc.

---

## Tier 3: `StateError` (State Layer)

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
    ConnectionFailed { detail: String },

    /// The operation was cancelled by the user.
    Cancelled,

    /// Cached data is invalid or unreadable (non-fatal, triggers re-parse).
    CacheInvalid,

    /// An unexpected internal error. Logged for debugging.
    Internal { detail: String },

    /// Opaque tunnel for errors the state layer couldn't handle.
    /// Carries decorator errors from horizontal crates (auth, quota, etc.)
    /// that passed through core and state layer untouched.
    /// The product at the top opens this box and downcasts.
    Transport(Box<dyn std::error::Error + Send + Sync>),
}
```

**Key design points:**

- **No `Archive(ArchiveError)` wrapping.** Core errors are translated, not wrapped. `CrcMismatch` becomes `FileCorrupted`. `PasswordRequired` becomes `PasswordNeeded` (or gets handled via prompt and disappears).
- **No product-specific variants.** `AuthenticationFailed` and `QuotaExceeded` do not exist here. They belong to product-level error types (`ServerError`, `AppError`). Products downcast from `StateError::Transport` to find them.
- **Two-hop tunnel.** `StateError::Transport` mirrors `ArchiveError::Transport` — same mechanics, one layer up. The state layer does not inspect the box. It re-wraps `ArchiveError::Transport(box)` as `StateError::Transport(box)`. The product opens it.
- **State-layer-only errors** like `CacheInvalid` never touch core. They're created and handled entirely within the state layer, or passed to the product if it should know.

---

## The Translation Flow

### State layer: handle core errors, tunnel the rest

The state layer understands only `ArchiveError` variants that core created. `Transport` is not a core error — it's a box injected by a decorator. The state layer re-wraps it blindly.

```rust
// Pseudocode — the state layer's error translation logic.

fn call_core(&self, operation) -> Result<T, StateError> {
    match self.archive.do_operation(operation).await {
        Ok(val) => Ok(val),

        // --- TUNNEL: state layer does NOT open the box. Blind forward. ---

        Err(ArchiveError::Transport(err_box)) =>
            Err(StateError::Transport(err_box)),

        // --- CORE ERRORS: handle or translate ---

        Err(ArchiveError::CrcMismatch { entry_name, .. }) =>
            Err(StateError::FileCorrupted { filename: entry_name }),

        Err(ArchiveError::PasswordRequired) => {
            // HANDLE: prompt user for password, retry
            let password = self.prompt_password().await;
            match password {
                Some(pw) => {
                    // retry with password ...
                    // if works: Ok(val)
                    // if wrong: Err(StateError::PasswordNeeded)
                }
                None => Err(StateError::Cancelled),
            }
        }

        Err(ArchiveError::PathTraversal { path }) =>
            Err(StateError::SecurityRisk {
                detail: format!("dangerous path: {path}")
            }),

        Err(ArchiveError::ReadFailed { detail, .. }) =>
            Err(StateError::ConnectionFailed { detail }),

        Err(ArchiveError::UnsupportedMethod { method }) =>
            Err(StateError::UnsupportedFormat { detail: method }),

        Err(ArchiveError::InvalidFormat { message }) =>
            Err(StateError::UnsupportedFormat { detail: message }),

        Err(ArchiveError::Cancelled) =>
            Err(StateError::Cancelled),

        Err(other) =>
            Err(StateError::Internal { detail: other.to_string() }),
    }
}
```

### Product layer: open the tunnel, handle what you know

The product is the one who assembled the decorator stack. It knows what error types are possible because it depends on those horizontal crates. It opens `StateError::Transport`, tries to downcast, handles or reports.

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
Decorator (Horiz.)  Layer 0 (I/O)    Layer 2 (Parser)   Layer 3.5 (State)      Layer 4 (Product)
──────────────────  ─────────────    ──────────────     ──────────────         ──────────────────
                    io::Error      → ReadFailed       → ConnectionFailed    → "Connection lost"
                    bad ZIP magic  → InvalidFormat    → UnsupportedFormat   → "Can't read file"
                    CRC wrong      → CrcMismatch      → FileCorrupted       → "File is damaged"
                    encrypted      → PasswordRequired → [handle: prompt]    → (disappears)
                                                      → PasswordNeeded     → password dialog
                    path escape    → PathTraversal    → SecurityRisk         → "Dangerous archive"
                    LZMA 1GB dict  → ResourceLimitExc → Internal             → "Can't process"

AuthReadAt sees 401 → Transport(AuthError::TokenExpired)
                                                      → Transport(box)     → [open: refresh]
                                                        (blind forward)      → (disappears)
                                                                              or "Sign in again"

MeteredReadAt       → Transport(QuotaError::Hit)
                                                      → Transport(box)     → [open: QuotaExceeded]
                                                        (blind forward)      → "Buy more credits"

KEY:
  "→"              = TRANSLATION (new type, old consumed)
  "[handle: ...]"  = error handled here, may never reach product
  "(disappears)"   = retry/prompt succeeded, product never sees it
  "(blind forward)" = state layer re-wraps without inspecting
  "[open: ...]"    = product downcasts the Transport box
```

---

## Decorator Construction (Product Assembles the Stack)

The **product** constructs the decorator stack using horizontal crates, then passes the composed `ReadAt` to the state layer. The state layer passes it to core. Neither the state layer nor core knows about decorators.

```rust
// Server product builds the stack:

fn open_cloud_archive(&self, url: &str) -> Result<Session, ServerError> {
    // Bottom: core's dumb HTTP reader (one request, one response)
    let http = HttpReadAt::new(url);

    // Decorate: product-level concerns from horizontal crates
    let authed  = AuthReadAt::new(http, self.token_provider.clone());   // archive-auth
    let retried = RetryReadAt::new(authed, self.retry_policy.clone());  // archive-retry
    let metered = MeteredReadAt::new(retried, self.quota_tracker.clone()); // server-internal

    // Pass composed ReadAt to state layer — state layer passes it to core
    let session = self.state.open(metered).await?;  // returns Result<Session, StateError>

    Ok(session)
}
```

```
What core sees:          impl ReadAt ──── "give me bytes"
What's actually there:   MeteredReadAt → RetryReadAt → AuthReadAt → HttpReadAt
                         (server)        (retry crate)  (auth crate)  (core)
```

A KMP app's stack might look completely different:

```rust
// KMP app — OAuth handled in Kotlin, passed as static header
fn open_cloud_archive(&self, url: &str, bearer_token: &str) -> Result<Session, AppError> {
    let http = HttpReadAt::new(url)
        .with_headers(vec![("Authorization", format!("Bearer {bearer_token}"))]);

    // No auth decorator, no metering — just hand to state layer
    let session = self.state.open(http).await?;
    Ok(session)
}
```

---

## Platform Notes

Error types are pure Rust enums — fully cross-platform. Platform-specific concerns:

- **Desktop (KMP):** `StateError` crosses FFI via UniFFI into Kotlin. Product-level errors (`AppError`) are defined in Kotlin. The UniFFI surface only exposes `StateError` — clean, no phantom variants for quota/auth that can't occur on local-only usage.
- **iOS/Android (KMP):** same as desktop — Compose Multiplatform UI, Rust core via UniFFI. OAuth may be handled entirely in Kotlin/Swift, bypassing Rust-side auth decorators.
- **Server:** pure Rust, no FFI. `ServerError` extends `StateError` with billing/auth variants.
- **CLI:** pure Rust. `CliError` is minimal — most `StateError` variants map directly.

The state layer's logic (translation, caching) is pure cross-platform Rust. Decorators and product-level error handling are composed at the product level — each product picks what it needs.
