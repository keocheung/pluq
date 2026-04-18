# ArchiveSniper — Error Architecture

> **Switchboard.** Canonical philosophy + 4-tier model + flow summary below.
> Variant definitions, decorator code, translation match arms live in
> `errors/` spokes — pull on demand.
> See also: `ARCHITECTURE.md` (system layers), `CONTEXT.md` (project state).

---

## Philosophy (non-negotiable)

1. **Each layer defines errors for its consumer.** No layer exposes a
   lower layer's error types upward.
2. **Handle or translate, never wrap.** An error either disappears
   (retry, prompt, recover) or becomes the consumer's vocabulary.
3. **The unhandleable falls to a catch-all.** Generic "something went
   wrong" with enough context for logging — never a stack trace shown to
   the user.
4. **Errors carry context for two audiences.** Decorators route
   programmatically (match on enum variants). Products build user
   messages (no string parsing).
5. **Decorator errors tunnel blindly.** `ReadError::Transport(Box<dyn>)`
   carries opaque decorator errors through core and the state layer
   untouched. Only the product opens the box and downcasts.

---

## The Four Error Tiers

```text
╔════════════════════════════════════════════════════════════════════╗
║ Tier 3 — Layer 4 (Products) sees: ArchiveError                      ║
║   • Handle universal errors directly                                ║
║   • Open ArchiveError::Transport, downcast to known decorator types ║
║   • Translate to product-level error (ServerError, AppError, etc.)  ║
╠════════════════════════════════════════════════════════════════════╣
║ Tier 2 — Layer 3 (Archive — Facade + State) produces: ArchiveError  ║
║   • Receives CoreError from format parsers                          ║
║   • Translates core errors it understands                           ║
║   • Builds user-friendly message from ReadFailure context           ║
║   • Promotes ReadError::Transport → ArchiveError::Transport (blind) ║
╠════════════════════════════════════════════════════════════════════╣
║ Tier 1 — Layer 2 (Format Parsers) produces: CoreError               ║
║   • Every variant is a format/archive concept                       ║
║   • Io(ReadError) wraps I/O at the parser boundary                  ║
║   • No Transport variant — tunnel lives inside ReadError            ║
╠════════════════════════════════════════════════════════════════════╣
║ Tier 0 — Layer 0 (I/O) produces: ReadError                          ║
║   • Standalone I/O language (local + remote)                        ║
║   • Failed(ReadFailure) — structured kind for routing + display     ║
║   • Transport(Box) — opaque decorator tunnel injected here          ║
║   • OutOfBounds — semantic upconversion to CorruptHeader at parser  ║
╚════════════════════════════════════════════════════════════════════╝
```

---

## Vocabulary Per Layer (one-liners)

- **`ReadError`** — `archive-core` Layer 0. Variants: `Failed(ReadFailure)`,
  `Transport(Box)`, `OutOfBounds`.
- **`ReadFailure`** — `archive-core` Layer 0. Fields: `source`, `operation`,
  `kind`, `diagnostic`.
- **`ReadErrorKind`** — `archive-core` Layer 0. Network / HTTP status /
  local FS variants. See `errors/read-error.md` for the full table.
- **Decorator errors** — each horizontal crate owns its own. `AuthError`
  (in `archive-auth`), `RetryError` (in `archive-retry`), `QuotaError`
  (server-internal). Each independent. Boxed into `ReadError::Transport`.
- **`CoreError`** — `archive-core` Layer 2. Variants: `Io`, `InvalidFormat`,
  `UnsupportedFormat`, `UnsupportedCompression`, `UnsupportedFeature`,
  `CorruptHeader`, `BitstreamError`, `CrcMismatch`, `PathTraversal`,
  `SymlinkTargetEscape`, `ResourceLimitExceeded`, `PasswordRequired`,
  `WrongPassword`, `Cancelled`. **No `Transport` variant** — tunnel rides
  inside `Io(ReadError::Transport)`.
- **`ArchiveError`** — `archive` Layer 3. Variants: `FileCorrupted`,
  `PasswordNeeded`, `UnsupportedFormat`, `SecurityRisk`, `ConnectionFailed`,
  `Cancelled`, `CacheInvalid`, `Internal`, `Transport`.
- **`ServerError` / `AppError` / `CliError`** — each product owns its own.
  Extends `ArchiveError` translation with product-specific variants (auth,
  quota, etc.). CLI never sees `Transport` — composes no decorators.

---

## Translation Flow Summary

```text
Layer 0 (I/O)            Layer 2 (Parser)       Layer 3 (Archive)        Layer 4 (Product)
ReadError                CoreError              ArchiveError             Product error / UI
-----------              ------------           --------------           ------------------

Failed{kind:NetTimeout}  → Io(ReadError::..)    → ConnectionFailed       → "Connection timed out"
Failed{kind:PermDenied}  → Io(ReadError::..)    → ConnectionFailed       → "Permission denied: '...'"
OutOfBounds              → CorruptHeader        → FileCorrupted          → "File is damaged"

                           InvalidFormat        → UnsupportedFormat      → "Not an archive"
                           UnsupportedFormat    → UnsupportedFormat      → "Can't read format"
                           UnsupportedCompr     → UnsupportedFormat      → "Can't decompress"
                           UnsupportedFeature   → UnsupportedFormat      → "Feature missing"

                           CorruptHeader        → FileCorrupted          → "File is damaged"
                           BitstreamError       → FileCorrupted          → "Extraction failed"
                           CrcMismatch          → FileCorrupted          → "Integrity check failed"

                           PasswordRequired     → [handle: prompt]       → (disappears)
                                                → PasswordNeeded         → password dialog
                           WrongPassword        → [handle: retry]        → (disappears)

                           PathTraversal        → SecurityRisk           → "Dangerous archive"
                           SymlinkTargetEscape  → SecurityRisk           → "Dangerous link"
                           ResourceLimitExceed  → Internal               → "Resource limit hit"

                           Cancelled            → Cancelled              → (operation stopped)

— Decorator tunnel (Transport box journey) ——————————————————————————————

AuthDecorator sees 401:
  ReadError::Transport(AuthError::TokenExpired)
   → CoreError::Io(ReadError::Transport(box))   [parser wraps]
   → ArchiveError::Transport(box)               [state layer blind-forwards]
   → Product downcasts → refresh token → (disappears)
                                       or "Sign in again"

KEY:
  →            = TRANSLATION (new type, old consumed)
  [handle: …]  = error handled here, may never reach product
  (disappears) = retry/prompt succeeded, product never sees it
  blind-forward = layer re-wraps without inspecting the box
```

---

## Spokes — Read on Demand

- **[ReadError (Tier 0)](errors/read-error.md)** —
  Full `ReadError`, `ReadFailure`, `SourceInfo`, `ReadOperation`,
  `ReadErrorKind` variant table with user-message hints, `NetworkPhase`.
  *Read when:* adding an I/O variant, building a new `ReadAt`, deciding
  retryability for a failure mode.

- **[CoreError (Tier 1)](errors/core-error.md)** —
  Full `CoreError` enum with `thiserror` annotations,
  `PasswordDetectionMethod`, parser conversion pattern
  (`map_err(CoreError::Io)`, `OutOfBounds → CorruptHeader`).
  *Read when:* adding a parser-level error, changing CRC/password flow.

- **[ArchiveError (Tier 2)](errors/archive-error.md)** —
  Full `ArchiveError` enum, state-layer translation match arms,
  `build_user_message()` reference impl.
  *Read when:* editing the state layer, changing user-facing wording,
  adding a new translatable core error.

- **[Decorator Errors & Transport Tunnel](errors/transport-tunnel.md)** —
  `AuthError`, `RetryError`, `QuotaError` examples; decorator `ReadAt`
  impl (401 intercept + token refresh, retry-after parsing); product
  downcast pattern; multi-decorator stack assembly.
  *Read when:* writing a horizontal crate, adding a product-level error
  type, debugging where a decorator error went.

---

## Mid-Stream Error Recovery (one-liner — full detail in spoke)

`read_stream` impls embed `ReadError` inside `io::Error` before returning
the `AsyncRead`. Decompression code recovers it via `into_inner()` +
`downcast::<ReadError>()`. Successful downcast → `CoreError::Io`; failure
→ `CoreError::BitstreamError`. Preserves structured context across the
async→sync boundary. See `architecture/layer1-codec.md` for the full
pattern.

---

## Platform Notes (one-liner)

Pure Rust enums, fully cross-platform. KMP apps cross FFI via UniFFI
exposing only `ArchiveError` (no phantom server-only variants). CLI
never receives `ArchiveError::Transport` — it composes no decorators —
so it can map that variant straight to `Internal`.
