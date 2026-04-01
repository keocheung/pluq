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

### Phase 2.5: Engine Expansion (Completed)
- [x] Flatten double facade (`ArchiveCore` removed, `Archive` holds format enum directly).
- [x] Type-safe scan API (`Opened`/`PendingArchive`) for expensive metadata operations.
- [x] Add missing ZIP codecs: **BZip2**, **LZMA**, and **Zstandard**.
- [x] RAR4 format support (header parser + extraction via `unrar`).


### Phase 2.75: TUI Foundations — Elastic Scan, Cache, CLI Prep
- [x] Serialize location types (`RarEntryLocation`, `Rar4EntryLocation`, `ZipEntryLocation`) + `from_cached` constructors.
- [x] Elastic scan: replace 64 KiB `read_range` hops with `read_stream` + 10 MB surgical/streaming threshold.
- [x] Progress reporting: wire `on_scan_progress` (absolute byte offset + entry count) into scan loop.
- [x] Defer RAR4 scan for HTTP sources (same `NeedsScan` pattern as RAR5).
- [x] Range support early detection: check `Accept-Ranges` header during `HttpReadAt::size()`, fail fast.
- [x] Implicit persistent cache: `Archive::open` checks `~/.cache/archive-sniper/[hash].json`; `PendingArchive::scan` saves after completion. Stores entries, locations, preamble, end_block.
- [x] Same-name output folder helper in `archive-cli` (`photos.rar` → extract to `photos/`).

### Phase 3: Interactive TUI (The Cockpit)
- [ ] `ratatui` integration in `archive-cli`.
- [ ] Tree-based navigation (using `archive` state layer).
- [ ] Multi-select, fuzzy search, and extraction progress UI.

### Phase 4: Cloud Auth & The Big 3
- [ ] OAuth2 flow for **Google Drive**, **Dropbox**, and **OneDrive**.
- [ ] Token storage and automatic `Authorization` header injection.
- [ ] Handling of expired tokens/auth challenges in the TUI.

### Phase 5: Polishing, Performance & Security Hardening
- [ ] Source-aware default folder naming (use real GDrive/Cloud filename if available instead of URL-guessing).
- [ ] Anti-bomb safeguards: enforce entry count, compression ratio, and header size limits.
- [ ] Memory-safe decoding: cap dictionary sizes for LZMA/LZMA2 to prevent OOM attacks.
- [ ] Symlink target verification to ensure links stay within the output directory.
- [ ] Parallel extraction for ZIP/non-solid RAR.
- [ ] Predictive micro-jumping for RAR sequential scan (on-demand).
- [ ] Cross-platform release pipeline (GitHub Actions).
- [ ] Advanced cache invalidation (content hashing, detect modifications between opens).
- [ ] Configurable cache location (CLI flag or config file).
- [ ] Cache size limits / LRU eviction.
- [ ] HTTP fingerprint enrichment (ETag + Last-Modified from HEAD response).
- [ ] Full range support detection with retry/fallback and user-facing guidance.

---

## Future Ideas (Post-v1)

*   **Write Support:** Archive creation (`snipe pack`). ZIP first, then 7z. TUI file browser for selecting files.
*   **Format Expansion:** 7z, TAR.GZ, ISO read support.
*   **SaaS Server:** HTTP API for web-based sniping. Metered/freemium. See `ARCHITECTURE.md` § Products.
*   **KMP Apps:** Android/iOS/Desktop via Compose Multiplatform + UniFFI. See `ARCHITECTURE.md` § Products.
*   **Cloud Archiving (Vault):** Stream files from cloud → compress → stream archive back.
*   **Plugin Hooks:** External tool integration (fzf, yazi) at search/select/browse decision points.
