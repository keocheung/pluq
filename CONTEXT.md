# Archive Sniper — Project Context & State

## [Dynamic State] - AGENTS MUST UPDATE THIS SECTION

* **Current Phase:** Phase 3 — Interactive TUI + Google Drive Provider.
* **Current Active Task:** SESSION_PLAN.md step 4 — Replace RAR5 cloud `extract_all` path.
* **Baseline:** 211 tests pass, 11 ignored. (archive-core: 118 unit + 43 integration [21 rar + 13 zip + 9 rar4]; archive: 42 unit + 11 cloud_e2e ignored; archive-cli: 1 unit; archive-gdrive: 7 unit). 3 pre-existing clippy warnings (collapsible_if in run_elastic_scan, let_and_return in extract_all, too_many_arguments in zip/mod.rs).
* **Pre-existing:** `extract_all_via_unrar` free function already in `crates/archive-core/src/rar/mod.rs` (kept from prior work).


---

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### The Knowledge Tiers (How to read this project)

*   **Tier 1 (Switchboard):** `CONTEXT.md` (You are here). Read first for orientation.
*   **Tier 2 (Canonical Pillars):** `docs/ARCHITECTURE.md`, `docs/ERRORS.md`, `docs/ROADMAP.md`. Concrete specs. **These docs override CONTEXT.md gist.**
*   **Tier 3 (Sandbox):** `docs/sandbox/`. Transient/Human docs (experiments, learning). **IGNORE BY DEFAULT**.
*   **Tier 4 (The Plan):** `SESSION_PLAN.md`. The instruction set for today.

### Agent Developer Guidelines

1.  **Start of Session:** Read this file first. Consult Tier 2 docs on-demand for deeper context.
2.  **During Work:** Keep `Current Active Task` pointing to the active `SESSION_PLAN.md` step.
3.  **End of Task:** Update `[Dynamic State]` (task pointer + baseline). On plan completion, reset to `[HANDOVER]`.
4.  **Commits:** **Conventional Commits** with crate scope (`feat(archive-core): ...`). Bodies use strict `-` bullet lists.
5.  **Git Protocol:** **NEVER** auto-commit or stage. Always ask Stefan first.

### The Product

**Archive Sniper** is a surgical cloud extraction tool (ZIP/RAR5/RAR4). Focus: Google Drive, Dropbox, OneDrive.

*   **UX Principle:** Simplicity and sanity. TUI-first (Browse, Search, Select, Extract).
*   **Philosophy:** Stateless core, surgical metadata discovery.
*   **Scope:** Extraction only (v1). No creation/compression. See `docs/ROADMAP.md`.

### Crate Manifest
*   **`archive-core`**: Stateless logic, Parsers, Codecs, and `ReadAt`.
*   **`archive`**: State Layer. Caching, Entry Tree, Error Translation.
*   **`archive-cli`**: The TUI product (`ratatui`).
*   **`archive-dev`**: Experimental scratchpad.

### Technical Pillars (The Gist)

#### Mental Model: 5 Layers
```
Layer 4: Products (CLI, Apps, Server)
  |--- Layer 3: Archive (Facade + State + Format Detection)
    |--- Layer 2: Format Parsers (ZIP, RAR)
      |--- Layer 1: Codecs (Deflate, Unrar) & I/O Traits
        |--- Layer 0: Raw bytes (Local, HTTP)
```

#### The Golden Rules
1. **Nothing Leaks:** Products see `Archive`/`Opened`/`PendingArchive`/`ArchiveError`, never core internals.
2. **Shared Codecs:** One implementation for all formats.
3. **Clean Entries:** `ArchiveEntry` has no format-specific offsets or CRCs.
4. **Right Tool:** `ReadAt` uses `read_range` (metadata) and `read_stream` (extraction).
5. **Stateless Core:** Layers 0–2 hold no state. State lives in Layer 3 (`Archive`).
6. **Vocabulary:** `ReadError` (I/O) → `CoreError` (Parsers) → `ArchiveError` (Products).
