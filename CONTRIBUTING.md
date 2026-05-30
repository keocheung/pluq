# Contributing to pluq

Thanks for your interest. pluq is solo-maintained and pre-1.0 — contributions are welcome but
review may be slow. Please open an issue or a Discussion before large changes.

## Working agreement

The full conventions live in **`AGENTS.md`** (session flow, commits, git protocol) and
**`CONTEXT.md`** (architecture). Read both first. The highlights:

## Architecture rules (non-negotiable)

pluq is built in **5 layers** with a strict dependency direction (see `CONTEXT.md` and
`docs/ARCHITECTURE.md`). When you touch the engine, honor the **6 Golden Rules**:

1. **Nothing leaks** — products see `Archive`/`Opened`/`PendingArchive`/`ArchiveError`, never core internals.
2. **Shared codecs** — one implementation per codec, across all formats.
3. **Clean entries** — `ArchiveEntry` carries no format-specific offsets or CRCs.
4. **Right tool** — `ReadAt` uses `read_range` (metadata) and `read_stream` (extraction).
5. **Stateless core** — layers 0–2 hold no state; state lives in layer 3 (`Archive`).
6. **Vocabulary** — `ReadError` (I/O) → `CoreError` (parsers) → `ArchiveError` (products).

A change that crosses a layer boundary the wrong way will be sent back.

## Dev setup

```bash
git clone https://github.com/StefanDekanski/pluq
cd pluq
cargo build
cargo test
```

Before opening a PR, the same gates CI runs must pass locally:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo doc --no-deps
```

**MSRV is Rust 1.88** — do not use newer-than-1.88 language/stdlib features.

## Commits & PRs

- **Conventional Commits** with a crate scope: `feat(archive-core): …`, `fix(pluq): …`,
  `docs: …`, `ci: …`. Bodies use `-` bullets explaining the *why*.
- Branch off `main`, keep PRs focused, fill in the PR template.
- The CHANGELOG is generated from commit messages by `git-cliff` — clear messages matter.

## Personal / scratch files

Anything not meant for the repo (local notes, scratch archives) goes in `.workspace/`, which is
gitignored. Never commit credentials, share-links, or real private archives.

## License

By contributing you agree your work is dual-licensed under **MIT OR Apache-2.0**.
