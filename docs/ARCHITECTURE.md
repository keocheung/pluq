# pluq — System Architecture

> **Switchboard.** Canonical mental model + boundaries below. Implementation
> detail lives in `architecture/` spokes — pull on demand.
> See also: `CONTEXT.md` (project state), `ERRORS.md` (error architecture).

---

## Mental Model — 5 Layers

```text
┌──────────────────────────────────────────────────────────────────┐
│  Layer 4: Products (binaries)                                     │
│    CLI (pluq)  │ Server/SaaS │ KMP Apps (Android/iOS/Desktop)     │
│    Each product assembles a ReadAt decorator stack and defines    │
│    its own product-level error type.                              │
└──────────────────────┬───────────────────────────────────────────┘
                       │  (composed ReadAt passed down)
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 3.5: Source Resolution (archive-source)                    │
│    Detects URL → constructs the right ReadAt (Local/HTTP/GDrive/  │
│    Dropbox). Returns ResolvedSource enum. See SOURCE.md.          │
└──────────────────────┬───────────────────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 3: Archive (Facade + State + Format Detection)             │
│    Archive::open_reader(label, reader) → Opened                   │
│    Holds ArchiveInner enum (Zip|Rar|Rar4). Owns cache, EntryTree, │
│    ScanInfo. Translates CoreError → ArchiveError.                 │
└──────────────────────┬───────────────────────────────────────────┘
                       │
┌──────────────────────▼───────────────────────────────────────────┐
│  Layer 2: Format Parsers (ArchiveReader trait)                    │
│    zip/ │ rar/ │ rar4/ │ (future: 7z, tar)                        │
│    Public: Vec<ArchiveEntry>. Private: Vec<FormatLocation>.       │
│    Translate format method IDs → shared CompressionMethod enum.   │
└─────────┬────────────────────────┬────────────────────────────────┘
          │                        │
┌─────────▼─────────┐  ┌──────────▼──────────────────┐
│  Layer 1: I/O      │  │  Layer 1: Codecs             │
│    ReadAt trait    │  │    CompressionMethod enum    │
│    LocalReadAt     │  │    build_decoder/encoder     │
│    HttpReadAt      │  │    Deflate, LZMA2, BZip2,    │
│    Multi-volume*   │  │    ZSTD, Store, PPMd         │
│    SFX adjusted*   │  │    Filters: BCJ, Delta       │
│                    │  │    Crypto: AES-CBC primitives│
└────────────────────┘  └──────────────────────────────┘

Layer 0: Raw bytes (disk, network, memory)
```

---

## The Six Golden Rules (non-negotiable)

1. **Nothing leaks across boundaries.** Products see `Archive` / `Opened` /
   `PendingArchive` / `ArchiveError` — never `ZipArchive`, `RarArchive`,
   `flate2`, `LocalReadAt`, or `CoreError`.
2. **Codecs are shared.** Deflate lives once. Every format parser uses the
   same instance.
3. **`ArchiveEntry` has zero format-specific fields.** Offsets, CRCs, folder
   indexes stay private to the format parser.
4. **Right tool for the job.** `ReadAt` exposes `read_range` (bounded
   metadata reads → `Bytes`) and `read_stream` (entry data extraction →
   `AsyncRead`). A 4 GB entry never lives in RAM.
5. **Core is stateless.** Layers 0–2 hold no session state, no caches, no
   auth tokens. Universal state (cache, entry tree) lives in Layer 3.
   Product-specific state (auth, quota, billing) lives in horizontal crates
   composed at Layer 4.
6. **I/O has its own error language.** `ReadAt` returns `ReadError`, not
   `CoreError`. Format parsers convert at the boundary. See `ERRORS.md`.

---

## Crate Manifest & Dependency Direction

```text
archive-core         ← ReadAt, codecs, format parsers (generic over R: ReadAt)
    ▲
    ├─ archive-gdrive       (GoogleDriveReadAt)
    ├─ archive-dropbox      (DropboxReadAt)
    ├─ archive-source       (resolve URL → ResolvedSource enum)
    │       ▲
    ├─ archive (state)      (Archive<R>, EntryTree, ArchiveCache, error xlation)
    │       ▲
    └─ pluq                 (TUI product, calls archive-source + archive)

archive-dev    — experimental scratchpad (no production code depends on it)
```

**Rules:** core depends on nothing project-internal. Provider crates depend
only on core. State layer (`archive`) depends on core. Source layer depends
on core + provider crates. Products depend on whatever they need.

---

## Cross-Platform Constraint [important]

Targets: Android, iOS, Windows, Linux, macOS (potentially WASM/browser).
Platform-specific concerns are isolated to Layer 0. Everything above is
portable Rust. Prefer pure-Rust crates; gate C-backed codecs behind feature
flags. See `architecture/layer0-io.md` for platform table.

---

## Spokes — Read on Demand

Each entry: title + one-line gist + "Read when" trigger.

- **[Layer 0 — I/O Sources](architecture/layer0-io.md)** —
  `ReadAt`/`LocalPath` traits, `LocalReadAt`, `HttpReadAt`, multi-volume,
  SFX, cross-platform impl table.
  *Read when:* adding a new I/O source, debugging HTTP range behavior,
  porting to a new platform.

- **[Layer 1 — Codecs](architecture/layer1-codec.md)** —
  `CompressionMethod`/`FilterMethod` enums, `Decoder`/`Encoder` traits,
  `build_decoder` dispatch, `CodecConfig` memory limits, async/sync
  bridge (`spawn_blocking` + `SyncIoBridge`), encryption primitives.
  *Read when:* adding a codec, changing the decompression pipeline,
  touching the async/sync boundary, debugging dictionary OOM.

- **[Layer 2 — Format Parsers](architecture/layer2-parsers.md)** —
  `ArchiveReader` trait, internal `FormatLocation` arrays, format method
  ID translation, `has_stream` flag, solid-block handling, 7z header
  quirks (encoded/encrypted/BCJ2/external), ZIP filename encoding detection.
  *Read when:* adding a format, parsing a new header variant, debugging
  filename mojibake or solid-block extraction.

- **[Layer 3 — Archive Facade & State](architecture/layer3-archive.md)** —
  `Archive<R>`, `Opened`/`PendingArchive`, `ScanInfo`, `EntryTree`,
  `ArchiveCache`/`ArchiveFingerprint`, format detection.
  *Read when:* changing the open/scan flow, touching cache invalidation,
  altering the entry-tree shape.

- **[Layer 4 — Products & Horizontal Crates](architecture/layer4-products.md)** —
  CLI/Server/KMP product responsibilities, `archive-auth`, `archive-retry`,
  metering, decorator composition pattern.
  *Read when:* wiring a new product, adding a decorator, deciding where
  auth/retry/quota logic lives.

- **[Cross-Cutting Concerns](architecture/cross-cutting.md)** —
  CRC verification policy, progress reporting, multi-volume discovery,
  resource limits (anti-bomb), timestamps, path traversal.
  *Read when:* changing safety defaults, adding progress UI, working on
  multi-volume archives.

- **[Key Types](architecture/types.md)** —
  Full definitions: `ArchiveEntry`, `ResourceLimits`, `RatioLimit`,
  `LimitMode`, `ProgressReporter`, `ArchiveIndex`.
  *Read when:* you need exact field shapes.

- **[Write-Side Notes (post-v1)](architecture/future-write.md)** —
  7z header-at-end constraint, user decision model for archive creation.
  *Read when:* working on archive creation. Otherwise ignore.

---

## Design Principles

1. **Safety first** — resource limits, input validation, path traversal
   protection (Sanitize default / Strict opt-in; never Relaxed).
2. **Correctness over speed** — always verify CRC, fail rather than guess.
3. **Fail early, fail informatively, fail safely** — errors carry context;
   no partial state on failure.
4. **Accept liberal, emit conservative** — tolerate spec deviations when
   reading; produce maximally compatible output when writing.
5. **Defer errors to extraction, not parse time** — let users list entries
   even when some compression methods are unsupported.
