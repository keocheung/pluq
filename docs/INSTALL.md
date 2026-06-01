# Installing pluq

pluq ships as a single self-contained binary — no runtime, no Rust toolchain needed.

## Recommended: one-liner installers

**macOS / Linux**
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/stefandekanski/pluq/releases/latest/download/pluq-installer.sh | sh
```

**Windows (PowerShell)**
```powershell
irm https://github.com/stefandekanski/pluq/releases/latest/download/pluq-installer.ps1 | iex
```

These download the right binary for your OS + architecture and place it on your `PATH`. Because
the file arrives via curl/irm, macOS Gatekeeper and Windows SmartScreen do not flag it.

## Homebrew (macOS)

```sh
brew install stefandekanski/tap/pluq
```

The smoothest macOS path — Homebrew handles placement and avoids Gatekeeper friction entirely.

## Manual download (all platforms / offline / CI)

Grab the archive for your platform from the
[Releases page](https://github.com/stefandekanski/pluq/releases/latest):

| Platform | Asset |
|----------|-------|
| macOS (Apple Silicon) | `pluq-aarch64-apple-darwin` |
| macOS (Intel)         | `pluq-x86_64-apple-darwin` |
| Linux (x64)           | `pluq-x86_64-unknown-linux-gnu` |
| Linux (arm64)         | `pluq-aarch64-unknown-linux-gnu` |
| Windows (x64)         | `pluq-x86_64-pc-windows-msvc` |

Unpack it and move `pluq` (or `pluq.exe`) somewhere on your `PATH`.

### macOS Gatekeeper note

A manually downloaded binary carries the quarantine flag and macOS may refuse to run it
("cannot be opened because the developer cannot be verified"). pluq is **not** code-signed or
notarized. Clear the flag:

```sh
xattr -cr /path/to/pluq
```

(The one-liner installer and Homebrew avoid this — only manual downloads are affected.)

## Verify

```sh
pluq --version
```

## Shell completions

```sh
pluq completions bash > ~/.local/share/bash-completion/completions/pluq   # bash
pluq completions zsh  > "${fpath[1]}/_pluq"                                # zsh
pluq completions fish > ~/.config/fish/completions/pluq.fish              # fish
```

`powershell` and `elvish` are also supported.
