# pluq — Active Roadmap

> **Maintenance rule.** This file shows **only active work**. On task completion: check off `[x]`, move the entry to [`roadmap/COMPLETED.md`](./roadmap/COMPLETED.md), and remove from this file. Post-v1 / "maybe later" ideas live in [`roadmap/FUTURE.md`](./roadmap/FUTURE.md). Keep this file lean so agents and humans reading it see only what's in flight.

## Phase 6: Public Release

> Release plumbing only — no engine work remains (Phases 1–5 are in [`COMPLETED.md`](./roadmap/COMPLETED.md)). Sub-phases are an explicit **dependency chain** so nothing needs retro-fixing (Stefan's standing preference): freeze names/versions → restructure repo → write URL-bound docs & features → wire distribution → audit history → flip public. The crate name `pluq` is locked, so the repo URL (`github.com/<owner>/pluq`) is predictable — URL-bound items carry no placeholder-swap risk.

### 6.3 — OSS docs & scaffolding (mostly complete)
> Core docs + scaffolding landed 2026-05-30 (6.2 restructure + the doc bundle — see [`COMPLETED.md`](./roadmap/COMPLETED.md)). One item remains:

- [ ] **Demo asciicast/GIF** (`vhs` or `asciinema`) — TUI sniping one file from a cloud ZIP. Hero asset for README + Show HN. **Off-chain / parallel:** independent of 6.4–6.6, needs only a running binary + a real cloud archive + a recording session (manual, not headless). Must land **before the 6.7 flip-public**, but gates nothing in the chain — record whenever convenient. Highest-leverage showoff item — the demo is the pitch.

### 6.4 — URL-bound product features
> Depend on the final repo / Discussions URLs. Placed after 6.3 settles those URLs so they're written once. Spec: V1_LOCKED Decisions 14 + 5. **Day-one priority** (with 6.5 install): small early userbase → each report is precious → rich + easy + safe bug capture from launch pays back directly in fix speed. New deps: `webbrowser`, `human-panic` (2.x).

- [ ] **`pluq feedback`** — opens browser (`webbrowser` crate) to GH Issues / Discussions. Sub-options: `--bug` (new-issue bug template), `--idea` (Ideas discussion), `--say-hi` (Discussions home: `https://github.com/StefanDekanski/pluq/discussions`).
- [ ] **`pluq bug-report`** — collects a **rich, redacted** diagnostic → `pluq-bugreport-<ts>.txt`. Prints path + prefilled GH issue URL. **Never auto-uploads.** Payload:
  - pluq version + **build git-sha + build date + target triple**
  - OS + OS-version + arch
  - configured backends (gdrive / dropbox / local) — **names only, no creds**
  - recent **log tail** through `redact.rs::scrub`
  - last **error chain** (4-tier model — gold for diagnosis)
  - TUI/term info (`$TERM`, terminal size)
  - which `PLUQ_*` env vars are **set** — **names only, never values** (they hold BYO creds)
  - last crash report (from panic-hook below) if present
- [ ] **Panic-hook** (`human-panic` 2.x) — auto-write crash report (TOML, temp dir) on unexpected panic; print path + point user at `pluq bug-report` / issue URL via custom `Metadata`. Catches the crashes users wouldn't bother reporting. Release-build + panic-only — complements on-demand `bug-report`.
- [ ] **Privacy controls** (the safety gate for the three above):
  - All collected text through `redact.rs::scrub` — **plus** home paths `/Users/<name>` → `~`, archive filenames stripped/`<file>` unless needed, runtime paths in panic msg scrubbed.
  - On write, print what the file contains + "nothing is sent anywhere."
  - **`PRIVACY.md`** (+ README section): "pluq collects zero telemetry; crash/bug reports are local files, never transmitted without your explicit action."
  - **Bug issue-template warning:** "this file may contain your username / file paths — review and remove anything sensitive before attaching (public issue = public forever)."
> **Why local-only + no-upload is the whole privacy strategy.** You collect nothing at runtime → no GDPR/CCPA consent-machinery, no privacy-policy obligation, no data-retention duty. The "collection" moment is the user choosing to attach the file (user-initiated disclosure). Inform + give control (inspectable plaintext) = sufficient; no consent checkbox needed. Auto-upload would flip all of that on — so it stays banned.

### 6.5 — Distribution pipeline
> **Day-one priority** (with 6.4 bug-capture): easy install is the other thing that must work at launch. `cargo-dist` compiles the `pluq` binary per platform **and** generates the install front-ends. The `curl | sh` one-liner is the **hero path** (rustup / Claude-Code style): detects OS+arch, downloads the prebuilt binary, drops it on `PATH` → `pluq` works immediately. A compiled CLI *is* a binary; every method below delivers the **same** self-contained executable, no runtime required — that's the whole appeal.
>
> **Installers locked: `shell` + `powershell` + `homebrew` + `msi`** (all cargo-dist-native, all auto-generated per release). Scoop is *not* a cargo-dist installer (manual bucket repo + per-release hash bumps) → moved to [`FUTURE.md`](./roadmap/FUTURE.md), signal-gated. Windows is fully covered by `powershell` (irm|iex hero) + `msi`.

- [ ] **`cargo-dist`** setup + GH Actions release workflow (compiles binaries + generates installers; also drives the launch release in 6.7). Config: `installers = ["shell", "powershell", "homebrew", "msi"]`.
- [ ] **One-liner install scripts** — `curl ... | sh` (unix/mac) + `irm ... | iex` PowerShell twin (Windows). Hero UX on every OS; both auto-generated by `cargo-dist`, both dodge Gatekeeper/SmartScreen (curl/irm don't set the quarantine / mark-of-the-web flag).
- [ ] **Homebrew tap** (`brew install`) — smoothest mac path (sidesteps Gatekeeper friction the script can't). cargo-dist auto-generates + auto-publishes the formula per release; one-time setup = create empty **`StefanDekanski/homebrew-tap`** repo + config (`tap = "StefanDekanski/homebrew-tap"`, `publish-jobs = ["homebrew"]`). Near-free after.
- [ ] **MSI installer** (Windows) — cargo-dist-native, auto-generated. Native double-click install for GUI-leaning Windows users.
- [ ] GitHub Releases prebuilt binaries (mac x64/arm64, linux x64/arm64, windows x64) — the artifacts the installers download. Also the **fallback** for curl-averse / offline / CI-automation users. Not the recommended human path (manual place-on-`PATH`); just not hidden.

### 6.6 — Pre-publish security audit
> Audit-only gate. **History rewrite dropped 2026-05-31** — see rationale below. No `git filter-repo`, no force-push, no SHA churn. This is the only pre-public security step left.

- [ ] **Verify no leaked credentials / share-links in committed history.** `git log --all -p` grep for real token formats (`ghp_`/`ya29.`/`AKIA`/`sk-`/private keys), real cloud share-links, home paths. If a *real* secret surfaces → targeted `git filter-repo --replace-text` of that value only (not the whole-file `--invert-paths` op). *(First pass run 2026-05-31: clean — only benign hits were keyring service-name constants `client_secret = archive_{dropbox,gdrive}` and fake doc placeholders `dropbox.com/s/abc`. Re-run once 6.3–6.5 land their final commits.)*

> **Why the `--invert-paths` file-scrub was dropped (2026-05-31).** Original plan (V1_LOCKED Decision 11) was to strip `CLAUDE.md` / `GEMINI.md` / `CONTEXT.md` from all history. Verified: those are the *only* personal files ever tracked (5 / 5 / 133 commits; `.workspace`, `sandbox`, `superpowers`, `dev_links.json`, logs, `temp` = 0 commits, gitignored from day one → never in history). Their HEAD versions are already public-safe (pointers + architecture doc). What the scrub would erase = old agent-workflow content (session-plan ritual, dynamic-state) — mildly personal, **not** sensitive. No secrets, no private data. Cost (rewrite ~140 SHAs + force-push + must-be-last discipline + error risk) outweighs a purely cosmetic gain. Agent-driven dev in repo history is normal. Reopen only if a real secret is found by the audit above, or a deliberate privacy-of-method call changes.

### 6.7 — Launch
> Irreversible / outward-facing. Do only after every gate above is green.
>
> **Distribution = Path A, binary-only (locked).** No crates.io publish. The `archive*` substrate stays internal, consumed **by path** across current and future products (SaaS, KMP) — it is a private foundation, not a third-party library. Distribution is `cargo-dist` GH-release binaries + Homebrew + `curl | sh` (6.5). `cargo install pluq` is *not* offered (would force publishing the whole family). The `pluq` crates.io name stays reserved (parked `0.0.0`). Revisit only on real Rust-user demand for `cargo install` — that, and only that, would reopen the family-publish question.

- [ ] Flip repo public.
- [ ] Enable GH Discussions (categories: Q&A, Ideas, Show & Tell) — targets for `pluq feedback`.
- [ ] Set GitHub repo description (≤150 chars) + topics (`rust`, `cli`, `tui`, `archive`, `zip`, `rar`, `google-drive`, `dropbox`, `extraction`).
- [ ] Tag `v0.1.0` → triggers `cargo-dist` release (GH Release binaries).
- [ ] Announce: r/rust, r/commandline, Show HN.

---

> Completed: [`roadmap/COMPLETED.md`](./roadmap/COMPLETED.md). Future / parking lot: [`roadmap/FUTURE.md`](./roadmap/FUTURE.md).
