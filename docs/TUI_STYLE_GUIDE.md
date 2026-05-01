# TUI Style Guide

> The canonical design system for pluq's terminal UI.
> Feature specs (zen-mode, home-screen, etc.) compose these building blocks.
> If a rule here conflicts with a feature spec, **this document wins**.

---

## 1. Color Palette

Six named tokens. Every color in the TUI must map to one of these.

| Token     | Value                  | Usage                                                        |
| --------- | ---------------------- | ------------------------------------------------------------ |
| `bg`      | `#000000` (pure black) | All backgrounds                                              |
| `accent`  | `#00ffff` (cyan)       | Cursor bg, active headers, directory names, gauges, borders on modal overlays, interactive elements |
| `text`    | `#ffffff` (white)      | File names, primary content, breadcrumb header                |
| `muted`   | `#555555` (dark gray)  | Separators, size column, footer/hint text, inactive elements  |
| `error`   | `#ff0000` (red)        | Error messages, validation failures                          |
| `warning` | `#ffff00` (yellow)     | Caution modals, large-dictionary warnings                    |

No other colors. If a new screen needs a color not listed here, add it to this table first.

---

## 2. Typography

| Element                  | Foreground | Modifier | Notes                              |
| ------------------------ | ---------- | -------- | ---------------------------------- |
| Breadcrumb header        | `text`     | **Bold** | ` archive.zip / photos /`          |
| Section header (active)  | `accent`   | **Bold** | ` Review Selection`, ` BOOKMARKS`  |
| Section header (inactive)| `muted`    | ---      | Dimmed when section not focused     |
| File name                | `text`     | ---      | ` main.rs`                         |
| Directory name           | `accent`   | ---      | Always has trailing slash: `renderer/` |
| Size column              | `muted`    | ---      | ` 2.4 MB` — fixed 10-char width    |
| Footer / hints           | `muted`    | ---      | ` Enter extract  q quit`           |
| Selection marker         | ---        | ---      | `+` prefix, inherits row style     |
| Error text               | `error`    | ---      |                                    |
| Warning text             | `warning`  | ---      |                                    |

---

## 3. Cursor / Highlight

One rule, everywhere:

> **`accent` background, `bg` (black) foreground, Bold.**

Applies to all navigable lists: browse, review, home screen, any future list.

---

## 4. Chrome & Borders

> **No outer borders or boxes on content screens.**

Screens use horizontal `---` lines (rendered as `"─".repeat(width)` in `muted`) to separate header, content area, and footer.

**Exception: Modal overlays** (help popup, bookmark overlay, confirmation dialogs) use a bordered `Block` with an `accent` border to visually float above dimmed content.

---

## 5. Layout Structure

All content screens follow this 5-row vertical layout:

```
Row 1  Header     (1 line)   Breadcrumb, screen title, filter indicator
Row 2  Separator   (1 line)   ─── muted horizontal line
Row 3  Content    (fills)    File list, progress info, input fields
Row 4  Separator   (1 line)   ─── muted horizontal line
Row 5  Footer     (1 line)   Details bar, selection summary, key hints
```

Modal overlays are rendered on top of this layout with a dimmed backdrop, not inside it.

---

## 6. Key Binding Rules

### Navigation

| Key                        | Action              | Context       |
| -------------------------- | ------------------- | ------------- |
| `j` / `k` / `Up` / `Down` | Move cursor in list | All lists     |
| `g` / `Home`              | Jump to top          | All lists     |
| `G` / `End`               | Jump to bottom       | All lists     |
| `l` / `Right` / `Enter`   | Dive into folder     | Browse (dirs) |
| `h` / `Left` / `Backspace`| Back to parent       | Browse        |

### Actions

| Key     | Action                          | Context          |
| ------- | ------------------------------- | ---------------- |
| `Space` | Toggle selection (files & dirs) | Browse, Review   |
| `a`     | Toggle all in current folder    | Browse           |
| `Tab`   | Toggle review mode              | Browse (if selection exists) |
| `Enter` | Start extraction                | Browse (if selection exists), Review |
| `/`     | Start filter input              | Browse           |
| `?`     | Toggle help overlay             | Browse           |

### Global

| Key   | Action                            |
| ----- | --------------------------------- |
| `Esc` | **Back one level** (see below)    |
| `q`   | **Quit the app** (see below)      |

### Global Quit Behavior

1. **Standard Context:** `q` is the universal key to quit the application from any screen.
2. **Text Input Focused:** When an input field (like `PluqInput`) is active, `q` is captured for text entry. In this context, `Ctrl+q` is used to quit the application.

### Esc Behavior (The "Back" Button)

`Esc` always retreats from the most immediate context. It never quits the app.

1. Modal open → close modal
2. In filter input → exit filter mode
3. In review mode → back to browse
4. In subdirectory → navigate to parent
5. At root of browse → return to home screen
6. At home screen → nothing (dead end; use `q` to quit)

`q` is the only way to exit the application, from any screen.

---

## 7. Components

Reusable building blocks. Feature specs reference these by name.

### Content Screen

The standard 5-row layout from Section 5. Used for browse, review, home screen sections, progress, and error screens.

### List / Table

Two columns:
- **Size** — `muted`, fixed 10-char width. Empty for directories.
- **Name** — `text` (files) or `accent` (directories, with trailing `/`). Fills remaining width.

Cursor: Section 3 highlight style.
Selection: `+` marker prefixed to name.

### Modal Overlay

- Centered on screen.
- Bordered `Block` with `accent` border.
- Content behind the modal is dimmed.
- `Esc` closes. `Enter` confirms.

### Progress Screen

Uses the Content Screen layout:
- **Header:** Screen title (e.g., `Extracting` or `Scanning archive...`).
- **Content area:**
  - Current file name.
  - `Gauge` widget styled with `accent`.
  - Byte counter: `Extracted:  1.2 MB / 4.8 MB`
  - Speed + ETA: `1.2 MB/s  ETA 2:34` (shown when available).
  - Batch label (if applicable): `(4 of 12 files)`.
- **Footer:** Cancel/quit hints in `muted`.

### Text Input

- Label in `text`, input value in `accent`.
- Cursor indicator: trailing `_`.
- For passwords: mask with `*` characters.

### Separator

`"─".repeat(terminal_width)` in `muted`. Used in rows 2 and 4 of Content Screen.

### Footer

Single line in `muted`. Content varies by context:
- **No selection:** Full path + size + cost of highlighted item.
- **With selection:** `[ N files --- SIZE · Cost: SIZE ]  Enter extract  q quit`
- **Status message:** Temporary feedback (e.g., `Extracted 'photo.jpg' (2.4 MB)`).

---

## 8. Current Deviations

Screens that still use the old bordered style and need migration:

- `Opening` phase — bordered `Block`
- `Scanning` phase — bordered `Block`, green gauge (should be `accent`)
- `Extracting` / `ExtractingBatch` — bordered `Block`
- `ConfirmExtract` — bordered `Block` with yellow border
- `Error` phase — bordered `Block`
- Help popup — bordered (acceptable as modal overlay)

These should be migrated to the Content Screen layout as defined above.
