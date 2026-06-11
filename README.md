# pluq

[![CI](https://github.com/stefandekanski/pluq/actions/workflows/ci.yml/badge.svg)](https://github.com/stefandekanski/pluq/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](https://www.rust-lang.org)

**Snipe one file out of a huge cloud archive — without downloading the whole thing.**

pluq is a surgical archive extraction tool. Point it at a ZIP or RAR — local, a direct URL,
Google Drive, or Dropbox — browse it in a fast terminal UI, and pull out exactly the files you
want. It reads only the bytes it needs: the central directory to list, and just the ranges for
the entries you extract.

![pluq TUI: paste a URL to Electron's 1.86 GB debug-symbol zip, browse the listing, filter to one file, and extract it in seconds](docs/demo/hero.gif)

*One file out of Electron's 1.86 GB symbol bundle: paste the URL, filter, extract — ~17 MB fetched, not 1.86 GB.*

## Features

- **Surgical extraction** — fetches metadata + only the selected entries' byte ranges, not the
  whole archive.
- **Formats** — ZIP (Deflate, Store, BZip2, LZMA, Zstandard), RAR5, RAR4. Extraction only.
  See [Format support](#format-support) for how surgical extraction differs per format.
- **Sources** — local files, HTTP(S) direct URLs, Google Drive, Dropbox. *(OneDrive: planned.)* Cloud files can be added by pressing `c` in the TUI to pick from Google Drive (Picker) or Dropbox (Chooser).
- **TUI-first** — browse, search, multi-select, and extract from a minimalist terminal UI.
  Also scriptable via `pluq get`/`list`/`test`.
- **Private files** — OAuth for Google Drive and Dropbox (`pluq auth add`). In the TUI, private Google Drive files are picker-only (using the `drive.file` scope).
- **Safe by default** — anti-bomb resource limits, path-traversal & symlink-escape guards,
  LZMA dictionary cap.

## Install

The one-liner is the recommended path on every OS (no Rust toolchain required):

**macOS / Linux**
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/stefandekanski/pluq/releases/latest/download/pluq-installer.sh | sh
```

**Windows (PowerShell)**
```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/stefandekanski/pluq/releases/latest/download/pluq-installer.ps1 | iex"
```

**Homebrew (macOS)**
```sh
brew install stefandekanski/tap/pluq
```

Prebuilt binaries for every platform are on the [Releases](https://github.com/stefandekanski/pluq/releases)
page (macOS x64/arm64, Linux x64/arm64, Windows x64) — see [docs/INSTALL.md](docs/INSTALL.md)
for manual install, Scoop, and the macOS Gatekeeper note.

> pluq is distributed as a prebuilt binary. `cargo install pluq` is **not** offered.

### Uninstall

```sh
rm ~/.cargo/bin/pluq      # script install (macOS / Linux)
brew uninstall pluq       # Homebrew
```

```powershell
Remove-Item $env:USERPROFILE\.cargo\bin\pluq.exe   # script install (Windows)
```

MSI installs uninstall via Windows **Settings → Apps**. To also remove cached data and saved
OAuth tokens, see [docs/INSTALL.md](docs/INSTALL.md#uninstall).

## Quickstart

```sh
pluq                                   # launch the TUI home screen
pluq path/to/archive.zip               # browse a local archive in the TUI
pluq https://example.com/big.rar       # browse a remote archive

pluq list  archive.zip                 # print the file list
pluq get   archive.zip "*.jpg"         # extract matching files
pluq get   archive.zip -o out/         # extract everything into out/
pluq test  archive.zip                 # CRC-verify every entry
```

See [docs/USAGE.md](docs/USAGE.md) for the full command reference and TUI keys, and
[docs/AUTH.md](docs/AUTH.md) for accessing private Google Drive / Dropbox files (including details on picking files into Bookmarks via the `c` keybind).

## Format support

Surgical extraction needs a **central index** pluq can read up front: list the archive, then
fetch only the byte ranges for the entries you pick. Every format gets there differently — and
for remote archives, that difference is what you feel as speed.

| Archive | Listing | Surgical extract |
|---|---|---|
| **ZIP** | Instant — reads the central directory at the tail | ✅ |
| **RAR with Quick Open index** | Instant — reads the QO index, just like ZIP | ✅ |
| **RAR without Quick Open** | One-time **scan** of the archive's header chain | ✅ (after scan) |
| **Solid RAR** | Lists | ❌ not supported |

- **ZIP** keeps its table of contents (the *central directory*) at the end of the file. pluq
  reads that, lists instantly, and pulls only the ranges you asked for.
- **RAR with Quick Open** carries an equivalent index (added at creation time, e.g.
  `rar a -qo+ archive.rar files`). pluq reads it directly — listing is instant, same as ZIP.
- **RAR without Quick Open** has no such index, so pluq has to **scan**: walk the file's header
  chain to map every entry before it can list. On a remote archive that's a visible one-time
  step; after it completes, extraction is fully surgical. Local files scan instantly.
- **Solid RAR** compresses all files as one continuous stream — each entry depends on the bytes
  of the ones before it, so there's no way to decompress a single file without the whole chain.
  That defeats surgical extraction: pluq lists a solid archive but refuses to extract from it.

![pluq TUI: a RAR without Quick Open is scanned once (visible progress bar), then 517 label files are glob-filtered and extracted surgically](docs/demo/rar-scan.gif)

*A 1034-file RAR with no Quick Open index: one scan of the 36 MB header chain, then `*.txt` filters
to the 517 label files — extracted for ~46 KB fetched; the ~35 MB of images never leave the server.*

> **Tip:** if you control how the RAR is made, add Quick Open info (`-qo+`) and keep it
> non-solid (`-s-`) for the fastest, most surgical remote experience.

## Project status

pluq is **pre-1.0** and **solo-maintained**. It does one thing — extraction — and aims to do it
well. Fixes are best-effort; archive creation is explicitly out of scope. Bug reports and ideas
are welcome via [Issues](https://github.com/stefandekanski/pluq/issues) and
[Discussions](https://github.com/stefandekanski/pluq/discussions).

## Privacy

pluq collects **zero telemetry** — nothing is ever transmitted. `pluq bug-report` and crash
reports are **local files** you choose whether to share. See [PRIVACY.md](PRIVACY.md).

## Acknowledgements

Built by Stefan Dekanski, with AI pair-programming — Claude Code (Anthropic) and Google Gemini.

## License

Dual-licensed under either of **MIT** ([LICENSE-MIT](LICENSE-MIT)) or **Apache-2.0**
([LICENSE-APACHE](LICENSE-APACHE)) at your option. RAR support uses the UnRAR library under its
own (extraction-only) terms — see [LICENSE-THIRD-PARTY.md](LICENSE-THIRD-PARTY.md).
