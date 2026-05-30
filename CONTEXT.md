# pluq — Project Context

## [Static Context] - DO NOT MODIFY WITHOUT PERMISSION

### The Knowledge Tiers (How to read this project)

*   **Tier 1 (Switchboard):** `CONTEXT.md` (You are here). Read first for orientation.
*   **Tier 2 (Canonical Switchboards):** `docs/ARCHITECTURE.md`, `docs/ERRORS.md`, `docs/ROADMAP.md`, `docs/SOURCE.md`. **Always-load.** Hold the load-bearing mental model: 5 layers, 6 Golden Rules, 4-tier error model, dependency direction. **These override the CONTEXT.md gist.**
*   **Tier 2.5 (On-Demand Spokes):** `docs/architecture/*.md`, `docs/errors/*.md`. Implementation detail behind each switchboard section. Pull individually using the "Read when" hints in the switchboard's spoke index. **Do not bulk-read the whole directory.**
*   **Tier 3 (Sandbox):** `.workspace/sandbox/` (gitignored). Transient/Human docs (experiments, learning). **IGNORE BY DEFAULT**.
*   **Tier 4 (The Plan):** the active working plan in `.workspace/` (gitignored), if any. The instruction set for the current task.

> **Anti-spaghetti rule.** The 5-layer model, 6 Golden Rules, crate
> dependency direction, and 4-tier error model are non-negotiable. Spokes
> are implementation detail — they explain *how*, not *whether*. If a
> spoke seems to contradict a switchboard rule, the switchboard wins;
> flag it for Stefan.

### Working conventions

Session flow, commit format, and git protocol live in **`AGENTS.md`** (the working agreement).
This file is the *architecture* switchboard only.

### The Product

**pluq** is a surgical cloud extraction tool (ZIP/RAR5/RAR4). Focus: Google Drive, Dropbox, OneDrive.

*   **UX Principle:** Simplicity and sanity. TUI-first (Browse, Search, Select, Extract).
*   **Philosophy:** Stateless core, surgical metadata discovery.
*   **Scope:** Extraction only (v1). No creation/compression. See `docs/ROADMAP.md`.

### Crate Manifest
*   **`archive-core`**: Stateless logic, Parsers, Codecs, `ReadAt`, `LocalPath`.
*   **`archive-source`**: Source resolution — URL detection, credential discovery, `ResolvedSource`.
*   **`archive`**: State Layer. Caching, Entry Tree, Error Translation. Generic over `R: ReadAt + LocalPath`.
*   **`pluq`**: The TUI product binary (`ratatui`). Distributed as a prebuilt binary (GH releases, Homebrew, install script); crates.io name reserved but **not** published. See `docs/ROADMAP.md` Phase 6.
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
