# Cloud Providers — Design Spec

> How ArchiveSniper talks to cloud storage platforms using their proper APIs
> instead of chasing redirects on raw share links.
>
> See also: `ARCHITECTURE.md` (system layers), `ERRORS.md` (error flow).

---

## The Problem

Raw cloud share links (Google Drive, Dropbox, OneDrive) are hostile to surgical extraction:

- Multi-hop redirect chains (share page → confirmation → CDN)
- Abuse detection / CAPTCHA gates (Google's "virus scan" interstitial)
- Missing or unreliable `Content-Length` / `Accept-Ranges` headers
- Undocumented rate limits with HTML error pages instead of proper status codes
- No way to distinguish "file not found" from "temporarily throttled"

## The Solution

Register ArchiveSniper as an app with each cloud platform. Use their official APIs
with an **API key** (app-level credential, no user OAuth) for public/shared files.

This gives us:
- Guaranteed Range request support
- Accurate Content-Length and ETag headers
- Well-documented rate limits with `Retry-After` headers
- Structured JSON error responses
- No redirect chains, no interstitials

---

## Architecture — Provider Resolution

Provider resolution is a **pre-construction** step. It transforms a user-facing
share URL into an API URL before `HttpReadAt` is created. Nothing downstream changes.

```
User input (share URL or local path)
        │
        ▼
  Source::detect(source)                          ← entry point (source.rs)
        │
        ├─ local path? → Source::Local(LocalReadAt::new(path))
        │
        └─ HTTP URL? ──→ provider::resolve(url)   ← NEW (pure function)
                │
                ├─ Google Drive URL → ResolvedUrl { api_url, headers, provider }
                ├─ Dropbox URL      → ResolvedUrl { api_url, headers, provider }
                └─ Unknown          → ResolvedUrl { original_url, no extras }
                │
                ▼
           HttpReadAt::new(api_url, client).with_headers(headers)
                │
                ▼
           Source::Http(reader)
```

### What stays the same

- `ReadAt` trait — unchanged
- `Source` enum — still `Local | Http`, no new variants
- `HttpReadAt` — unchanged (already supports `with_headers()`)
- Format parsers, extraction, caching — completely untouched
- Error types — no changes needed for API-key access

### What's new

One module: `archive-core/src/io/provider.rs` (or `provider/` if it grows).

```rust
/// Result of resolving a user-facing URL through provider detection.
pub struct ResolvedUrl {
    /// The URL to actually fetch (API endpoint, not the share link).
    pub url: String,
    /// Extra headers to attach to every request (e.g. auth tokens).
    pub headers: HeaderMap,
    /// Which provider was detected (for logging/diagnostics).
    pub provider: Provider,
}

pub enum Provider {
    GoogleDrive,
    Dropbox,
    // OneDrive,
    Generic,  // unknown URL, pass through as-is
}

/// Pure function: detect provider from URL, return resolved API URL.
pub fn resolve(url: &str) -> ResolvedUrl { ... }
```

`Source::detect()` calls `provider::resolve()` for HTTP URLs instead of
constructing `HttpReadAt` directly. That's the only integration point.

---

## Google Drive — First Provider

### API Access (No OAuth Required)

For **publicly shared** files, Google Drive API v3 supports access with just an
API key. No OAuth consent screen, no user tokens, no refresh flow.

- Register app in [Google Cloud Console](https://console.cloud.google.com/)
- Enable the Google Drive API
- Create an API key (restrict to Drive API only)
- Use `?key=API_KEY` query parameter on all requests

### URL Detection

Google Drive share links come in several forms. All contain a file ID:

| URL Pattern | File ID Location |
|---|---|
| `https://drive.google.com/file/d/{FILE_ID}/view?usp=sharing` | path segment |
| `https://drive.google.com/uc?id={FILE_ID}&export=download` | query param `id` |
| `https://drive.google.com/open?id={FILE_ID}` | query param `id` |

File IDs are alphanumeric + hyphens + underscores, typically 28–44 characters.

### URL Translation

User URL → API URL:

```
Input:  https://drive.google.com/file/d/1A2B3C4D/view?usp=sharing
Output: https://www.googleapis.com/drive/v3/files/1A2B3C4D?alt=media&key=API_KEY
```

The `?alt=media` parameter tells the API to return file content (not JSON metadata).
This endpoint supports standard HTTP Range headers.

### Metadata Prefetch (Optional, Recommended)

Before the first range request, call the metadata endpoint:

```
GET https://www.googleapis.com/drive/v3/files/{FILE_ID}?key=API_KEY&fields=size,name,mimeType
```

Returns JSON:
```json
{
  "name": "photos.rar",
  "size": "1048576",
  "mimeType": "application/x-rar-compressed"
}
```

Benefits:
- Validates file exists and is accessible before any range requests
- Accurate file size without HEAD request
- Real filename for display in TUI
- Early, clear error if file is private / deleted / quota-exceeded

This can serve as the `size()` implementation for Google Drive sources,
or we can let `HttpReadAt.size()` do its normal HEAD request (both work).

### Error Responses

The API returns structured JSON errors:

```json
{
  "error": {
    "code": 404,
    "message": "File not found: 1A2B3C4D",
    "errors": [{ "reason": "notFound", "domain": "global" }]
  }
}
```

Common codes:
- `404` — file not found or not shared publicly
- `403` — API key invalid, quota exceeded, or Drive API not enabled
- `429` — rate limited (includes `Retry-After` header)

These map cleanly to our existing `ReadErrorKind::HttpStatus`. The structured
body is already captured by `send_range_request`'s unexpected-status handler
(added in the tracing work). No error plumbing changes needed.

---

## API Key Management

Priority chain (first wins):

1. **Environment variable:** `ARCHIVE_SNIPER_GDRIVE_KEY`
2. **Config file:** `~/.config/archive-sniper/config.toml` → `[google_drive] api_key = "..."`
3. **Embedded default:** compiled into the binary

For v1, we implement #1 (env var) and #3 (embedded default). Config file comes
later when we have more settings to manage.

The embedded key is restricted in Google Cloud Console to the Drive API only.
Users who want their own quota can override via env var.

If no API key is available (env var empty, no embedded key), fall back to
`Generic` provider (raw HTTP, current behavior). Log a warning that API access
is recommended.

---

## Future: Dropbox, OneDrive

Each new provider adds:
1. A URL detection pattern in `provider::resolve()`
2. A URL translation function
3. Provider-specific API key / credential handling

The architecture doesn't change. `resolve()` gets a new match arm, returns
a different `ResolvedUrl`. Everything else is the same.

### Dropbox (sketch)

- App key from [Dropbox App Console](https://www.dropbox.com/developers/apps)
- Shared link metadata: `POST /2/sharing/get_shared_link_metadata`
- Shared link download: `POST /2/sharing/get_shared_link_file` (supports Range)
- Auth: app access token in `Authorization: Bearer` header

### OneDrive (sketch)

- App registration in Azure AD, client credentials flow
- Resolve share link → driveItem via Microsoft Graph API
- Download via Graph API with Range headers

---

## Future: OAuth (User's Private Files)

API-key access only works for publicly shared files. For private files, we need
OAuth — the user grants ArchiveSniper permission to access their files.

This is a separate, larger effort that builds on top of the provider infrastructure:
- OAuth flow (browser redirect, token exchange)
- Token storage and refresh
- The `ReadError::Transport` tunnel (from ERRORS.md) carries auth errors
- ReadAt decorators (from ARCHITECTURE.md Layer 4) handle token injection + refresh

The provider resolution layer we're building now is the foundation. OAuth adds
a credential layer on top of it, but the URL detection and API URL construction
remain the same.
