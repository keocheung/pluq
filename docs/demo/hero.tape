# pluq — hero demo: snipe one file out of a 1.86 GB remote ZIP, in seconds, in the TUI.
#
# Story: Electron's Windows debug-symbol bundle is a 1.86 GB zip. We want ONE
# file out of it (libGLESv2.dll.pdb). pluq fetches only that file's byte range
# (≈17.2 MB) — not the 1.86 GB archive. (Chosen over ffmpeg.dll.pdb: its 3.4 MB
# fetch finishes before the progress bar can visibly fill — no payoff shot.)
#
# Render:  vhs docs/demo/hero.tape       (outputs docs/demo/hero.gif)
# Install: brew install vhs   |   https://github.com/charmbracelet/vhs
# Needs:   `pluq` on PATH (e.g. `cargo install --path crates/pluq`, or a release binary).
#
# Tweak FontSize/Width/Height/Theme and the Sleep beats to taste, then re-render.

Output docs/demo/hero.gif

Require pluq

Set Shell       "bash"
Set FontSize    18
Set Width       1200
Set Height      720
Set Padding     20
Set Theme       "Catppuccin Mocha"
Set WindowBar   Colorful
Set TypingSpeed 90ms

# --- invisible setup (not recorded): clean temp dir + warm the range cache --
# The recorded open MUST be deterministic. A cold HTTP-range open of a 1.86 GB
# remote ZIP can take several seconds — longer than the Sleeps below — and then
# the filter keystrokes land while pluq is still "Opening archive…", which
# desyncs the whole demo (it ends up extracting the wrong files). So we pre-open
# the same URL here (off-camera) to fill the central-directory cache; the
# recorded open is then near-instant and timing is stable across machines.
Hide
Type "cd /tmp && rm -rf pluq-demo && mkdir pluq-demo && cd pluq-demo && export PS1='$ ' && clear"
Enter
Type `pluq "https://github.com/electron/electron/releases/download/v42.4.0/electron-v42.4.0-win32-x64-pdb.zip"`
Enter
Sleep 10s
Type "q"
Sleep 500ms
Type "clear"
Enter
Show

# Launch pluq → the Home screen. Typed at human speed.
Type "pluq"
Sleep 600ms
Enter

# Let the Home screen settle. The Pluq Input is focused by default.
Sleep 2500ms

# The ONE jump: paste a 1.86 GB remote ZIP URL into the Pluq Input. A human
# pastes a long link, they don't type it — so this beat is instant.
Set TypingSpeed 0
Type `https://github.com/electron/electron/releases/download/v42.4.0/electron-v42.4.0-win32-x64-pdb.zip`
Set TypingSpeed 90ms

# Eyes scan the pasted URL, then commit.
Sleep 1800ms
Enter

# Central directory served from the warm cache → the listing appears at once.
# Pause so it registers before we move.
Sleep 2500ms

# Browse: glide the cursor down to electron.exe.pdb — a single 3.6 GB file —
# and pause. The status bar shows its size and Cost (687.3 MB to extract).
# This sells how huge the archive is before we snipe the one file we want.
Down@200ms 8
Sleep 1800ms

# Filter the listing to the one file we want. Typing "libgles" narrows the
# list live (case-insensitive substring); the hint reads
# [Filter: "libgles" (1 matches) — Enter: accept]. Enter accepts the filter
# (the listing stays narrowed) and drops focus onto the result.
Type "/"
Sleep 1000ms
Type "libgles"
Sleep 1800ms
Enter

# Filtered down to one file (libGLESv2.dll.pdb). Select the filtered set with `a`.
Sleep 1500ms
Type "a"
Sleep 1800ms

# Extract (Enter) → modal opens. Default = a folder named after the archive
# (pre-filled). Read it, then accept with Enter.
Enter
Sleep 2500ms
Enter

# Only libGLESv2.dll.pdb's byte range is fetched (≈17.2 MB compressed → 84.6 MB
# written) — a LIVE download, so the progress bar fills here (the payoff shot).
# Measured ~4.7s on a fast connection; 7s leaves headroom. Bump if your
# connection is slow and the bar gets cut off.
Sleep 7000ms

# Hold the final "Extracted …" frame so the loop reads cleanly.
Sleep 2500ms
