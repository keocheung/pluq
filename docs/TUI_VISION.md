# TUI Vision

> The long-term vision for ArchiveSniper's terminal UI.
> This is a living document — add ideas as they come up.

---

## Principle

`snipe` with no arguments launches a full interactive application.
The TUI is the primary experience. CLI commands exist for scripting and automation.

---

## Home Screen

Launching `snipe` shows the home screen — an entry point where users can open
a new archive or jump back into a saved/recent one.

```
┌─ Archive Sniper ─────────────────────────────────────────┐
│                                                           │
│  Open: [_________________________________________] [Go]   │
│                                                           │
│  ┌─ Saved Archives ───────────────────────────────────┐   │
│  │                                                    │   │
│  │  photos-2024.rar        Google Drive    4.2 GB     │   │
│  │  project-backup.zip     Local           890 MB     │   │
│  │  assets-v3.rar          Dropbox         1.1 GB     │   │
│  │                                                    │   │
│  ├─ Recent ───────────────────────────────────────────┤   │
│  │                                                    │   │
│  │  demo.zip               Local           12 MB      │   │
│  │  client-data.rar        Google Drive    340 MB     │   │
│  │                                                    │   │
│  └────────────────────────────────────────────────────┘   │
│                                                           │
│  enter: open  s: save  d: delete  /: search               │
└───────────────────────────────────────────────────────────┘
```

### Open field

- Text input: paste a URL (Google Drive, Dropbox, etc.) or a local file path
- Tab-completion for local paths (future)

### Saved Archives (Bookmarks)

- User explicitly saves an archive with a name ("save link" action)
- Stored in `~/.config/archive-sniper/bookmarks.json`
- Shows: user-given name, provider (Google Drive / Dropbox / Local), file size
- Select one to open it immediately
- Combined with the metadata cache (`~/.cache/archive-sniper/`), re-opens are
  near-instant — no re-scan needed

### Recent Archives

- Automatically tracked — last N opened archives
- Stored alongside bookmarks (or in a separate recent.json)
- Duplicates with saved archives are hidden from the recent list

---

## Browse Screen

After opening an archive (from home screen, `snipe browse <url>`, or a bookmark),
the user lands in the browse screen. This is what Phase 3 already implements,
extended with upcoming features.

```
┌─ photos-2024.rar ─ Google Drive ─ 4.2 GB ───────────────┐
│                                                           │
│  Search: [filter...______]                                │
│                                                           │
│  [x] photos/vacation/IMG_001.jpg          2.4 MB          │
│  [ ] photos/vacation/IMG_002.jpg          3.1 MB          │
│  [x] photos/vacation/IMG_003.jpg          1.8 MB          │
│  [ ] documents/readme.txt                 4.2 KB          │
│                                                           │
│  [Extract Selected]  2 files, 4.2 MB                      │
│                                                           │
│  q: quit  /: search  space: select  enter: extract        │
│  s: save link  esc: home                                  │
└───────────────────────────────────────────────────────────┘
```

### Planned features (from roadmap)

- **Search/filter:** type to filter entries by name
- **Multi-select:** space to toggle, bulk extract
- **Sort:** by name, size, date
- **Tree view:** expand/collapse directories (toggle between flat and tree)

### Navigation

- `esc` from browse returns to home screen (not quit)
- `q` quits the app entirely
- `s` saves the current archive as a bookmark

---

## Extraction

When extracting (single or batch), the browse screen transitions to show progress:

- Progress bar with bytes extracted / total
- Current file name
- Speed indicator (MB/s)
- Cancel with `esc` (returns to browse, partial extraction kept)

After extraction completes, return to browse with a status message.

---

## CLI Commands (Coexistence)

CLI commands remain for non-interactive use:

| Command | Purpose |
| --- | --- |
| `snipe` | Launch TUI (home screen) |
| `snipe browse <source>` | Launch TUI directly into browse screen |
| `snipe list <source>` | Print entries to stdout (scriptable) |
| `snipe extract <source> <entry>` | Extract one file (scriptable) |
| `snipe test <source>` | Verify integrity (CI/automation) |

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
