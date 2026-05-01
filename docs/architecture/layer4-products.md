# Layer 4 — Products & Horizontal Crates

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

The product layer assembles its `ReadAt` decorator stack from horizontal
crates, hands the composed reader to the state layer, and defines its own
product-level error type. The decorator pattern keeps auth, retry,
metering OUT of core.

---

## Horizontal Capabilities (Optional Crates)

Optional crates that products pick from to compose their `ReadAt`
decorator stack. Each crate depends on `archive-core` (for the `ReadAt`
trait and `CoreError`), but NOT on `archive`. Products assemble
decorators, compose them into a `ReadAt` stack, and pass the result down
to the state layer.

### `archive-auth` — OAuth / Token Lifecycle

- `AuthReadAt<R: ReadAt>` — intercepts HTTP 401 responses, boxes
  `AuthError::TokenExpired` into `ReadError::Transport`, lets the product
  handle refresh.
- `AuthError` — `TokenExpired`, `RefreshFailed`, etc.
- Products that handle OAuth in the host language (e.g., Kotlin/Swift on
  mobile) can skip this crate entirely and pass pre-authenticated headers
  to `HttpReadAt::with_headers()`.

### `archive-retry` — Retry / Backoff

- `RetryReadAt<R: ReadAt>` — retries transient failures with configurable
  backoff.
- Products may want different retry aggressiveness (desktop vs mobile vs
  server).

### Metering / Billing — server-internal, not a shared crate

```rust
// Example: MeteredReadAt (server-only)
pub struct MeteredReadAt<R: ReadAt> {
    inner: R,
    bytes_read: Arc<AtomicU64>,
}
```

- Wraps `HttpReadAt` (or any cloud source) — counts bytes in
  `read_range()` and `read_stream()`.
- `bytes_read.load()` at end of operation = billable amount.
- Per-operation accounting (each extract/list call gets a fresh meter or
  an operation ID).
- Only the server product uses this. Client apps don't meter.

---

## How Decorators Create Tunneled Errors

Each decorator intercepts errors from the `ReadAt` below it. If the error
is something the decorator recognizes (e.g., HTTP 401), it boxes its own
error type into `ReadError::Transport`. Core carries the box blindly. The
state layer carries it blindly. The product at the top opens the box,
downcasts to the types it knows about (from the horizontal crates it
depends on), and handles or reports them.

See `docs/errors/transport-tunnel.md` for the full multi-hop flow and
decorator construction example.

---

## Products

Each product is an actual binary (or library consumed via FFI). It
assembles the full stack: horizontal crates (if needed) → state layer →
core.

### Server / SaaS

Serves the web version. The website can't do direct HTTP range requests
(CORS, no Range header control), so it goes through the server API. This
is the only metered/freemium path.

- Assembles: `archive-core` + `archive` + `archive-auth` + `archive-retry`
  + metering (internal).
- Defines `ServerError` — extends `ArchiveError` with `QuotaExceeded`,
  `AuthenticationFailed`, etc.
- Billing, quota accounting, OAuth for cloud providers (Dropbox, Drive)
  all live here.

### KMP Apps (Android / iOS / Desktop)

Standalone products. They are cloud extractors — connect
directly to Dropbox/Drive, fetch ranges, extract. Buy once, no metering.
Built with Compose Multiplatform, consuming the Rust core via UniFFI.

- Assembles: `archive-core` + `archive` + optionally `archive-auth` (or
  handle OAuth in Kotlin/Swift).
- Defines `AppError`.
- Full local archive management (read + write) plus direct cloud sniping.
- No dependency on the server for core functionality.

### `archive-source`

Source resolution layer between products (Layer 4) and `archive` (Layer
3). Centralizes URL detection, credential discovery, and reader
construction.

- Detects source type: local file path, Google Drive URL, generic HTTP
  URL.
- Looks up credentials (e.g. `PLUQ_GDRIVE_KEY`).
- Returns `ResolvedSource` enum (`Local(LocalReadAt)` |
  `Http(CachingReadAt<HttpReadAt>)` |
  `GDrive(CachingReadAt<GoogleDriveReadAt>)`).
- Products call `resolve(input, &config)` then pass the reader to
  `Archive::open_reader()`.

See `docs/SOURCE.md` for the full design.

### CLI (`pluq`) — the v1 product

Interactive TUI archive browser + cloud sniping from the terminal.
Handles OAuth externally (browser flow → static bearer token via
`HttpReadAt::with_headers()`), no `ReadAt` decorators needed.

- Assembles: `archive-core` + `archive` + `archive-source`, no decorators.
- Defines `CliError`.
- Owns: TUI (`ratatui`), CLI parsing (`clap`), OAuth flows.
- Delegates cloud provider URL routing to `archive-source::resolve()`.

### Web Site

Not a Rust product. Consumes the server's HTTP API. Metered/freemium.
