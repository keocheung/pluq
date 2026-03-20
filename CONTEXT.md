# Archive Sniper — Project Context & State

## [Dynamic State] - AGENTS MUST UPDATE THIS SECTION

* **Current Phase:** Phase 2 — Core engine: RAR read support + state layer.
* **Current Active Task:** HTTP redirect pre-resolution complete. SESSION_PLAN fully done. Next: State layer (`archive` crate) — Phase 2 continuation.
* **Baseline:** 156 tests pass + 11 ignored cloud E2E tests (archive-core: 108 unit + 23 integration [7 ZIP + 16 RAR] + 11 ignored cloud_e2e, archive: 25), clippy clean.
* **Note:** `CoreError` replaces old `ArchiveError` (core error type). `ArchiveError` replaces old `StateError` (product-facing error type from the `archive` crate). `archive` crate replaces old `archive-state`.

---

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### The Knowledge Tiers

*   **Tier 1 (Switchboard):** `CONTEXT.md` (You are here). Always read this first for orientation and current state.
*   **Tier 2 (Canonical Pillars):** `docs/ARCHITECTURE.md` and `docs/ERRORS.md`. Read on-demand when planning new features, resolving ambiguity, or ensuring your code aligns with the project's core philosophy.
*   **Tier 3 (Sandbox):** `docs/sandbox/`. Transient and on-demand documents for human needs (experiments, learning logs). **IGNORE BY DEFAULT**. Only read if explicitly instructed to do so for a specific task.

### Agent Developer Guidelines

1. **Commit Messages:** All agents MUST use **Conventional Commits** (e.g., `feat:`, `fix:`, `docs:`, `chore:`, `test:`). Scope the commit to the crate if applicable (e.g., `feat(archive-core): ...`). Do not use capitalized imperative mood strings without a conventional prefix.
2. **Commit Message Bodies:** When adding a commit message body, use a **strict list-based format** with a hyphen and space (`-`) bullet points. Do not write paragraphs. This makes commits easier to scan.
3. **Architecture:** `docs/ARCHITECTURE.md` is the canonical specification for system architecture (layers, traits, formats, codecs). `docs/ERRORS.md` is the canonical specification for error types and error flow across layers. Both are final — not RFCs. **If any value in `CONTEXT.md` conflicts with `docs/ARCHITECTURE.md` or `docs/ERRORS.md`, the spec file wins.** Agents must note any conflict they find and ask Stefan, but must assume the spec files are correct until told otherwise. `CONTEXT.md` tracks state; the spec files define truth.

### The Product

**Archive Sniper** is a surgical cloud archive extraction tool. Extract individual files from cloud-hosted archives (Google Drive, OneDrive, Dropbox) via HTTP range requests — without downloading the whole archive. Also works on local archives.

> **⚠️ Working Name:** "Archive Sniper" is a placeholder. The final product name has not been decided. Keep code, crate names (`archive-core`, `archive-cli`), and comments name-agnostic where possible so renaming is painless later.

**v1 identity: Cloud sniping CLI with interactive TUI.**

The tool focuses on **extraction only** — no compression, no archive creation. The core experience: point snipe at a cloud URL or local file, browse the contents interactively, select what you need, extract it.

**v1 capabilities:**

1. **Cloud sniping:** Extract individual files from Google Drive / OneDrive / Dropbox archives via HTTP range requests — without downloading the whole archive.
2. **Local extraction:** Same TUI experience for local archives — browse, select, extract.
3. **Interactive TUI:** `ratatui`-based archive browser — tree navigation, expand/collapse, pagination, select/deselect files and folders, selection overview, built-in fuzzy search.
4. **Direct HTTP URL support:** Works on any URL that supports Range requests (S3, Backblaze, CDNs) — no auth needed.

**Formats (read only, v1):** ZIP and RAR — the two snipe-friendly formats that support random access. TAR/TAR.GZ/7z are not snipe-friendly and are deferred to post-v1.

**CLI interface:**

* `snipe <url-or-path>` → interactive TUI browser
* `snipe list <url-or-path>` → non-interactive listing (for scripting)
* `snipe get <url-or-path> <path/glob>` → non-interactive extraction (for scripting)
* `snipe test <url-or-path>` → verify archive integrity (decompress + CRC, extract nothing)
* `snipe auth add/remove/list` → manage cloud provider OAuth tokens
* `snipe feedback` → link to GitHub issues

**UX Philosophy:** Simplicity and sanity out of the box. The default experience should require **≤3 configuration choices** and satisfy the vast majority of users. The "fun" factor comes from the interactive TUI — no memorizing file paths, no manual typing. Browse, search, select, extract.

**Progressive enhancement:** Built-in fuzzy search works standalone. If `fzf` is installed, snipe can optionally delegate search to it for power users. Future plugin hooks at key decision points (search, select, browse) for external tools.

**Monetization (future, post-v1):**

* **Cloud sniping:** Freemium — 256MB/month free transfer, buy GB packs for more.
* **Cloud archiving (Vault):** Separate pay-per-operation. Not part of v1.

### Core Architecture (`archive-core`)

The heart of the product is a Rust crate with two layers of abstraction:

#### Layer 1: I/O Abstraction — `ReadAt` Trait [LOCKED]

Abstracts *where* bytes come from. All parsers consume this. Dual-method design: `read_range` for bounded header reads, `read_stream` for large entry data extraction.

```rust
fn read_range(&self, offset: u64, len: u64) -> impl Future<Output = Result<Bytes, ReadError>> + Send;
fn read_stream(&self, offset: u64, len: u64) -> impl Future<Output = Result<Box<dyn AsyncRead + Send + Unpin>, ReadError>> + Send;
fn size(&self) -> impl Future<Output = Result<u64, ReadError>> + Send;
```

* **Implementations (done):** `LocalReadAt` (disk), `HttpReadAt` (HTTP range requests with optional auth headers). Both implement all three methods — no defaults.
* The parser never knows/cares where the bytes come from.

#### Layer 2: Format Abstraction — `ArchiveReader` Trait

Abstracts *what format* the bytes represent. Each format gets its own implementation.

* `entries()` — returns the parsed entry list
* `extract(entry, sink)` — streams decompressed bytes to a sink (file, buffer, etc.)
* `test(entry)` — decompress + verify CRC, write nothing
* `extraction_cost(entry)` — bytes that must be downloaded to produce this entry

See `ARCHITECTURE.md` § ArchiveReader for the canonical trait definition.

* The state layer (Layer 3.5) talks to `ArchiveReader` / `ArchiveCore`. Products talk to the state layer.
* Each format implementation either uses our own parsing code or wraps an external crate.

### Format Strategy (v1 — snipe-friendly formats only)

| Format     | Parser Strategy          | Sniping Support                          | v1 Scope |
|------------|--------------------------|------------------------------------------|----------|
| **ZIP**    | Our own code + `flate2`  | ✅ Perfect (surgical)                    | ✅ v1    |
| **RAR**    | Wrap external crate      | ✅ Good (non-solid) / ⚠️ Partial (solid) | ✅ v1    |
| **7z**     | Wrap external crate      | ⚠️ Partial (block-based)                 | Post-v1  |
| **TAR**    | Our own code             | ❌ Sequential scan                       | Post-v1  |
| **TAR.GZ** | `flate2` + our TAR       | ❌ Full download                         | Post-v1  |

**Rationale:** v1 ships only snipe-friendly formats — ZIP and RAR support random access, making surgical cloud extraction possible. Formats that require sequential scanning or full download defeat the core value proposition and are deferred. Own code for ZIP (surgical control), wrap external crate for RAR (proprietary/complex).

### Technical Stack & Constraints

* **Core engine:** Rust (memory-safe, async I/O).
* **Core I/O abstraction:** `ReadAt` trait [LOCKED].
* **ZIP parsing:** Our own code (reading EOCD + Central Directory + entries via `ReadAt`).
* **Decompression:** `flate2` (Deflate — used by ZIP), external crates for LZMA/RAR as needed.

### Code Quality Convention

* **`archive-core` / `archive-cli`** — Production quality. Clean, tested, idiomatic Rust. These crates ship to users.
* **`archive-dev`** — Scratchpad for real-world experiments. Calls the CLI or `Archive::open()`. Never depended on by production crates.
  * **"Dev it" / "Dev this"** — Stefan shorthand: *write dev code for this.* Agent should add a test scenario to `crates/archive-dev/src/main.rs` that exercises the described feature via the CLI.
  * **`crates/archive-dev/output/`** — gitignored scratch folder; dump listing files, extracted files, etc. here.
  * If something is awkward to express in dev code, the core/CLI is missing an API — fix it there, not here.

### Architecture Convention

* **`archive-core`** holds all stateless archive logic (parsing, extraction, codecs, format detection). The core entry point is `Archive::open(reader)` which takes a `ReadAt` and auto-detects format. Core is stateless — no auth, no retries, no caching.
* **`archive`** is the State Layer (Layer 3.5) — a **universal cache layer**. It holds session state, metadata caching (`ArchiveCache`), and entry tree building (`EntryTree`). It does NOT hold auth, retry, billing, or quota logic — those are product-level concerns in horizontal crates (see `ARCHITECTURE.md` § Horizontal Capabilities). The state layer receives a `ReadAt` from the product above (possibly decorated, but state layer doesn't know or care), passes it to core, translates core errors into `ArchiveError`, and tunnels decorator errors blindly. All products go through the state layer — never call `archive-core` directly.
* **`archive-cli`** is the v1 product — the `snipe` binary. It owns the TUI (`ratatui`), CLI argument parsing (`clap`), OAuth flows, and cloud provider URL routing. Archive logic (parsing, extraction, decompression) must stay in the state layer or core — the CLI crate handles interaction, not archive internals.
* **`archive-dev`** should ideally use the CLI as an entry point (`cargo run -p archive-cli -- ...`). If dev needs to write code, it should call `Archive::open()` — if that's not enough, the core is missing something.

### Git Convention

* Agents **MUST NOT** auto-commit or auto-push. Always ask Stefan for permission first. Commits should happen on a per-task basis after code review.
* **Format:** Use the strict list-based format for commit message bodies (see Guidelines at the top of this document).

### Build Order (Roadmap)

> **CONTEXT.md is the single source of truth for project planning and build ordering.** `ARCHITECTURE.md` and `ERRORS.md` define *what* to build; this file defines *when* and *in what order*. Do not put ordering or priority lists in the spec files.

**Philosophy: Ship a focused, fun, complete cloud sniping CLI.** v1 is extraction-only with an interactive TUI. Compression, additional formats, and other products come after v1 ships.

#### Phase 1 — Foundation [COMPLETE ✅]

* `ReadAt` trait (local + HTTP), `CoreError`
* ZIP parser (EOCD + Central Directory, Deflate/Store) — our own code
* `ZipIndex` — JSON-serializable cache for parsed listings
* `archive-cli` — `list` / `extract` subcommands (local + HTTP auto-detect)

#### Phase 2 — Core Engine: Snipe-Friendly Formats [ACTIVE]

Goal: ZIP + RAR read support, state layer. Everything testable via CLI.

**Done:**

* ✅ Core abstractions — codec layer, `ArchiveReader` trait, error model (`ERRORS.md`), format-agnostic `ArchiveEntry`
* ✅ ZIP read support — parsing, extraction, CRC verification, legacy filename decoding, cloud sniping E2E validated
* ✅ CLI foundations — `list`, `extract`, `test` subcommands
* ✅ ZIP hardening — chunked extraction, progress, ZipCrypto, WinZip AES, DoS protection

**Active:**

* **RAR support** ← current task — wrap external crate (read + extract only)
* State layer (`archive`) — universal cache layer: session boundary, metadata cache, entry tree, error translation

#### Phase 3 — Interactive TUI

Goal: `ratatui`-based archive browser that makes extraction fun.

* Tree view of archive contents with pagination for huge archives
* Directory navigation — expand/collapse folders
* Select/deselect individual files or whole folders
* Selection overview panel (what you've picked, total size, estimated transfer)
* Built-in fuzzy search by filename
* Progress display + "transferred X of Y" stats (the wow moment)
* Optional `fzf` handoff for search when available (progressive enhancement)

#### Phase 4 — Cloud Auth + Sniping E2E

Goal: OAuth flow for cloud providers, end-to-end cloud sniping.

* `snipe auth add google` — browser-based OAuth dance, token storage
* `snipe auth add onedrive` — Microsoft Graph API, range request support
* `snipe auth add dropbox` — Dropbox API, range request support
* `snipe auth list` / `snipe auth remove <provider>`
* Direct HTTP URL support already works via `HttpReadAt` — just needs CLI wiring
* Cloud provider URL detection + auto-routing (detect Drive/OneDrive/Dropbox URLs)

#### Phase 5 — CLI Polish + Distribution

Goal: shippable binary, installable everywhere.

* Non-interactive CLI mode: `snipe list`, `snipe get`, `snipe auth`
* Shell completions (from `clap`)
* `snipe feedback` command (opens GitHub issues)
* GitHub Actions release pipeline (Linux x86, macOS arm64/x86, Windows)
* Homebrew tap
* macOS notarization
* `cargo install` from crates.io

---

#### Post-v1 — Future Phases (Unscheduled)

**Write support + additional formats:**
* Compression: ZIP, 7z, TAR.GZ creation (`snipe pack` subcommand)
* TUI filesystem browser for selecting files to pack
* 7z read support (wrap external crate)
* TAR / TAR.GZ read support (own code, sequential)

**Plugin hooks:**
* External tool integration at key decision points (search, select, browse)
* fzf, yazi, and other terminal tool interoperability

**SaaS server:**
* HTTP API for the web version (CORS workaround for browser-based sniping)
* OAuth token management server-side
* Credit/quota accounting (freemium model)

**Client apps (KMP):**
* Compose Multiplatform for Android, iOS, desktop
* Rust core consumed via UniFFI
* Standalone products, no metering

**Cloud archiving ("Vault" / "Reverse Snipe"):**
* Stream files from cloud → compress on our servers → stream archive back to cloud

**TAR.GZ random-access index (`zran`-style):**
* Snapshot gzip decompressor state at checkpoints during full scan
* On later extraction, seek to nearest checkpoint instead of byte 0
