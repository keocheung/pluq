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

### Phase 5: Security Hardening & v1 Polish (Completed)
- [x] **CLI rename to match product spec** — Folded `extract` + `extract-all` into `pluq get [glob] [-o dir] [--all]`. Gitignore-style glob (pattern with `/` matches full path, else basename) via `glob` crate. Binary renamed to `pluq` via `[[bin]]` in `pluq/Cargo.toml`.
- [x] **Shell completions** — `pluq completions {bash,zsh,fish,powershell,elvish}` emits script to stdout via `clap_complete`. Install pattern documented in subcommand help.
- [x] **Anti-bomb safeguards** — Implementation of `ResourceLimits` (max entries, header size, total unpacked, per-entry unpacked, and compression ratio guards). Enforced at the boundary of `ArchiveReader` in ZIP, RAR5, and RAR4 parsers.
- [x] **LZMA dictionary cap** — Memory-safe decoding safeguard that rejects or caps LZMA dictionary sizes to prevent OOM attacks (e.g., a 10KB archive requesting 4GB decompression buffer). Threaded through the `CodecConfig` layer.
- [x] **Consistent guard enforcement** — Limits are threaded through all extraction paths (`extract`, `test`, `extract_selected`, `extract_all_streaming`) across both `archive-core` readers and the high-level `Archive` facade.
- [x] **Path traversal guard** — Centralized path safety logic in `archive-core` via `PathPolicy` (Sanitize/Strict). Strips `..`, absolute roots, drive/UNC prefixes. Enforced in the state layer across all extraction methods. Ad-hoc CLI duplicate removed.
- [x] **New `archive-cache` crate** — Migrated cache management logic to a dedicated crate; strictly enforced architectural boundaries and sealed `Archive` internal format accessors (Golden Rule #1); relocated `FormatCacheState` to `archive-core`.
- [x] **HTTP fingerprint enrichment** — Added ETag and Last-Modified equivalent capture to HTTP, Google Drive, and Dropbox readers to enable robust server-side change detection without extra round-trips.
- [x] **Glob extraction in TUI** — Extended search box to handle `*`, `?`, `[` patterns; bulk-toggle matching entries for extraction; shared `glob_match` helper between CLI and TUI.
- [x] **`.gitignore` Cargo.lock fix** — ignore only `crates/*/Cargo.lock`; commit root lockfile (binary crate convention).
- [x] **File logging always-on** — `tracing-appender` rolling file → `directories::ProjectDirs` state path. Cap 5 MB. Redact URLs / tokens at log site.
- [x] **`human-panic` integration** — friendly panic message + log path on crash.
- [x] **Versioned cache schema** — `schema_version: u32` field (default `0` for old files); version check first in `is_valid`; `load` emits `info!` on mismatch and returns `InvalidData`; `SCHEMA_VERSION = 1` exported from `archive-cache`.
- [x] **Symlink detection and verification** — Parsers mark `is_symlink` (bumped schema to v2); extraction verifies target stays within output directory bounds. Fallback behavior (file containing path) retained for v1.
- [x] **Solid RAR UX fix** — Solid entries propagate through `BatchResult.failed` instead of being silently skipped; CLI display appends `"→ request support: <url>"` suffix for `ArchiveError::UnsupportedFormat` errors; TUI result summary updated to show first failure reason.

### Phase 6.1: Engineering Prep (Completed)
- [x] **Final naming pass** — verified: production source + all `Cargo.toml` already on `pluq` / `PLUQ_*`; bin crate is `pluq`. Residual hits confined to gitignored/personal files + the `mempalace.yaml` wing name (intentional).
- [x] **MSRV** — `rust-version = "1.88"` via `[workspace.package]`, inherited by all nine crates. Enforced by the CI `msrv` job.
- [x] **License metadata** — `license = "MIT OR Apache-2.0"` via `[workspace.package]`, inherited by all crates.
- [x] **CI workflow** — `.github/workflows/ci.yml`: `fmt`, `clippy`, `doc`, `msrv@1.88`, `test` matrix (mac/linux/win), `cargo-deny`; on PR + push to main.
- [x] **`cargo clippy --all-targets --all-features`** clean (CI gate).
- [x] **`cargo doc --no-deps`** clean (CI gate).
- [x] **`cargo deny`** wired into CI (`deny.toml`: licenses + advisories + bans + sources; unrar git source allow-listed).

### Phase 6.2: Repo restructure (Completed 2026-05-30)
- [x] **`.workspace/` convention** — moved personal/transient files (`dev_links.json`, `temp/`, `docs/sandbox/`, `docs/superpowers/`, `entities.json`) into `.workspace/`.
- [x] **`.gitignore`** collapsed to a single `.workspace/` rule + agent/editor/OS files (kept `SESSION_PLAN*.md`, test-fixture allowlist, mempalace files).
- [x] **`.geminiignore`** trimmed to universal rules only — keeps `.workspace/` visible to Gemini (session plans).
- [x] **Root `CLAUDE.md` + `GEMINI.md`** reduced to redirect stubs (→ `AGENTS.md` + `CONTEXT.md`).
- [x] **`CONTEXT.md`** trimmed to architecture-only; the agent-process section moved to `AGENTS.md`.

### Phase 6.3: OSS docs & scaffolding (Completed 2026-05-30 — demo GIF + FUNDING.yml deferred, still in ROADMAP)
- [x] **`AGENTS.md`** — vendor-neutral working conventions (absorbs the trimmed `CONTEXT.md` process bits).
- [x] **`README.md`** — pitch, install matrix, quickstart, badges (CI/license/MSRV), maintenance-status; OneDrive marked planned-only.
- [x] **`LICENSE-MIT` + `LICENSE-APACHE`** (dual-license) + `Cargo.toml` package metadata (repository/homepage/description/keywords/categories).
- [x] **`LICENSE-THIRD-PARTY.md`** — UnRAR license reproduced **verbatim** (clause 2 obligation); fork verified FFI to the original UnRAR C++ source; extraction-only compliance documented.
- [x] **`CONTRIBUTING.md`, `SECURITY.md`** (GH Security Advisories), **`CODE_OF_CONDUCT.md`** (Contributor Covenant 2.1).
- [x] **`CHANGELOG.md` + `cliff.toml`** (git-cliff; placeholder until release-time generation in 6.7).
- [x] **`.github/`** — `ISSUE_TEMPLATE/` (bug + feature + config) + `PULL_REQUEST_TEMPLATE.md` + `dependabot.yml` (cargo + github-actions).
- [x] **`docs/INSTALL.md`, `docs/USAGE.md`, `docs/AUTH.md`** — incl. macOS Gatekeeper note, ToS scope statement, and the embedded-keys decision (rclone-model: embed app-level creds, per-user token stays local, `PLUQ_*` BYO override).

### Phase 6.4: feedback / bug-report / privacy (Completed 2026-05-31)
> New deps: `webbrowser`, `os_info` (`human-panic` was already present). Shared repo URL consts live in `main.rs`; they 404 until the repo goes public in 6.7.
- [x] **`pluq feedback`** — opens the browser (`webbrowser` crate). Flags `--bug` (new-issue bug template), `--idea` (Discussions Ideas category), `--say-hi` (Discussions home); no flag → Discussions home. Prints the URL as a fallback when no browser is available.
- [x] **`pluq bug-report`** — writes a rich, **redacted** diagnostic to `pluq-bugreport-<ts>.txt` in the CWD; prints the path, "nothing is sent anywhere", and the new-issue URL. **Never uploads.** Payload (every section error-tolerant, all dynamic text through `redact::scrub_report`): pluq version + git-sha + build timestamp + target triple (via `build.rs` / `build_info`); OS + OS-version (`os_info`) + arch; configured backends (gdrive/dropbox/local — **names only**); `PLUQ_*` env var **names only** (never values); `$TERM` + terminal size; scrubbed 200-line tail of the newest log (where the 4-tier error chain surfaces); newest `report-*.toml` crash report if present.
- [x] **Panic-hook** (`human-panic` 2.x) — already wired in `main()`; set its `homepage` to the repo URL. Release-build, panic-only; writes a local crash TOML and points the user at the support log.
- [x] **`redact::scrub_report`** — diagnostic scrubber layering home-dir redaction (`/Users/<name>`, `/home/<name>`, `C:\Users\<name>` → `~`) over the existing URL+token `scrub`. The logging hot path (`scrub`) stays untouched.
- [x] **Privacy docs** — `PRIVACY.md` (zero-telemetry, local-only, no-transmit, "you choose to attach") + README Privacy section + bug-report attach-warning appended to the issue template.
- **Deferred (intentionally not done):** archive-filename stripping inside reports — low value once home paths collapse to `~`, and over-stripping kills diagnostic value; revisit only on a real leak. Panic-message runtime-path scrubbing relies on `human-panic`'s defaults rather than a bespoke pass.

### Phase 6.5: Distribution pipeline (Completed 2026-06-01)
> Config + `release.yml` landed and `dist plan` green; real artifact build proven at the first tagged release (6.7).
- [x] **`cargo-dist`** setup + GH Actions release workflow (compiles binaries + generates installers; also drives the launch release in 6.7). Config: `installers = ["shell", "powershell", "homebrew", "msi"]`.
- [x] **One-liner install scripts** — `curl ... | sh` (unix/mac) + `irm ... | iex` PowerShell twin (Windows). Hero UX on every OS; both auto-generated by `cargo-dist`, both dodge Gatekeeper/SmartScreen (curl/irm don't set the quarantine / mark-of-the-web flag).
- [x] **Homebrew tap** (`brew install`) — smoothest mac path (sidesteps Gatekeeper friction the script can't). cargo-dist auto-generates + auto-publishes the formula per release; one-time setup = create empty **`stefandekanski/homebrew-tap`** repo + config (`tap = "stefandekanski/homebrew-tap"`, `publish-jobs = ["homebrew"]`). Near-free after.
- [x] **MSI installer** (Windows) — cargo-dist-native, auto-generated. Native double-click install for GUI-leaning Windows users.
- [x] GitHub Releases prebuilt binaries (mac x64/arm64, linux x64/arm64, windows x64) — the artifacts the installers download. Also the **fallback** for curl-averse / offline / CI-automation users. Not the recommended human path (manual place-on-`PATH`); just not hidden.

### Phase 6.6: Pre-publish security audit (Completed 2026-06-01)
> Audit-only gate; no engine work. First pass (2026-05-31) reported clean but
> **missed** real test-fixture share-links — caught on the post-6.5 re-run.
- [x] **Credential / share-link history sweep.** `git log --all -p` across all
  history for token formats (`ghp_`/`ya29.`/`AKIA`/`AIza`/`sk-`/private keys),
  cloud share-links, and home paths. Token-format + home-path axes clean. No
  committed `*_EMBED` API keys.
- [x] **Redacted 3 real personal share-references** via `git filter-repo
  --replace-text` (targeted value-only scrub, not the dropped whole-file
  `--invert-paths` op): 2 Google-Drive file IDs (one was live in a `main.rs`
  test fixture) + 1 Dropbox share-link with its `rlkey`. Shares
  left live on purpose (still used for local testing via the gitignored
  `dev_links.json`); the public history simply no longer points at them. SHAs
  rewritten + force-pushed while the repo was still private.
- [x] **Purged GitHub PR refs.** Force-pushing scrubbed `main` was *necessary but
  not sufficient*: GitHub retains every PR's head as a read-only server ref
  (`refs/pull/<N>/head`) that a rewrite + force-push does **not** delete and the
  owner cannot self-service remove — those still exposed the scrubbed values.
  Closed by **deleting + recreating** the (private, no-fork, solo) repo from the
  verified-clean local clone, then re-pushing `main`, re-adding the
  `HOMEBREW_TAP_TOKEN` secret, and re-filing the one tracked issue.
  `git ls-remote origin 'refs/pull/*'` now empty. All of this happened while
  private → no scrubbed SHA ever reached a public viewer.

### Phase 6.7: Launch (In Progress)
- [x] Enable GH Discussions (categories: Q&A, Ideas, Show & Tell) — targets for `pluq feedback`.
- [x] Set GitHub repo description (≤150 chars) + topics (`rust`, `cli`, `tui`, `archive`, `zip`, `rar`, `google-drive`, `dropbox`, `extraction`).
- [x] Tag `v0.1.0` → triggers `cargo-dist` release (GH Release binaries).
