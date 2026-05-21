# pluq — Project Context

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### The Knowledge Tiers (How to read this project)

*   **Tier 1 (Switchboard):** `CONTEXT.md` (You are here). Read first for orientation.
*   **Tier 2 (Canonical Switchboards):** `docs/ARCHITECTURE.md`, `docs/ERRORS.md`, `docs/ROADMAP.md`, `docs/SOURCE.md`. **Always-load.** Hold the load-bearing mental model: 5 layers, 6 Golden Rules, 4-tier error model, dependency direction. **These override the CONTEXT.md gist.**
*   **Tier 2.5 (On-Demand Spokes):** `docs/architecture/*.md`, `docs/errors/*.md`. Implementation detail behind each switchboard section. Pull individually using the "Read when" hints in the switchboard's spoke index. **Do not bulk-read the whole directory.**
*   **Tier 3 (Sandbox):** `docs/sandbox/`. Transient/Human docs (experiments, learning). **IGNORE BY DEFAULT**.
*   **Tier 4 (The Plan):** `SESSION_PLAN.md`. The instruction set for today.

> **Anti-spaghetti rule.** The 5-layer model, 6 Golden Rules, crate
> dependency direction, and 4-tier error model are non-negotiable. Spokes
> are implementation detail — they explain *how*, not *whether*. If a
> spoke seems to contradict a switchboard rule, the switchboard wins;
> flag it for Stefan.

### Agent Developer Guidelines

1.  **Start of Session:** Read this file first. If a `SESSION_PLAN.md` exists at the project root, that is the active work — read it; no plan file means no active task. Consult Tier 2 switchboards on-demand. Pull a spoke only when its "Read when" hint matches the work.
2.  **Session Planning:** Use a "Master-Step" pattern at the project root: `SESSION_PLAN.md` (overview/index) + `SESSION_PLAN_STEP_N.md` (detailed steps in separate files). This naming convention ensures they are automatically ignored by `.gitignore`. These plan files are the project's only live-state tracker — no committed doc records "where we are".
3.  **End of Task:** If completing a roadmap task, check it off `[x]` and move it from `ROADMAP.md` to `docs/roadmap/COMPLETED.md`. There is no status section to update.
4.  **Commits:** **Conventional Commits** with crate scope (`feat(archive-core): ...`). Bodies use strict `-` bullet lists.
5.  **Git Protocol:** **NEVER** auto-commit or stage. Always ask Stefan first.

### The Product

**pluq** is a surgical cloud extraction tool (ZIP/RAR5/RAR4). Focus: Google Drive, Dropbox, OneDrive.

*   **UX Principle:** Simplicity and sanity. TUI-first (Browse, Search, Select, Extract).
*   **Philosophy:** Stateless core, surgical metadata discovery.
*   **Scope:** Extraction only (v1). No creation/compression. See `docs/ROADMAP.md`.

### Crate Manifest
*   **`archive-core`**: Stateless logic, Parsers, Codecs, `ReadAt`, `LocalPath`.
*   **`archive-source`**: Source resolution — URL detection, credential discovery, `ResolvedSource`.
*   **`archive`**: State Layer. Caching, Entry Tree, Error Translation. Generic over `R: ReadAt + LocalPath`.
*   **`pluq`**: The TUI product binary (`ratatui`). Published to crates.io as `pluq`.
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
