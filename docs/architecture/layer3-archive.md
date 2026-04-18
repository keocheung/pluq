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

## Caching (`ZipIndex` Pattern)

**Cache the metadata (entry list), not the data (file contents).**

```rust
pub struct ArchiveIndex {
    pub source: String,
    pub source_size: u64,
    pub entries: Vec<ArchiveEntry>,
    pub created_at: u64,
    // Staleness detection
    pub etag: Option<String>,        // HTTP ETag header
    pub last_modified: Option<String>, // HTTP Last-Modified header
}
```

On load: compare `source_size` against current archive size (one HEAD
request for HTTP, one stat for local). If size changed → cache stale →
re-scan. For HTTP: also check ETag/Last-Modified.

---

## Crate Placement

`archive`, depends on `archive-core`. Pure cross-platform Rust. No
dependency on any horizontal crate (auth, retry, metering). Platform-
specific concerns (cache storage path) are injected via traits.

See `docs/errors/archive-error.md` for the full error translation flow
and tunnel mechanism.
