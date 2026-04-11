# Archive Sniper — Project Context & State

## [Dynamic State] - AGENTS MUST UPDATE THIS SECTION

* **Current Phase:** Phase 3 — Interactive TUI + Google Drive Provider.
* **Current Active Task:** SESSION_PLAN Step 2 — Speed + ETA during extraction
* **Baseline:** 236 tests pass, 14 ignored. Clippy clean.


---

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### The Knowledge Tiers (How to read this project)

*   **Tier 1 (Switchboard):** `CONTEXT.md` (You are here). Read first for orientation.
*   **Tier 2 (Canonical Pillars):** `docs/ARCHITECTURE.md`, `docs/ERRORS.md`, `docs/ROADMAP.md`. Concrete specs. **These docs override CONTEXT.md gist.**
*   **Tier 3 (Sandbox):** `docs/sandbox/`. Transient/Human docs (experiments, learning). **IGNORE BY DEFAULT**.
*   **Tier 4 (The Plan):** `SESSION_PLAN.md`. The instruction set for today.

### Agent Developer Guidelines

1.  **Start of Session:** Read this file first. Consult Tier 2 docs on-demand for deeper context.
2.  **Session Planning:** Use a "Master-Step" pattern at the project root: `SESSION_PLAN.md` (overview/index) + `SESSION_PLAN_STEP_N.md` (detailed steps in separate files). This naming convention ensures they are automatically ignored by `.gitignore`.
3.  **During Work:** Keep `Current Active Task` pointing to the active `SESSION_PLAN.md` step.
3.  **End of Task:** Update `[Dynamic State]` (task pointer + baseline). On plan completion, reset to `[HANDOVER]`.
4.  **Commits:** **Conventional Commits** with crate scope (`feat(archive-core): ...`). Bodies use strict `-` bullet lists.
5.  **Git Protocol:** **NEVER** auto-commit or stage. Always ask Stefan first.

### The Product

**Archive Sniper** is a surgical cloud extraction tool (ZIP/RAR5/RAR4). Focus: Google Drive, Dropbox, OneDrive.

*   **UX Principle:** Simplicity and sanity. TUI-first (Browse, Search, Select, Extract).
*   **Philosophy:** Stateless core, surgical metadata discovery.
*   **Scope:** Extraction only (v1). No creation/compression. See `docs/ROADMAP.md`.

### Crate Manifest
*   **`archive-core`**: Stateless logic, Parsers, Codecs, `ReadAt`, `LocalPath`.
*   **`archive-source`**: Source resolution — URL detection, credential discovery, `ResolvedSource`.
*   **`archive`**: State Layer. Caching, Entry Tree, Error Translation. Generic over `R: ReadAt + LocalPath`.
*   **`archive-cli`**: The TUI product (`ratatui`).
*   **`archive-gdrive`**: Google Drive `ReadAt` implementation.
*   **`archive-dev`**: Experimental scratchpad.

### Technical Pillars (The Gist)

#### Mental Model: 5 Layers
```
Layer 4: Products (CLI, Apps, Server)
  |--- Layer 3.5: Source Resolution (archive-source)
    |--- Layer 3: Archive (Facade + State + Format Detection)
      |--- Layer 2: Format Parsers (ZIP, RAR)
        |--- Layer 1: Codecs (Deflate, Unrar) & I/O Traits
          |--- Layer 0: Raw bytes (Local, HTTP, GDrive)
```

#### The Golden Rules
1. **Nothing Leaks:** Products see `Archive`/`Opened`/`PendingArchive`/`ArchiveError`, never core internals.
2. **Shared Codecs:** One implementation for all formats.
3. **Clean Entries:** `ArchiveEntry` has no format-specific offsets or CRCs.
4. **Right Tool:** `ReadAt` uses `read_range` (metadata) and `read_stream` (extraction).
5. **Stateless Core:** Layers 0–2 hold no state. State lives in Layer 3 (`Archive`).
6. **Vocabulary:** `ReadError` (I/O) → `CoreError` (Parsers) → `ArchiveError` (Products).
