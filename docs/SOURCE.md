# Source Resolution — Design Spec

> How ArchiveSniper resolves user input (URL or path) into the right `ReadAt`
> reader for the job. Centralizes detection, credential discovery, and reader
> construction so product layers don't duplicate this logic.
>
> See also: `ARCHITECTURE.md` (system layers), `ERRORS.md` (error flow),
> `ROADMAP.md` (Phase 4 / 4.5).

---

## The Problem

**Raw cloud share links are hostile to surgical extraction:**

- Multi-hop redirect chains (share page -> confirmation -> CDN)
- Abuse detection / CAPTCHA gates (Google's "virus scan" interstitial)
- Missing or unreliable `Content-Length` / `Accept-Ranges` headers
- Undocumented rate limits with HTML error pages instead of proper status codes
- No way to distinguish "file not found" from "temporarily throttled"

**Plain HTTP servers are fine.** A direct link to `https://university.edu/data.zip`
that responds correctly to `Range` requests and gives proper headers works out of
the box with `HttpReadAt`. No special handling needed.

**The cloud providers (Google, Dropbox, OneDrive) are the problem** precisely because
they're "smart": redirect chains, virus scan interstitials, HTML error pages. They
need dedicated provider crates with proper API access.

**The construction logic must be shared.** Every product layer (CLI, TUI, future
server, future KMP apps) needs the same "detect URL -> build the right reader" logic.
Without centralizing it, each product duplicates detection, credential lookup, and
reader construction.

---

## The Solution: `archive-source` Crate

A standalone crate that takes user input and returns a fully-constructed `ReadAt`
reader. Product layers call `resolve()` and pass the result to `Archive`.

### Resolution Flow

```
User input (URL or path)
        |
        v
  source::resolve(input, &config)        <-- archive-source crate
        |
        +-- local path      -> LocalReadAt
        |
        +-- GDrive URL      -> CachingReadAt<GoogleDriveReadAt>
        |
        +-- Dropbox URL     -> CachingReadAt<DropboxReadAt>
        |
        +-- any other HTTP  -> CachingReadAt<HttpReadAt>
        |
        v
  ResolvedSource (enum implementing ReadAt)
        |
        v
  Archive::open(reader)                  <-- facade, generic over R: ReadAt
```

### Key Principles

- **Every remote reader gets `CachingReadAt`.** It optimizes repeated byte-range
  fetches — beneficial for GDrive, Dropbox, and plain HTTP alike.

- **`resolve()` does detection + credential discovery + construction.**
  It does NOT do credential acquisition (OAuth flows, user prompts). That's a
  product-layer concern.

- **Generic HTTP is not a fallback.** It's the correct path for well-behaved
  servers. Provider crates exist to work *around* misbehaving cloud platforms.

- **The Archive facade becomes generic** — `Archive<R: ReadAt>`. It takes
  whatever reader it's given. No `Source` enum needed in `archive-core`.

---

## Dependency Graph

```
archive-core                <-- ReadAt trait, HttpReadAt, LocalReadAt,
    ^                           CachingReadAt, format parsers (generic over R)
    |
archive-gdrive              <-- GoogleDriveReadAt (implements ReadAt)
archive-dropbox             <-- DropboxReadAt (implements ReadAt)
    ^
    |
archive-source              <-- resolve(): detect URL -> construct reader
    ^                           SourceConfig, SourceError, classify_transport()
    |
archive (facade)            <-- Archive<R: ReadAt>, format detection, cache
    ^
    |
archive-cli                 <-- calls resolve() + Archive::open(), no provider logic
```

### What Moves Out of `archive-core`

- **`Source` enum** — the `Provider(Box<dyn DynReadAt>)` variant and all `DynReadAt`
  type-erasure machinery are deleted. The enum existed to give `Archive` a single
  concrete type to monomorphize on. With a generic `Archive<R>`, it's unnecessary.
- **`Source::detect()`** — the local-vs-HTTP detection moves to `archive-source`
  as part of `resolve()`.
- Core retains: `ReadAt` trait, `HttpReadAt`, `LocalReadAt`, `CachingReadAt`,
  format parsers generic over `R: ReadAt`.

### What Stays the Same

- `ReadAt` trait — unchanged.
- `HttpReadAt` — unchanged.
- `CachingReadAt` — unchanged (wrapping moves to `archive-source`).
- Format parsers — already generic over `R: ReadAt`, no changes needed.
- Error types — `ReadError`, `CoreError` unchanged. See Error Handling below.

---

## The `resolve()` API

```rust
/// Resolve user input into a fully-constructed reader.
///
/// Detects the source type (local, cloud provider, generic HTTP), looks up
/// credentials, and constructs the appropriate ReadAt implementation.
///
/// Returns `Err(SourceError)` if a provider was detected but setup failed
/// (e.g., missing API key, unparseable URL).
pub fn resolve(
    input: &str,
    config: &SourceConfig,
) -> Result<ResolvedSource, SourceError>
```

### `SourceConfig`

Holds credential discovery state. Product layers construct this at startup.

```rust
pub struct SourceConfig {
    /// OAuth token store (~/.config/archive-sniper/tokens.json).
    /// None in Phase 4 (app credentials only). Added in Phase 4.5.
    pub token_store: Option<TokenStore>,

    // App credentials come from env vars / embedded defaults,
    // looked up by each provider's auth module. No explicit fields
    // needed here for Phase 4 — provider crates handle their own
    // credential chain (env var -> embedded default).
}
```

Phase 4: `SourceConfig` is minimal (possibly just a unit struct with a constructor).
Phase 4.5: adds `token_store` for OAuth tokens. The `resolve()` signature doesn't
change — just richer config.

### `ResolvedSource`

An enum implementing `ReadAt` via delegation. Each variant is a fully-constructed,
ready-to-use reader.

```rust
pub enum ResolvedSource {
    Local(LocalReadAt),
    Http(CachingReadAt<HttpReadAt>),
    GDrive(CachingReadAt<GoogleDriveReadAt>),
    Dropbox(CachingReadAt<DropboxReadAt>),
    // future providers add variants here
}

impl ReadAt for ResolvedSource {
    // delegates to inner reader per variant
}
```

No type erasure (`Box<dyn>`), no `DynReadAt` bridge. Static dispatch, zero overhead.

---

## Cloud Provider Crates

Each provider is a standalone crate implementing `ReadAt`. They own their auth
mechanics, URL detection, and API-specific error types. `archive-source` depends
on them and calls their constructors.

### Google Drive (`archive-gdrive`) — Implemented

- `GoogleDriveReadAt` wraps `HttpReadAt` pointed at `googleapis.com/drive/v3/files/{id}?alt=media&key=...`
- `size()` calls metadata API (`GET /drive/v3/files/{id}?fields=name,size`)
- URL detection: `is_gdrive_url()`, `extract_file_id()`
- Auth: API key from `ARCHIVE_SNIPER_GDRIVE_KEY` env var or embedded default
- Errors: `GDriveError` enum (`FileNotFound`, `NotPublic`, `InvalidApiKey`,
  `RateLimited`, `QuotaExceeded`) tunneled via `ReadError::Transport`

### Dropbox (`archive-dropbox`) — Phase 4

- `DropboxReadAt` uses `POST /2/sharing/get_shared_link_file` with
  `Dropbox-API-Arg` header for range reads (confirmed: returns 206 + correct bytes)
- `size()` via `POST /2/sharing/get_shared_link_metadata`
- URL detection: check host == `dropbox.com`
- Auth: Basic auth (`Authorization: Basic base64(app_key:app_secret)`), app
  credentials from `ARCHIVE_SNIPER_DROPBOX_APP_KEY` / `ARCHIVE_SNIPER_DROPBOX_APP_SECRET`.
  Requires `sharing.read` scope on the Dropbox app.
- No OAuth required for public shared links

### OneDrive (`archive-onedrive`) — Post-v1

- Resolve share link -> driveItem via Microsoft Graph API
- Range reads via Graph API download endpoint
- Deferred to post-v1

---

## Error Handling

Two separate error paths. Both integrate cleanly with `ERRORS.md`.

### Path 1: Construction Errors (`SourceError`)

Happen at `resolve()` time, before `Archive` is opened. Product catches directly.

```rust
pub enum SourceError {
    /// Provider detected but credentials missing.
    /// e.g., GDrive URL but no API key configured.
    NoCredentials { provider: &'static str, guidance: String },

    /// URL matched a provider pattern but couldn't be parsed.
    /// e.g., GDrive URL with no extractable file ID.
    InvalidUrl { url: String, reason: String },
}
```

### Path 2: Runtime I/O Errors (Transport Tunnel)

Provider-specific errors during reads flow through the existing `ReadError::Transport`
tunnel exactly as described in `ERRORS.md`:

```
GoogleDriveReadAt hits 403
  -> ReadError::Transport(Box::new(GDriveError::NotPublic))
  -> CoreError::Io(ReadError::Transport(box))          [core carries blindly]
  -> ArchiveError::Transport(box)                      [facade carries blindly]
  -> Product calls classify_transport() to interpret    [archive-source classifies]
```

### Transport Error Classification

`archive-source` provides a classification helper so products don't need to depend
on individual provider crates for error types:

```rust
/// Provider-agnostic error classification for transport tunnel errors.
pub enum ProviderError {
    /// File exists but is not publicly shared. OAuth may help.
    NotPublic { provider: &'static str },
    /// File does not exist or was deleted.
    FileNotFound { provider: &'static str },
    /// Rate limited by provider. Retry after delay.
    RateLimited { provider: &'static str, retry_after: Option<Duration> },
    /// API key or credentials are invalid.
    InvalidCredentials { provider: &'static str },
    /// Provider-specific quota exceeded.
    QuotaExceeded { provider: &'static str },
}

/// Attempt to classify a transport tunnel error into a provider-agnostic category.
///
/// Called by products on `ArchiveError::Transport` contents. Returns `None`
/// if the error is not from a known provider (e.g., from a decorator like
/// AuthReadAt or MeteredReadAt — those are product-level concerns).
pub fn classify_transport(err: &dyn std::error::Error) -> Option<ProviderError> {
    // downcasts to GDriveError, DropboxError, etc.
}
```

This replaces the ad-hoc `format_transport_error()` currently in `archive-cli/src/provider.rs`.

---

## OAuth Fit (Phase 4.5)

OAuth adds user-token access for private files. The `archive-source` design
accommodates this without architectural changes.

### Layer Responsibilities

| Layer | Auth responsibility |
|---|---|
| Provider crate (`archive-gdrive`) | **Uses** credentials (headers, token refresh on 401) |
| Source crate (`archive-source`) | **Discovers** credentials (token store, env vars, embedded) |
| Product layer (`archive-cli`) | **Acquires** credentials (OAuth browser flow, user prompts) |

### Provider Auth Enums

Each provider crate defines its own auth enum (Phase 4 starts with the first
variant; Phase 4.5 adds the second):

```rust
// archive-gdrive
pub enum GDriveAuth {
    ApiKey(String),                                         // Phase 4
    OAuth { access_token: String, refresh_token: String },  // Phase 4.5
}

// archive-dropbox
pub enum DropboxAuth {
    AppCredentials { key: String, secret: String },         // Phase 4
    OAuth { access_token: String, refresh_token: String },  // Phase 4.5
}
```

Provider crates handle refresh internally — they know their refresh endpoints
and token exchange protocols. `archive-source` doesn't need to know how refresh
works, only which credentials are available.

### Credential Discovery in `resolve()`

```rust
// Phase 4.5 — resolve() credential selection (pseudocode)
if is_gdrive_url(input) {
    let file_id = extract_file_id(input)?;
    let auth = match config.token_store.and_then(|ts| ts.gdrive_token()) {
        Some(token) => GDriveAuth::OAuth { ... },     // prefer OAuth if available
        None => GDriveAuth::ApiKey(api_key()?),        // fall back to app credential
    };
    let reader = GoogleDriveReadAt::new(file_id, auth);
    return Ok(ResolvedSource::GDrive(CachingReadAt::new(reader)));
}
```

### The Private-File Error Flow

```
1. resolve() -> builds reader with ApiKey (no OAuth tokens yet)
2. Archive::open(reader) -> reader calls API -> 403 NotPublic
3. Tunnels up as ArchiveError::Transport(GDriveError::NotPublic)
4. Product calls classify_transport() -> ProviderError::NotPublic
5. TUI: "This file is private. Authenticate with Google?" -> OAuth flow
6. Tokens saved to TokenStore
7. Product retries resolve() -> now finds OAuth tokens -> works
```

The `resolve()` signature doesn't change. `SourceConfig` gains a `token_store`
field. Provider crates gain an OAuth variant in their auth enum. Product layers
gain OAuth flow code. No architectural surgery.

---

## Migration: What Changes From Current Code

### Deleted from `archive-core`

- `source.rs` — the `Source` enum, `DynReadAt` trait, `Source::detect()`,
  `Source::provider()`, `#[allow(private_interfaces)]`
- `LocalPath` impl for `Source` in `io/mod.rs`

### Moved to `archive-source`

- URL detection logic (currently `Source::detect()` in core + `provider::build_reader()`
  in CLI) -> `resolve()`
- Transport error formatting (currently `provider::format_transport_error()` in CLI)
  -> `classify_transport()`

### Changed in `archive` (facade)

- `ArchiveInner` becomes generic: `ArchiveInner<R: ReadAt>` instead of
  `ArchiveInner { Zip(ZipArchive<Source>), ... }`
- `Archive::open(source: &str)` removed (was doing detection internally)
- `Archive::open_reader()` becomes the primary entry point, taking any `R: ReadAt`
- `local_path()` detection for RAR extraction shortcut needs a new approach
  (see below)

### `local_path()` for RAR Extraction

Currently `Source::local_path()` tells the RAR pipeline whether to use the
local-file shortcut (pass path to `unrar`) vs the cloud synthetic-archive approach.

With `ResolvedSource`, this becomes:

```rust
impl ResolvedSource {
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            ResolvedSource::Local(r) => Some(&r.path),
            _ => None,
        }
    }
}
```

Same logic, moved from core's `Source` to `archive-source`'s `ResolvedSource`.

### Deleted from `archive-cli`

- `src/provider.rs` — detection + construction + error formatting all move to
  `archive-source`. CLI just calls `resolve()` and `classify_transport()`.
