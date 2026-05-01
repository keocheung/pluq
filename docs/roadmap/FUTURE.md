# pluq — Post-v1 & Future Ideas

> Parking lot. Items here are **not committed** and **not on the active roadmap**. Promote to `ROADMAP.md` only after a real demand signal (issue volume, repeat asks, concrete use case).

## Post-v1 (v1.x candidates)

### Provider expansion
- [ ] **OneDrive** — `archive-onedrive` crate with OAuth2.

### Format expansion (surgical-friendly)
- [ ] **TAR / TAR.GZ / TAR.ZSTD** — per-entry headers fit surgical model; deflate + zstd codecs already in workspace.
- [ ] **Multi-volume RAR** — `part1.rar` / `part2.rar` discovery and stitched ReadAt.

### CLI / UX polish
- [ ] **Stream-extract to stdout** — `pluq get path/in/archive.pdf -o -` → pipe into `pdftotext`, `less`, etc.
- [ ] **Source-aware default folder naming** — use real GDrive / cloud filename instead of URL-guessing.
- [ ] **TUI `[solid]` marker** on entry list + scan-complete fully-solid hint.
- [ ] **`pluq doctor`** health-check subcommand (token store status, cache dir writable, network reachable, FFI deps OK).
- [ ] **Anonymous `--help` survey link** — only if structured-feedback need emerges post-launch.

### Performance
- [ ] **Parallel extraction** for ZIP / non-solid RAR.

### Cache
- [ ] **Advanced cache invalidation** — content hashing, detect modifications between opens.
- [ ] **Configurable cache location** — CLI flag or config file.
- [ ] **Cache size limits / LRU eviction.**

### I/O robustness
- [ ] **Full Range support detection** with retry / fallback and user-facing guidance.

### Observability
- [ ] **Telemetry** — only with a specific question to answer (no data hoarding). Opt-in. Skip until concrete need.

### Engineering hygiene
- [ ] **Error-system typed enum refactor** — replace `String`-payload sub-causes with typed enums (`UnsupportedKind::SolidRar | LzmaDictTooLarge { ... }`, `SecurityKind::*`). Touch-everything change.
- [ ] **Fuzzing harness** (`cargo-fuzz`) — ZIP central directory parser, RAR5 vint parser. Corpus + CI integration.
- [ ] **Decompression-bomb test corpus** — sample malformed / zip-bomb / rar-bomb fixtures; verify `ResourceLimits` guards trip without panic.

---

## Future Ideas (long-term, possibly never)

- **Write Support** — archive creation (`pluq pack`). ZIP first. TUI file browser for selecting files.
- **Format expansion (read-only, deeper future)** — ISO read; 7z **only on real demand signal**. Solid-default UX cost is real: most 7z archives in the wild use solid mode → surgical cloud extraction defeated → would download whole archive anyway. High implementation cost (codec graph, BCJ2, encoded headers, AES variant) for low cloud-extract value.
- **SaaS Server** — HTTP API for web-based sniping. Metered / freemium. See `architecture/layer4-products.md`.
- **KMP Apps** — Android / iOS / Desktop via Compose Multiplatform + UniFFI. See `architecture/layer4-products.md`.
- **Cloud Archiving (Vault)** — stream files from cloud → compress → stream archive back.
- **Plugin Hooks** — external tool integration (fzf, yazi) at search / select / browse decision points.
