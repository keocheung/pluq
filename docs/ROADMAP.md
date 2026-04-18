# Archive Sniper — Active Roadmap

## Phase 3c: Style Guide Compliance & Home Screen
- [ ] **Fix 'q' key inconsistency** — 'q' quits even when SnipeInput is focused (URLs with 'q' untypable). Need to balance global 'q' mandate with input requirements.

## Phase 4: Cloud Provider API Readers
- [ ] **OneDrive** — `archive-onedrive` crate. Resolve share link → driveItem via Microsoft Graph API, download with Range headers. *(v1.x — deferred)*

## Phase 4.5: OAuth2 — Private File Access
- [ ] OAuth2 flow for **Google Drive** (browser redirect, token exchange).
- [ ] OAuth2 flow for **Dropbox**.
- [ ] Token storage (`~/.config/archive-sniper/tokens.json`) and refresh.
- [ ] `archive-auth` ReadAt decorator — injects `Authorization: Bearer` header, handles token refresh transparently.
- [ ] TUI auth prompts — detect `NotPublic`/`Unauthenticated` errors, guide user through OAuth flow in-app.
- [ ] **`snipe auth add/remove/list`** — CLI surface for managing stored OAuth tokens.

## Phase 5: Security Hardening & Release
- [ ] **CLI rename to match product spec** — `browse` → bare default, `extract`/`extract-all` → `get [glob] [-o dir]`. Logic unchanged; clap surface only. Do before shell completions.
- [ ] **`snipe feedback`** — open GitHub issues URL in default browser. Trivial.
- [ ] Anti-bomb safeguards: enforce entry count, compression ratio, and header size limits.
- [ ] Memory-safe decoding: cap dictionary sizes for LZMA/LZMA2 to prevent OOM attacks.
- [ ] Path traversal guard — currently only in `archive-cli/src/main.rs` (ExtractAll handler). Move into `archive-core` so all extraction paths are protected, not just CLI extract-all.
- [ ] Symlink target verification to ensure links stay within the output directory.
- [ ] Cross-platform release pipeline (GitHub Actions).
- [ ] Shell completions — `snipe completions bash/zsh/fish` via clap (near zero effort). Generate after CLI rename.
- [ ] HTTP fingerprint enrichment (ETag + Last-Modified from HEAD response) — *optional stretch goal*.
- [ ] Glob extraction in TUI — type `*.jpg` in the search box and extract all matching entries; bridges TUI and `snipe get` use cases — *optional stretch goal*.

---

> View completed phases and tasks in [COMPLETED.md](./roadmap/COMPLETED.md)
