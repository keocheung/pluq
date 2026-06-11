# pluq — format demo: RAR without Quick Open → the one-time scan, in the TUI.
#                      + glob filter, select the filtered set, custom output folder.
#
# Story: a YOLO training dataset RAR — 1034 files (518 images + 517 label
# .txt files), ~36 MB. No Quick Open record, so pluq scans the header chain
# once (the visible scan bar). Then the surgical bit: glob-filter to *.txt,
# select all 517 labels, extract them — only ≈46 KB fetched; the ~35 MB of
# images never leave the server.
#
# Render:  vhs docs/demo/rar-scan.tape   (outputs docs/demo/rar-scan.gif)
# Install: brew install vhs   |   https://github.com/charmbracelet/vhs
# Needs:   `pluq` on PATH (e.g. `cargo install --path crates/pluq`, or a release binary).
#          URL = a range-supporting RAR with NO Quick Open record, so pluq must
#          scan the header chain (visible progress bar). --clean-cache forces a
#          cold scan every take (a first-time user sees the same scan).
#
# Tweak FontSize/Width/Height/Theme and the Sleep beats to taste, then re-render.

Output docs/demo/rar-scan.gif

Require pluq

Set Shell       "bash"
Set FontSize    18
Set Width       1200
Set Height      720
Set Padding     20
Set Theme       "Catppuccin Mocha"
Set WindowBar   Colorful
Set TypingSpeed 90ms

# --- invisible setup (not recorded): clean temp dir + tidy prompt -----------
Hide
Type "cd /tmp && rm -rf pluq-demo && mkdir pluq-demo && cd pluq-demo && export PS1='$ ' && clear"
Enter
Show

# Launch the TUI on a remote non-QO RAR. The command is typed at human speed;
# only the long URL is pasted (the ONE jump) — a human pastes a link, they
# don't type it out.
Type `pluq --clean-cache "`
Set TypingSpeed 0
Type `https://github.com/phd-benel/BGI/releases/download/dataset/Bgi_dataset_yolov7_label.rar`
Set TypingSpeed 90ms
Type `"`
Sleep 1500ms
Enter

# No Quick Open index → pluq scans the header chain. The progress bar stays on
# screen for the whole scan (NOT hidden) — this scan IS the point of the demo.
#
# CRITICAL: this Sleep must outlast the COLD open + full scan before ANY
# keystroke below fires. vhs sends keys on a fixed clock regardless of app
# state, so if a key lands while pluq is still "Opening archive…"/"Scanning…",
# it's dropped and the whole demo desyncs (wrong/too-few files extracted).
# Measured ~7.5s cold (open + scan) on a fast connection; 12s leaves headroom.
# This is the knob to tune: measure your worst case and pad it. Unlike the hero
# tape we can't warm the cache: a cold scan is the point, so the bar must render
# live. Re-render a few times and keep the take where the scan bar fills most
# cleanly.
Sleep 12000ms

# Pause on the loaded listing — 1034 files — so the scale registers before we act.
Sleep 3000ms

# The surgical bit: glob-filter to just the label files. Typing "*.txt" narrows
# the listing live; the hint shows the match count (517). Enter accepts the
# filter (the listing stays narrowed) and drops focus onto the results.
Type "/"
Sleep 1000ms
Type "*.txt"
Sleep 1800ms
Enter

# Select the whole filtered set with `a` — 517 labels, zero images.
Sleep 1500ms
Type "a"
Sleep 1800ms

# Extract (Enter) → modal. Arrow down to Custom, Enter to edit, type our own
# folder name (human speed), Enter to confirm.
Enter
Sleep 2500ms
Down
Sleep 1000ms
Down
Sleep 1500ms
Enter
Sleep 1000ms
Type "bgi-labels"
Sleep 2000ms
Enter

# Only the labels' byte ranges are fetched (≈46 KB total, coalesced) — the 35 MB
# of images are never downloaded. Near-instant; the min-visible floor keeps the
# bar on screen.
Sleep 4000ms

# Hold the final "Extracted …" frame so the loop reads cleanly.
Sleep 4000ms
