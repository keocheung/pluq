# pluq — Active Roadmap

> **Maintenance rule.** This file shows **only active work**. On task completion: check off `[x]`, move the entry to [`roadmap/COMPLETED.md`](./roadmap/COMPLETED.md), and remove from this file. Post-v1 / "maybe later" ideas live in [`roadmap/FUTURE.md`](./roadmap/FUTURE.md). Keep this file lean so agents and humans reading it see only what's in flight.

## Phase 6: Public Release

> Release plumbing only — no engine work remains (Phases 1–5 are in [`COMPLETED.md`](./roadmap/COMPLETED.md)). Sub-phases are an explicit **dependency chain** so nothing needs retro-fixing (Stefan's standing preference): freeze names/versions → restructure repo → write URL-bound docs & features → wire distribution → audit history → flip public. The crate name `pluq` is locked, so the repo URL (`github.com/<owner>/pluq`) is predictable — URL-bound items carry no placeholder-swap risk.

### 6.3 — OSS docs & scaffolding (mostly complete)
> Core docs + scaffolding landed 2026-05-30 (6.2 restructure + the doc bundle — see [`COMPLETED.md`](./roadmap/COMPLETED.md)). One item remains:

- [ ] **Demo asciicast/GIF** (`vhs` or `asciinema`) — TUI sniping one file from a cloud ZIP. Hero asset for README + Show HN. **Off-chain / parallel:** independent of 6.4–6.6, needs only a running binary + a real cloud archive + a recording session (manual, not headless). Must land **before the 6.7 flip-public**, but gates nothing in the chain — record whenever convenient. Highest-leverage showoff item — the demo is the pitch.




### 6.7 — Launch
> Irreversible / outward-facing. Do only after every gate above is green.
>
> **Distribution = Path A, binary-only (locked).** No crates.io publish. The `archive*` substrate stays internal, consumed **by path** across current and future products (SaaS, KMP) — it is a private foundation, not a third-party library. Distribution is `cargo-dist` GH-release binaries + Homebrew + `curl | sh` (6.5). `cargo install pluq` is *not* offered (would force publishing the whole family). The `pluq` crates.io name stays reserved (parked `0.0.0`). Revisit only on real Rust-user demand for `cargo install` — that, and only that, would reopen the family-publish question.

- [ ] Flip repo public.
- [ ] Announce: r/rust, r/commandline, Show HN.

---

> Completed: [`roadmap/COMPLETED.md`](./roadmap/COMPLETED.md). Future / parking lot: [`roadmap/FUTURE.md`](./roadmap/FUTURE.md).
