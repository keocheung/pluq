# Archive Sniper — Project Context & State

## [Dynamic State] - AGENTS MUST UPDATE THIS SECTION

* **Current Phase:** Phase 2 — Core engine: remaining formats + write support.
* **Current Active Task:** Test ZIP with real cloud file, then RAR support.
* **Latest Changes:**
  * [2026-02-25] Stefan: Revised roadmap — UI deferred until core engine is feature-complete. Core-first, CLI-first, UI-last.
  * [2026-02-25] Gemini: Completed Phase 1 — added `ZipIndex` (JSON serialization of parsed entries), `archive-cli` binary with `list` + `extract` commands, enum-based `Source` dispatch for local/HTTP. 15 tests passing. Initialized git repo, pushed to GitHub (SSH).
  * [2026-02-24] Gemini: Major architecture update — added `ArchiveReader` trait design, format strategy, broadened product vision, monetization model.
  * [2026-02-24] Gemini: Implemented `HttpReadAt` (range requests via `reqwest`), with `with_headers()` for future OAuth. 5 `wiremock` tests passing (10 total).
  * [2026-02-23] Gemini: Initialized Rust workspace, created `archive-core` crate with `ReadAt` trait (incl. `size()`), `ArchiveError`, `LocalReadAt`, and 5 passing unit tests. Created `GEMINI.md`.
  * [2026-02-23] Project initialized. `CONTEXT.md` and `CLAUDE.md` created.

---

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

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

Abstracts *where* bytes come from. All parsers consume this.

```rust
fn read_range(&self, offset: u64, len: u64) -> impl Future<Output = Result<Bytes, ArchiveError>> + Send;
fn size(&self) -> impl Future<Output = Result<u64, ArchiveError>> + Send;
```

* **Implementations (done):** `LocalReadAt` (disk), `HttpReadAt` (HTTP range requests with optional auth headers).
* The parser never knows/cares where the bytes come from.

#### Layer 2: Format Abstraction — `ArchiveReader` Trait

Abstracts *what format* the bytes represent. Each format gets its own implementation.

```rust
fn entries(&self) -> Result<Vec<ArchiveEntry>, ArchiveError>;
fn extract(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError>;
```

* The rest of the app (CLI, server, UI) only talks to `ArchiveReader`.
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
* **`archive-dev`** — Scratchpad. Quick experiments, real-file smoke tests, throwaway hacks. Run with `cargo run -p archive-dev`. Never depended on by production crates. Not production quality and that's fine.

### Build Order (Roadmap)

**Philosophy: Core-first, CLI-first, UI-last.** The engine and CLI must reach feature parity before any UI work begins. This keeps the core UI-agnostic and means every capability can be tested without a UI.

#### Phase 1 — Foundation [COMPLETE ✅]

* `ReadAt` trait (local + HTTP), `ArchiveError`
* ZIP parser (EOCD + Central Directory, Deflate/Store) — our own code
* `ZipIndex` — JSON-serializable cache for parsed listings
* `archive-cli` — `list` / `extract` subcommands (local + HTTP auto-detect)

#### Phase 2 — Core Engine: All Major Formats + Write Support [ACTIVE]

Goal: cover ~99% of real-world archives. Test everything via CLI.
Format priority order: **ZIP → RAR → 7z → TAR** (by real-world usage).

* **Test ZIP with real cloud file** — validate end-to-end sniping before moving on
* **RAR support** — wrap external crate (read + extract only; RAR creation is proprietary)
* **7z support** — wrap external crate (read + extract)
* **TAR / TAR.GZ parser** — our own code (sequential; no cloud sniping)
* **Write support (local):** Create ZIP and TAR archives from local files (skip RAR — proprietary)
* CLI subcommands: `compress`, progress reporting, multi-entry extract
* `ArchiveReader` trait — unified interface across all formats for CLI and future layers

#### Phase 3 — Cloud Sniping Server

* Minimal stateless HTTP server: accept cloud URL → return listing → extract single file
* OAuth token flow for Google Drive / Dropbox
* Credit / quota accounting (freemium model)

#### Phase 4 — UI (Desktop / Mobile / Web)

* Desktop app (macOS first, then Windows/Linux)
* Mobile (iOS / Android)
* Billing UI, cloud dashboard
* Only starts once Phase 2 is fully proven via CLI

#### Phase X (Future) — Cloud Archiving ("Vault" / "Reverse Snipe")

* Stream files from cloud → compress on our servers → stream archive back to cloud
* Separate pay-per-operation billing
