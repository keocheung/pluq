# Archive Sniper — Product Roadmap & Scope

## The Vision

Create the fastest, most surgical way to browse and extract files from cloud-hosted archives (ZIP/RAR5) without downloading the entire file.

### Target v1 Interface (Product Spec)

These commands define the CLI's public API. The TUI is the default entry point.

*   `snipe <url-or-path>` → **Interactive TUI browser** (The primary experience)
*   `snipe list <url-or-path>` → Non-interactive listing (for scripting)
*   `snipe get <url-or-path> [glob] [-o <dir>]` → Non-interactive extraction; no glob = extract all; `-o` sets output directory (default: `./<archive-stem>/`)
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

### Phase 3a: Core Extraction + Multi-select
- [x] `ratatui` integration in `archive-cli`.
- [x] Single-file extraction with progress UI (Scanning + Extracting phases).
- [x] **`archive-source` refactor** — Create `archive-source` crate; centralize URL detection, credential discovery, and reader construction. Delete `Source` enum + `DynReadAt` from `archive-core`. Make `Archive` generic over `R: ReadAt`. Move CLI `provider.rs` logic into `archive-source`. See `docs/SOURCE.md` for full spec.
- [x] **Esc to cancel extraction** — Cancel in-progress extraction and return to browse (Ctrl-C already wired for CLI; TUI now uses Esc for surgical cancellation).
- [x] **Large dictionary safety gate** — Detect RAR5 archives requiring >4GB RAM and warn the user before extraction.
- [ ] **Multi-select extraction** — Space to toggle entries, bulk extract. Requires a new `extract_selected(&self, entries, sink_factory, ...)` method on `ArchiveReader` (archive-core) that sorts entries by physical offset and coalesces adjacent entries into minimal `read_stream` calls — reducing N HTTP requests to a small number of clustered reads. Default trait impl falls back to a loop of `extract()` (correct for local files). Format parsers (ZIP, RAR5, RAR4) provide optimized overrides using their private `FormatLocation` offset data.
- [ ] **`extract-all` loop fix** — currently loops `archive.extract()` per entry (N HTTP requests for cloud RAR). Once `extract_selected` exists, route `ExtractAll` through it instead.
- [ ] **Selection summary** — Footer updates live as entries are toggled: "12 files selected — 48.3 MB".

### Phase 3b: TUI Polish
> Items in this phase are independent and can be tackled in any order.

- [ ] **Search / filter** — Type to filter the entry list in real time (fuzzy or substring).
- [ ] **Tree-based navigation** — Collapse/expand directory nodes; select/deselect entire subtrees.
- [ ] **Sort by columns** — Key to cycle sort order (name / size / compressed size) in the browse table.
- [ ] **Extraction cost display** — Show estimated download cost per entry before extracting (`extraction_cost()` already implemented in `ArchiveReader`). Critical UX for cloud archives where extracting one small file may require downloading a large block.
- [ ] **Speed + ETA during extraction** — Show MB/s and estimated time remaining in the extraction progress screen.
- [ ] **`?` help popup** — Overlay showing all keybindings, toggled with `?`.
- [ ] **Password-protected archive support** — `extract()` already accepts `Option<&[u8]>`; TUI detects the password error and shows an inline prompt.

### Phase 3c: Home Screen
- [ ] **Home screen** — Entry point with URL/path input, saved bookmarks, and recents (see `TUI_VISION.md`).

### Phase 4: Cloud Provider API Readers
> API-key / app-credential access for publicly shared files. No OAuth required.
> Each provider is a separate `archive-{provider}` crate implementing `ReadAt`.
> Source resolution (URL detection, credential discovery, reader construction)
> is centralized in the `archive-source` crate. See `docs/SOURCE.md`.

- [x] **Google Drive** — `archive-gdrive` crate (`GoogleDriveReadAt`). Uses Drive v3 API for size/metadata, delegates range reads to inner `HttpReadAt`. API key from env var or embedded default.
- [ ] **Dropbox** — `archive-dropbox` crate (`DropboxReadAt`). Shared-link metadata via `POST /2/sharing/get_shared_link_metadata`, range reads via `POST /2/sharing/get_shared_link_file`. App access token in `Authorization: Bearer` header.
- [ ] **OneDrive** — `archive-onedrive` crate. Resolve share link → driveItem via Microsoft Graph API, download with Range headers. *(v1.x — deferred)*

### Phase 4.5: OAuth2 — Private File Access
> Builds on Phase 4 provider readers. Allows accessing private (non-shared) files
> by obtaining user consent via OAuth2 flow.

- [ ] OAuth2 flow for **Google Drive** (browser redirect, token exchange).
- [ ] OAuth2 flow for **Dropbox**.
- [ ] Token storage (`~/.config/archive-sniper/tokens.json`) and refresh.
- [ ] `archive-auth` ReadAt decorator — injects `Authorization: Bearer` header, handles token refresh transparently.
- [ ] TUI auth prompts — detect `NotPublic`/`Unauthenticated` errors, guide user through OAuth flow in-app.
- [ ] **`snipe auth add/remove/list`** — CLI surface for managing stored OAuth tokens.

### Phase 5: Security Hardening & Release
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

## Post-v1

- [ ] Source-aware default folder naming (use real GDrive/Cloud filename instead of URL-guessing).
- [ ] Parallel extraction for ZIP/non-solid RAR.
- [ ] Advanced cache invalidation (content hashing, detect modifications between opens).
- [ ] Configurable cache location (CLI flag or config file).
- [ ] Cache size limits / LRU eviction.
- [ ] Full range support detection with retry/fallback and user-facing guidance.
- [ ] OneDrive provider (`archive-onedrive` crate) + OAuth2.

---

## Future Ideas

*   **Write Support:** Archive creation (`snipe pack`). ZIP first, then 7z. TUI file browser for selecting files.
*   **Format Expansion:** 7z, TAR.GZ, ISO read support.
*   **SaaS Server:** HTTP API for web-based sniping. Metered/freemium. See `ARCHITECTURE.md` § Products.
*   **KMP Apps:** Android/iOS/Desktop via Compose Multiplatform + UniFFI. See `ARCHITECTURE.md` § Products.
*   **Cloud Archiving (Vault):** Stream files from cloud → compress → stream archive back.
*   **Plugin Hooks:** External tool integration (fzf, yazi) at search/select/browse decision points.
