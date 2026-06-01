# pluq

[![CI](https://github.com/stefandekanski/pluq/actions/workflows/ci.yml/badge.svg)](https://github.com/stefandekanski/pluq/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](https://www.rust-lang.org)

**Snipe one file out of a huge cloud archive — without downloading the whole thing.**

pluq is a surgical archive extraction tool. Point it at a ZIP or RAR — local, a direct URL,
Google Drive, or Dropbox — browse it in a fast terminal UI, and pull out exactly the files you
want. It reads only the bytes it needs: the central directory to list, and just the ranges for
the entries you extract.

<!-- DEMO: hero asciicast/GIF goes here — deferred (manual recording task). -->

## Features

- **Surgical extraction** — fetches metadata + only the selected entries' byte ranges, not the
  whole archive.
- **Formats** — ZIP (Deflate, Store, BZip2, LZMA, Zstandard), RAR5, RAR4. Extraction only.
- **Sources** — local files, HTTP(S) direct URLs, Google Drive, Dropbox. *(OneDrive: planned.)*
- **TUI-first** — browse, search, multi-select, and extract from a minimalist terminal UI.
  Also scriptable via `pluq get`/`list`/`test`.
- **Private files** — OAuth for Google Drive and Dropbox (`pluq auth add`).
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
irm https://github.com/stefandekanski/pluq/releases/latest/download/pluq-installer.ps1 | iex
```

**Homebrew (macOS)**
```sh
brew install stefandekanski/tap/pluq
```

Prebuilt binaries for every platform are on the [Releases](https://github.com/stefandekanski/pluq/releases)
page (macOS x64/arm64, Linux x64/arm64, Windows x64) — see [docs/INSTALL.md](docs/INSTALL.md)
for manual install, Scoop, and the macOS Gatekeeper note.

> pluq is distributed as a prebuilt binary. `cargo install pluq` is **not** offered.

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
[docs/AUTH.md](docs/AUTH.md) for accessing private Google Drive / Dropbox files.

## Project status

pluq is **pre-1.0** and **solo-maintained**. It does one thing — extraction — and aims to do it
well. Fixes are best-effort; archive creation is explicitly out of scope. Bug reports and ideas
are welcome via [Issues](https://github.com/stefandekanski/pluq/issues) and
[Discussions](https://github.com/stefandekanski/pluq/discussions).

## Privacy

pluq collects **zero telemetry** — nothing is ever transmitted. `pluq bug-report` and crash
reports are **local files** you choose whether to share. See [PRIVACY.md](PRIVACY.md).

## License

Dual-licensed under either of **MIT** ([LICENSE-MIT](LICENSE-MIT)) or **Apache-2.0**
([LICENSE-APACHE](LICENSE-APACHE)) at your option. RAR support uses the UnRAR library under its
own (extraction-only) terms — see [LICENSE-THIRD-PARTY.md](LICENSE-THIRD-PARTY.md).
