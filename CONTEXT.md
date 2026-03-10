# Archive Sniper — Project Context & State

## [Dynamic State] - AGENTS MUST UPDATE THIS SECTION

* **Current Phase:** Phase 2 — Core engine: remaining formats + write support.
* **Current Active Task:** SESSION_PLAN.md Step 9 — archive-state: Archive Handle + EntryTree + ArchiveCache.
* **Baseline:** 50 tests pass, clippy clean. SESSION_PLAN.md Steps 1–8 complete.

---

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### Agent Developer Guidelines

1. **Commit Messages:** All agents MUST use **Conventional Commits** (e.g., `feat:`, `fix:`, `docs:`, `chore:`, `test:`). Scope the commit to the crate if applicable (e.g., `feat(archive-core): ...`). Do not use capitalized imperative mood strings without a conventional prefix.
2. **Architecture:** `ARCHITECTURE.md` is the canonical specification for system architecture (layers, traits, formats, codecs). `ERRORS.md` is the canonical specification for error types and error flow across layers (the layered translation model, Transport tunnel, decorator pattern). Both are final — not RFCs. **If any value in `CONTEXT.md` conflicts with `ARCHITECTURE.md` or `ERRORS.md`, the spec file wins.** Agents must note any conflict they find and ask Stefan, but must assume the spec files are correct until told otherwise. `CONTEXT.md` tracks state; the spec files define truth.

### The Product

**Archive Sniper** is a modern archive management tool. Think of it as a clean, modern replacement for WinRAR / 7-Zip / The Unarchiver, with a unique killer feature: **cloud sniping**.

> **⚠️ Working Name:** "Archive Sniper" is a placeholder. The final product name has not been decided. Keep code, crate names (`archive-core`, `archive-cli`), and comments name-agnostic where possible so renaming is painless later.

**Core capabilities:**

1. **Local management:** Browse, extract, and compress archives on disk.
2. **Cloud sniping:** Extract individual files from cloud storage (Drive/Dropbox) via HTTP range requests — without downloading the whole archive.
3. **Cloud archiving (future):** Compress files already on cloud storage into an archive. Streaming approach (user needs free space) or "Vault" approach (we temporarily hold data on S3 for users without enough space).

**UX Philosophy:** Simplicity and sanity out of the box. The default experience should require **≤3 configuration choices** and satisfy the vast majority of users. A future "Advanced mode" may expose power-user options, but simplicity comes first.

**Monetization (Credit-Based):**

* **Cloud sniping:** Freemium — 256MB/month free transfer, buy GB packs for more.
* **Cloud archiving (Vault):** Separate pay-per-operation — user pays upfront based on estimated GB needed. Priced to cover S3 + bandwidth costs + margin. Not part of freemium quota.

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

```rust
fn entries(&self) -> Result<Vec<ArchiveEntry>, ArchiveError>;
fn extract(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError>;
```

* The state layer (Layer 3.5) talks to `ArchiveReader` / `Archive`. Products (CLI, server, KMP apps) talk to the state layer.
* Each format implementation either uses our own parsing code or wraps an external crate.

### Format Strategy

| Format     | Parser Strategy          | Sniping Support                          | Compression Info                             |
|------------|--------------------------|------------------------------------------|----------------------------------------------|
| **ZIP**    | Our own code + `flate2`  | ✅ Perfect (surgical)                    | Method stored per entry in local file header |
| **TAR**    | Our own code             | ❌ Sequential scan                       | No compression (container only)              |
| **TAR.GZ** | `flate2` + our TAR       | ❌ Full download                         | Must decompress from byte 0                  |
| **RAR**    | Wrap external crate      | ✅ Good (non-solid) / ⚠️ Partial (solid) | Method in file header                        |
| **7z**     | Wrap external crate      | ⚠️ Partial (block-based)                 | Method in header metadata                    |

**Rationale:** Own code for simple/critical formats (ZIP, TAR) where we need surgical control over range requests. Wrap battle-tested external crates for complex/proprietary formats (RAR, 7z) where reimplementing compression algorithms isn't feasible.

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
* **`archive-state`** [TO BUILD] is the State Layer (Layer 3.5) — a **universal cache layer**. It holds session state, metadata caching (`ArchiveCache`), and entry tree building (`EntryTree`). It does NOT hold auth, retry, billing, or quota logic — those are product-level concerns in horizontal crates (see `ARCHITECTURE.md` § Horizontal Capabilities). The state layer receives a `ReadAt` from the product above (possibly decorated, but state layer doesn't know or care), passes it to core, translates core errors into `StateError`, and tunnels decorator errors blindly. All products go through the state layer — never call `archive-core` directly.
* **`archive-cli`** is a **thin shell** — just parses arguments and calls the state layer. If CLI code gets "fat," that's a smell that the state layer or core is missing a high-level API.
* **`archive-dev`** should ideally use the CLI as an entry point (`cargo run -p archive-cli -- ...`). If dev needs to write code, it should call `Archive::open()` — if that's not enough, the core is missing something.

### Git Convention

* Agents **MUST NOT** auto-commit or auto-push. Always ask Stefan for permission first. Commits should happen on a per-task basis after code review.

### Build Order (Roadmap)

> **CONTEXT.md is the single source of truth for project planning and build ordering.** `ARCHITECTURE.md` and `ERRORS.md` define *what* to build; this file defines *when* and *in what order*. Do not put ordering or priority lists in the spec files.

**Philosophy: Core-first, CLI-first, UI-last.** The engine and CLI must reach feature parity before any UI work begins. This keeps the core UI-agnostic and means every capability can be tested without a UI.

#### Phase 1 — Foundation [COMPLETE ✅]

* `ReadAt` trait (local + HTTP), `ArchiveError`
* ZIP parser (EOCD + Central Directory, Deflate/Store) — our own code
* `ZipIndex` — JSON-serializable cache for parsed listings
* `archive-cli` — `list` / `extract` subcommands (local + HTTP auto-detect)

#### Phase 2 — Core Engine: All Major Formats + Write Support [ACTIVE]

Goal: cover ~99% of real-world archives. Test everything via CLI.
Format priority order: **ZIP → RAR → 7z → TAR** (by real-world usage).

**Done:**

* ✅ Core abstractions — codec layer, `ArchiveReader` trait, error model (`ERRORS.md`), format-agnostic `ArchiveEntry`
* ✅ ZIP read support — parsing, extraction, CRC verification, legacy filename decoding, cloud sniping E2E validated
* ✅ CLI foundations — `list`, `extract`, `test` subcommands

**Active — shared infrastructure (cheaper now, benefits all formats):**

* Additional shared codecs (LZMA, BZip2, Zstd) — ZIP uses them, 7z needs them
* State layer (`archive-state`) — universal cache layer: session boundary, metadata cache, entry tree, error translation

**Active — format expansion:**

* **RAR support** ← current task — wrap external crate (read + extract only)
* **7z support** — wrap external crate (read + extract)
* **TAR / TAR.GZ parser** — our own code (sequential; no cloud sniping)

**Later in Phase 2:**

* **Write support (local):** Create ZIP and TAR archives (skip RAR — proprietary)
* CLI: `compress` subcommand, progress reporting, multi-entry extract

#### Phase 3 — Cloud Sniping Server (SaaS)

* HTTP API server — primarily serves the **web site** version, which can't do direct HTTP range requests (CORS, no Range header control)
* OAuth token flow for Google Drive / Dropbox (server holds tokens for cloud providers)
* Credit / quota accounting (freemium model) — metering and billing live **only** in the server crate
* Client apps (Phase 4) are independent cloud snipers — they do NOT depend on this server for core functionality

#### Phase 4 — Client Apps (KMP + CLI)

* **KMP (Kotlin Multiplatform) project** — Compose Multiplatform for shared UI across Android, iOS, and 3 desktop platforms (macOS, Windows, Linux). Rust core consumed via UniFFI.
* Client apps are **standalone products**: buy once, no metering. They connect directly to Dropbox/Drive for cloud sniping. Full local archive management (read + write).
* Client apps may optionally call the SaaS API in certain conditions (future consideration), but are independent by default.
* **Web site** — consumes the SaaS API (Phase 3). Not a Rust product. Metered/freemium.
* Only starts once Phase 2 is fully proven via CLI

#### Phase X (Future) — Cloud Archiving ("Vault" / "Reverse Snipe")

* Stream files from cloud → compress on our servers → stream archive back to cloud
* Separate pay-per-operation billing
