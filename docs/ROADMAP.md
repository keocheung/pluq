# Archive Sniper — Active Roadmap

## Phase 4: Cloud Provider API Readers
- [ ] **OneDrive** — `archive-onedrive` crate. Resolve share link → driveItem via Microsoft Graph API, download with Range headers. *(v1.x — deferred)*

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
