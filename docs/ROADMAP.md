# Archive Sniper — Product Roadmap & Scope

## The Vision

Create the fastest, most surgical way to browse and extract files from cloud-hosted archives (ZIP/RAR5) without downloading the entire file.

### Target v1 Interface (Product Spec)

These commands define the CLI's public API. The TUI is the default entry point.

*   `snipe <url-or-path>` → **Interactive TUI browser** (The primary experience)
*   `snipe list <url-or-path>` → Non-interactive listing (for scripting)
*   `snipe get <url-or-path> <path/glob>` → Non-interactive extraction
*   `snipe test <url-or-path>` → Verify archive integrity (extract nothing)
*   `snipe auth add/remove/list` → Manage OAuth tokens for Big 3 providers
*   `snipe feedback` → Open GitHub issues

---

## Development Phases

### Phase 1: ZIP Engine (Completed)
- [x] `ReadAt` I/O abstraction (Local + HTTP).
- [x] ZIP central directory parser.
- [x] Deflate/Store extraction with CRC32 verification.

### Phase 2: RAR Engine (Completed)
- [x] RAR5 block and vint parser.
- [x] Sequential scan with 10MB greedy lookahead.
- [x] Quick Open (QO) optimization support.
- [x] Extraction via `unrar` crate for compressed methods.
- [x] Format detection facade (`ArchiveCore::open`).

### Phase 3: Interactive TUI (Up Next)
- [ ] `ratatui` integration in `archive-cli`.
- [ ] Tree-based navigation (using `archive` state layer).
- [ ] Multi-select, fuzzy search, and extraction progress UI.

### Phase 4: Cloud Auth & The Big 3
- [ ] OAuth2 flow for **Google Drive**, **Dropbox**, and **OneDrive**.
- [ ] Token storage and automatic `Authorization` header injection.
- [ ] Handling of expired tokens/auth challenges in the TUI.

### Phase 5: Polishing & Performance
- [ ] Parallel extraction for ZIP/non-solid RAR.
- [ ] Predictive micro-jumping for RAR sequential scan (on-demand).
- [ ] Cross-platform release pipeline (GitHub Actions).

---

## Future Ideas (Post-v1)

*   **Write Support:** Archive creation (`snipe pack`). ZIP first, then 7z. TUI file browser for selecting files.
*   **Format Expansion:** 7z, TAR.GZ, ISO read support.
*   **SaaS Server:** HTTP API for web-based sniping. Metered/freemium. See `ARCHITECTURE.md` § Products.
*   **KMP Apps:** Android/iOS/Desktop via Compose Multiplatform + UniFFI. See `ARCHITECTURE.md` § Products.
*   **Cloud Archiving (Vault):** Stream files from cloud → compress → stream archive back.
*   **Plugin Hooks:** External tool integration (fzf, yazi) at search/select/browse decision points.
