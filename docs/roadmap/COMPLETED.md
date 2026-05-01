# pluq — Completed Work

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
- [x] Implicit persistent cache: `Archive::open` checks `~/.cache/pluq/[hash].json`; `PendingArchive::scan` saves after completion. Stores entries, locations, preamble, end_block.
- [x] Same-name output folder helper in `pluq` (`photos.rar` → extract to `photos/`).

### Phase 3a: Core Extraction + Multi-select
- [x] `ratatui` integration in `pluq`.
- [x] Single-file extraction with progress UI (Scanning + Extracting phases).
- [x] **`archive-source` refactor** — Create `archive-source` crate; centralize URL detection, credential discovery, and reader construction. Delete `Source` enum + `DynReadAt` from `archive-core`. Make `Archive` generic over `R: ReadAt`. Move CLI `provider.rs` logic into `archive-source`. See `docs/SOURCE.md` for full spec.
- [x] **Esc to cancel extraction** — Cancel in-progress extraction and return to browse (Ctrl-C already wired for CLI; TUI now uses Esc for surgical cancellation).
- [x] **Large dictionary safety gate** — Detect RAR5 archives requiring >4GB RAM and warn the user before extraction.
- [x] **Multi-select extraction** — Space to toggle entries, bulk extract. `extract_selected` on `ArchiveReader` (archive-core) with ZIP stream-coalescing override and default fallback. TUI: Space toggle, `a` select-all, Enter batch extract.
- [x] **`extract-all` loop fix** — CLI `ExtractAll` now routes through `extract_selected` instead of looping `archive.extract()` per entry.
- [x] **Selection summary** — Footer updates live as entries are toggled: "12 files selected — 48.3 MB".

### Phase 3b: TUI Polish & zen-mode
- [x] **Drill-down navigation** — `Right`/`l` to dive into folders, `Left`/`h`/`Backspace` to return to parent.
- [x] **Full-path selection** — Refactor `app.toggled` to use `HashSet<String>` (paths) instead of indices.
- [x] **zen-mode aesthetic** — Minimalist Cyan/Black theme, Size + Name columns only, strip borders.
- [x] **Tab Review mode** — `Tab` to see/prune selected files; `Esc` to return to browsing.
- [x] **Extraction cost display** — Show estimated download cost for the highlighted entry.
- [x] **Search / filter** — `/` to filter the current folder's entry list.
- [x] **`?` help popup** — Overlay showing all keybindings.
- [x] **Speed + ETA during extraction** — MB/s and estimated time remaining.
- [x] **Password-protected archive support** — Inline TUI prompt for encrypted archives.

### Phase 3c: Style Guide Compliance & Home Screen
- [x] **Fix 'q' key inconsistency** — 'q' quits even when PluqInput is focused (URLs with 'q' untypable). Fixed via Ctrl+q for quitting when input is focused.
- [x] **Migrate non-browse screens to borderless layout** — Opening, Scanning, Extracting, ExtractingBatch, ConfirmExtract, and Error phases still use bordered `Block`. Migrate to the Content Screen layout (5-row, `─` separators).
- [x] **Fix scanning gauge color** — Change from `Color::Green` to `accent` (Cyan) per palette.
- [x] **Esc never quits** — Esc should be "back one level" only; remove quit-on-Esc-at-root. `q` is the sole exit.
- [x] **Directory selection** — `Space` on a directory should toggle-select all its files recursively.
- [x] **Document `g`/`G`/`Home`/`End` in help popup** — These bindings exist in code but aren't shown in `?` overlay.
- [x] **Extraction strategy modal** — Pre-extraction destination picker (archive-name folder / here / custom name). See zen-mode spec Section 6.1.
- [x] **Home screen** — Entry point with URL/path input, bookmarks, and recents (see `TUI_VISION.md` and home-screen spec).

### Phase 4: Cloud Provider API Readers
- [x] **Google Drive** — `archive-gdrive` crate (`GoogleDriveReadAt`). Uses Drive v3 API for size/metadata, delegates range reads to inner `HttpReadAt`. API key from env var or embedded default.
- [x] **Dropbox** — `archive-dropbox` crate (`DropboxReadAt`). Shared-link metadata via `POST /2/sharing/get_shared_link_metadata`, range reads via `POST /2/sharing/get_shared_link_file`. App authentication via `Authorization: Basic base64(app_key:app_secret)` header. Credentials from `PLUQ_DROPBOX_APP_KEY` + `PLUQ_DROPBOX_APP_SECRET` (or embedded at build time). No expiry.

### Phase 4.5: OAuth2 — Private File Access
- [x] **`archive-auth` crate** — `TokenStore` (JSON, atomic write, Unix 0600 perms), PKCE loopback flow via `oauth2` v5 + reqwest, `authenticate()` + `refresh()` helpers, `GDRIVE_CONFIG` / `DROPBOX_CONFIG` constants.
- [x] **Google Drive OAuth** — `GDriveAuth { ApiKey | OAuth }` enum. Reader refactored to direct-reqwest (dropped HttpReadAt). Refresh-on-401 under `tokio::sync::RwLock` with thundering-herd guard. Token persistence via `Arc<TokenStore>::save("gdrive", ...)`. `archive_gdrive::oauth::authenticate()` wrapper.
- [x] **Dropbox OAuth** — `DropboxAuth { AppCredentials | OAuth }` enum. Same refresh pipeline. Token stored under `"dropbox"`. `archive_dropbox::oauth::authenticate()` wrapper.
- [x] **`archive-source` wiring** — `SourceConfig.token_store: Option<Arc<TokenStore>>`. `resolve()` prefers OAuth when both token and client ID are present, falls back to API key / app credentials. Graceful warn-and-fallback when token exists but client ID is missing.
- [x] **`pluq auth {add, remove, list}`** — CLI subcommand for managing stored OAuth tokens. `list` shows provider + status + refresh-token presence (no secrets printed).
- [x] **TUI auth prompt** — `NotPublic` / `InvalidCredentials` on known providers surfaces an in-app prompt. Y runs the OAuth flow in a background task, N surfaces a hint to use `pluq auth add`. On success the failed archive open is retried transparently.

### Phase 5: Security Hardening & Release (in progress)
- [x] **CLI rename to match product spec** — Folded `extract` + `extract-all` into `pluq get [glob] [-o dir] [--all]`. Gitignore-style glob (pattern with `/` matches full path, else basename) via `glob` crate. Binary renamed to `pluq` via `[[bin]]` in `pluq/Cargo.toml`.
- [x] **Shell completions** — `pluq completions {bash,zsh,fish,powershell,elvish}` emits script to stdout via `clap_complete`. Install pattern documented in subcommand help.
- [x] **Anti-bomb safeguards** — Implementation of `ResourceLimits` (max entries, header size, total unpacked, per-entry unpacked, and compression ratio guards). Enforced at the boundary of `ArchiveReader` in ZIP, RAR5, and RAR4 parsers.
- [x] **LZMA dictionary cap** — Memory-safe decoding safeguard that rejects or caps LZMA dictionary sizes to prevent OOM attacks (e.g., a 10KB archive requesting 4GB decompression buffer). Threaded through the `CodecConfig` layer.
- [x] **Consistent guard enforcement** — Limits are threaded through all extraction paths (`extract`, `test`, `extract_selected`, `extract_all_streaming`) across both `archive-core` readers and the high-level `Archive` facade.
- [x] **Path traversal guard** — Centralized path safety logic in `archive-core` via `PathPolicy` (Sanitize/Strict). Strips `..`, absolute roots, drive/UNC prefixes. Enforced in the state layer across all extraction methods. Ad-hoc CLI duplicate removed.
- [x] **New `archive-cache` crate** — Migrated cache management logic to a dedicated crate; strictly enforced architectural boundaries and sealed `Archive` internal format accessors (Golden Rule #1); relocated `FormatCacheState` to `archive-core`.
- [x] **HTTP fingerprint enrichment** — Added ETag and Last-Modified equivalent capture to HTTP, Google Drive, and Dropbox readers to enable robust server-side change detection without extra round-trips.
- [x] **Glob extraction in TUI** — Extended search box to handle `*`, `?`, `[` patterns; bulk-toggle matching entries for extraction; shared `glob_match` helper between CLI and TUI.
