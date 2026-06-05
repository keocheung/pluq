# Using pluq

## Sources

A *source* is the first positional argument to `pluq` and most subcommands. It can be:

- a **local path** — `./photos.zip`, `/data/backup.rar`
- an **HTTP(S) URL** — `https://example.com/big.zip` (server must support range requests)
- a **Google Drive** share URL or file (see [AUTH.md](AUTH.md))
- a **Dropbox** share URL (see [AUTH.md](AUTH.md))

## TUI

Run `pluq` with no arguments for the home screen, or `pluq <source>` to open an archive directly. On the home screen, press `c` to add files from cloud providers.

| Key | Action |
|-----|--------|
| `↑`/`↓`, `k`/`j` | move cursor |
| `→`/`l` | enter folder · `←`/`h`/`Backspace` | go up |
| `Space` | toggle-select file (or whole folder) · `a` | select all |
| `Enter` | extract selection |
| `/` | search / glob-filter the current folder |
| `Tab` | review & prune the current selection |
| `g`/`G`, `Home`/`End` | jump to top / bottom |
| `?` | help overlay (full key list) |
| `c` | add from cloud (Google Drive / Dropbox picker) — on Home screen |
| `Esc` | back one level · `q` | quit |

## Commands

### `pluq list <source> [--all]`
Print the file list. `--all` includes OS junk (`.DS_Store`, `__MACOSX/`, `Thumbs.db`).

### `pluq get <source> [glob] [-o <dir>] [--all]`
Extract. With no glob, extracts everything. Glob matching is **gitignore-style**: a pattern
containing `/` matches the full entry path (`photos/**/*.jpg`); otherwise it matches the basename
(`*.jpg`). `-o`/`--output` sets the destination (default: the archive name without its extension).

```sh
pluq get backup.zip "docs/**/*.pdf" -o ~/pdfs
pluq get backup.zip --all -o restore/
```

### `pluq test <source> [--all]`
Decompress and CRC-verify every entry. Exits non-zero if any entry fails — useful in scripts.

### `pluq auth {add <provider> | remove <provider> | list}`
Manage OAuth tokens for private cloud files. See [AUTH.md](AUTH.md).

### `pluq completions <shell>`
Print a completion script (`bash`, `zsh`, `fish`, `powershell`, `elvish`) to stdout.

## Global flags

| Flag | Effect |
|------|--------|
| `--debug` | verbose (debug-level) logging; `PLUQ_LOG` is also honored |
| `--clean-cache` | delete the persistent metadata cache before running |
| `--clean-logs` | delete the debug-log directory before running |

## Cache & logs

pluq keeps a small **metadata cache** (archive listings/locations) so reopening a remote archive
skips re-scanning, and a capped rolling **debug log**. Both live in your platform's standard app
directories. Clear them with `--clean-cache` / `--clean-logs`. Set `PLUQ_LOG=debug` (or use
`--debug`) to raise log verbosity. Logs redact URLs and tokens.
