# Archive Sniper — Active Roadmap

## Phase 4: Cloud Provider API Readers
- [ ] **OneDrive** — `archive-onedrive` crate. Resolve share link → driveItem via Microsoft Graph API, download with Range headers. *(v1.x — deferred)*

## Phase 5: Security Hardening & Release
- [ ] **`snipe feedback`** — open GitHub issues URL in default browser. Trivial.
- [ ] Path traversal guard — currently only in `archive-cli/src/main.rs` (Get handler). Move into `archive-core` so all extraction paths are protected, not just the CLI.
- [ ] Symlink target verification to ensure links stay within the output directory.
- [ ] Cross-platform release pipeline (GitHub Actions).
- [ ] HTTP fingerprint enrichment (ETag + Last-Modified from HEAD response) — *optional stretch goal*.
- [ ] Glob extraction in TUI — type `*.jpg` in the search box and extract all matching entries; bridges TUI and `snipe get` use cases — *optional stretch goal*.

---

> View completed phases and tasks in [COMPLETED.md](./roadmap/COMPLETED.md)
