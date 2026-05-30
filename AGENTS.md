# AGENTS.md — Working Conventions

Vendor-neutral guide for any agent or human contributor working on **pluq**. Tool-specific
notes (if any) live in redirect stubs (`CLAUDE.md`, `GEMINI.md`) and in gitignored tool dirs
(`.claude/`, `.gemini/`).

## Orientation — read order

1. **`CONTEXT.md`** first — the architecture switchboard (product, 5-layer model, 6 Golden
   Rules, crate manifest). It is the Tier-1 map.
2. **Tier-2 switchboards, on demand:** `docs/ARCHITECTURE.md`, `docs/ERRORS.md`,
   `docs/ROADMAP.md`, `docs/SOURCE.md`. These hold the load-bearing model and override the
   `CONTEXT.md` gist.
3. **Tier-2.5 spokes, individually:** `docs/architecture/*.md`, `docs/errors/*.md` — pull one
   only when its "Read when" hint matches the work. Do not bulk-read the directory.

> **Anti-spaghetti rule.** The 5-layer model, 6 Golden Rules, crate dependency direction, and
> 4-tier error model are non-negotiable. If a spoke seems to contradict a switchboard, the
> switchboard wins — flag it.

## Session flow

- **Start:** read `CONTEXT.md`; consult Tier-2 switchboards on demand.
- **Personal / transient / working files** (scratch, notes, any local working plan) live under
  **`.workspace/`**, which is gitignored — nothing there is part of the repo. If an active
  working plan exists, it lives in `.workspace/`; read it before starting.
- No committed doc tracks "where we are" — live state stays local to `.workspace/`.

## End of task

If you complete a `docs/ROADMAP.md` item: check it off `[x]`, move the entry to
`docs/roadmap/COMPLETED.md`, and remove it from `ROADMAP.md`. There is no status section to update.

## Commits

- **Conventional Commits** with crate scope: `feat(archive-core): …`, `fix(pluq): …`,
  `docs(repo): …`, `ci: …`, `build(workspace): …`.
- Bodies use strict `-` bullet lists (the "why", not a diff restatement).

## Git protocol

- **Never auto-commit or stage.** Always ask the maintainer first.
- Branch off `main`; open a PR. The maintainer (Stefan) reviews all changes.

## Code conventions

- Clean, idiomatic **async Rust**.
- Honor the layer boundaries and the 6 Golden Rules in `CONTEXT.md` — products never see core
  internals; the core stays stateless; one codec implementation serves all formats.
