# Layer 3 — Archive Facade & State

> Spoke of `docs/ARCHITECTURE.md`. Read the switchboard first for the
> 5-layer mental model and 6 Golden Rules.

Single entry point for products. Holds all universal state: cache, entry
tree, scan lifecycle. Translates `CoreError` → `ArchiveError`.

---

## Entry Point

`Archive::open_reader(label, reader)` receives a pre-constructed
`R: ReadAt + LocalPath` reader and auto-detects the archive format (ZIP,
RAR5, RAR4), returning `Opened::Ready(Archive)` or
`Opened::NeedsScan(PendingArchive)`. Source resolution (local vs HTTP vs
cloud provider) is handled upstream by `archive-source` (see
`docs/SOURCE.md`). Holds the format-specific parser directly via an
internal `ArchiveInner` enum. Translates `CoreError` → `ArchiveError`.
Tunnels `ReadError::Transport` blindly as `ArchiveError::Transport`.

---

## What Layer 3 IS

Format detection, error translation, universal state (cache, entry tree,
scan lifecycle). All products (CLI, server, KMP apps) get this for free.

## What Layer 3 is NOT

Not the place for auth, billing, quotas, OAuth, or retry policies. Those
are product-specific concerns that live in horizontal crates, assembled
at the product level (Layer 4). See `layer4-products.md`.

---

## Responsibilities

- Receives a pre-constructed `R: ReadAt + LocalPath` reader from
  `archive-source` (or directly from tests).
- Auto-detects format via magic bytes and opens the correct parser.
- Returns `Opened::Ready` (metadata available) or `Opened::NeedsScan`
  (deferred scan required, e.g. RAR over HTTP without QO).
- Manages `ArchiveCache` (on-disk cross-session metadata cache with
  invalidation).
- Builds `EntryTree` (virtual folder structure derived from flat entry
  names).
- Translates `CoreError` → `ArchiveError` for core errors it understands.
- Tunnels `ReadError::Transport` (inside `CoreError::Io`) blindly as
  `ArchiveError::Transport` — does not inspect or downcast.

---

## Key Types

- **`Archive<R>`** — product-facing handle to an open archive (holds
  `ArchiveInner<R>` enum: `Zip(ZipArchive<R>)` | `Rar(RarArchive<R>)` |
  `Rar4(Rar4Archive<R>)`); generic over `R: ReadAt + LocalPath`.
- **`Opened`** — result of `Archive::open()`: `Ready(Archive)` or
  `NeedsScan(PendingArchive)`.
- **`PendingArchive`** — archive needing a metadata scan; call
  `scan_info()` for cost, then `scan()` to produce an `Archive`.
- **`ScanInfo`** — `Ready` or `Required { estimated_bytes, reason }` —
  tells callers whether and how expensive the scan is.
- **`ArchiveError`** — error type for product consumption (see
  `docs/errors/archive-error.md`).
- **`EntryTree` / `TreeNode`** — virtual folder structure.
- **`ArchiveCache` / `ArchiveFingerprint`** — cross-session metadata +
  location cache.

---

## Persistent Metadata Caching

`archive-cache` owns disk persistence and source fingerprint validation. It exposes
`CachedArchive` to products, hiding the composed metadata reader type.

- **ZIP:** persist original byte ranges read during opening, including format
  detection, the EOCD search window, ZIP64 records when needed, and the Central
  Directory. Store offsets with bytes; do not persist decoded entries or locations.
- **RAR4/RAR5:** retain parsed entries and format-specific scan results, since their
  sequential scanning paths differ from ZIP metadata discovery.

On a ZIP cache hit, a `ReadAt` wrapper in `archive-cache` replays those ranges through
`Archive::open_reader_uncached`. The current parser, filename detector and resource
limits run again without downloading the cached metadata. Extraction still reads
from the original source. Streams are never recorded, and recording ends before
extraction or a pending RAR scan. Capture has a byte budget; an oversized capture
is discarded instead of persisting an incomplete snapshot.

Source validation still runs before replay (ETag, Last-Modified, mtime, or size).
Malformed or incomplete cached metadata falls back to a fresh source parse. Cache
schema changes invalidate old files; changes to ZIP filename decoding alone no
longer require invalidation. The first open after migration reads metadata again.

The EOCD search window may incidentally include adjacent compressed data, but no
extraction stream or whole archive is deliberately cached.

---

## Crate Placement

`archive` depends on `archive-core` and handles the facade and format state.
`archive-cache` composes source resolution, disk persistence and metadata replay
above the facade. Core parsers do not own persistence or cache session state.

See `docs/errors/archive-error.md` for the full error translation flow
and tunnel mechanism.
