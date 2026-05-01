# TUI Vision

> The long-term **feature** vision for pluq's terminal UI.
> This document describes *what the TUI can do* — screens, capabilities, user flows.
> For *how it looks* (colors, borders, layout rules, key bindings), see `TUI_STYLE_GUIDE.md`.

---

## Principle

`pluq` with no arguments launches a full interactive application.
The TUI is the primary experience. CLI commands exist for scripting and automation.

---

## Data & Persistence

- **Bookmarks:** Stored in `~/.config/pluq/bookmarks.json`.
- **Recent:** Stored in `~/.config/pluq/recent.json`.

### Schema Example
```json
{
  "name": "Photos 2024",
  "source": "https://drive.google.com/...",
  "provider": "GDrive",
  "size": 4509715660,
  "last_opened": "2026-04-11T14:30:00Z"
}
```

---

## Overlays & Prompts

### File Collisions (Extraction)
If exact file names collide during batch extraction, the following prompt is displayed:
`[!] N files already exist. (o) Overwrite All (s) Skip All (c) Cancel`
*(Note: Merging into existing folders is done silently without prompting).*

### Bookmark Collisions
If a user tries to save or rename a bookmark to a name that already exists, the following prompt is displayed:
`[!] Bookmark "[Name]" already exists. (o) Overwrite (r) Rename (c) Cancel`

---

## Home Screen

Launching `pluq` shows the home screen — an entry point where users can open
a new archive or jump back into a saved/recent one.

### Capabilities & Keys
- `b`: **Bookmark** the selected item from the Recent list.
- `r`: **Rename** the selected bookmark.
- `d`: **Delete** the selected bookmark or recent entry.

### pluq input

- Text input: paste a URL (Google Drive, Dropbox, etc.) or a local file path
- Tab-completion for local paths (future)

### Bookmarks

- User explicitly saves an archive with a custom name
- Stored in `~/.config/pluq/bookmarks.json`
- Shows: user-given name, provider tag (GDrive / Dropbox / Local), file size
- Select one to open it immediately
- Combined with the metadata cache (`~/.cache/pluq/`), re-opens are
  near-instant — no re-scan needed

### Recent Archives

- Automatically tracked — last 10 opened archives
- Stored in `~/.config/pluq/recent.json`
- Duplicates with bookmarks are hidden from the recent list

---

## Browse Screen

After opening an archive (from home screen, `pluq <source>`, or a bookmark),
the user lands in the browse screen — a drill-down file browser.

### Capabilities

- **Drill-down navigation:** Enter directories, back out to parent
- **Search/filter:** Filter the current folder's entry list
- **Selection:** Toggle individual files and directories, or select all in folder
- **Selection Review:** Full-screen view of all staged files, with pruning
- **Extraction Cost:** Live cost estimate for highlighted and selected entries
- **Add Bookmark:** `b` to bookmark the currently browsing archive.

---

## Extraction

### Destination Strategy

Before extraction begins, the user chooses where to extract:

1. **Archive-name folder (default):** `photos.zip` extracts to `./photos/`
2. **Here:** Extract directly into CWD
3. **Custom name:** User types a folder name

### Progress

- Progress bar with bytes extracted / total
- Current file name and batch index
- Speed indicator and ETA
- Cancel with Esc (returns to browse, partial extraction kept)

After extraction completes, return to browse with a status message.

---

## CLI Commands (Coexistence)

CLI commands remain for non-interactive use:

| Command | Purpose |
| --- | --- |
| `pluq` | Launch TUI (home screen) |
| `pluq <source>` | Launch TUI directly into browse screen |
| `pluq list <source>` | Print entries to stdout (scriptable) |
| `pluq get <source> [glob] [-o dir]` | Non-interactive extraction |
| `pluq test <source>` | Verify integrity (CI/automation) |
| `pluq auth add/remove/list` | Manage OAuth tokens |

The TUI and CLI share all the same core logic. The TUI is the primary UX;
CLI commands are the scripting/automation interface.

---

## Ideas (Unscoped)

Space for future ideas — not committed, just captured.

- **Preview pane:** show text file contents or image thumbnails inline
- **Drag and drop:** paste paths from file manager (terminal permitting)
- **Multi-archive tabs:** open several archives, switch between them
- **Output directory picker:** choose where to extract within the TUI
- **Theme support:** color schemes, light/dark mode
- **Mouse support:** ratatui supports mouse events — click to select, scroll
