#!/usr/bin/env bash
#
# contact-sheet.sh — sample a GIF into a tiled PNG "filmstrip" for quick review.
#
# A rendered .tape GIF is motion; a still can't show timing. This samples one
# frame every INTERVAL seconds and tiles them left→right, top→bottom into a
# single PNG, so the whole timeline is reviewable in one image. Tile k ≈ k·INTERVAL.
#
# Usage:
#   ./contact-sheet.sh hero.gif            # default 1.4s sampling
#   ./contact-sheet.sh hero.gif 0.4        # densify: 0.4s sampling (catch fast flashes)
#
# Output: /tmp/<gif-basename>_sheet.png  (kept out of the repo on purpose).
# Needs:  ffmpeg + ffprobe (brew install ffmpeg).

set -euo pipefail

GIF="${1:?usage: contact-sheet.sh <gif> [interval-seconds]}"
INTERVAL="${2:-1.4}"
COLS=5

for bin in ffmpeg ffprobe; do
  command -v "$bin" >/dev/null || { echo "error: $bin not found (brew install ffmpeg)" >&2; exit 1; }
done
[ -f "$GIF" ] || { echo "error: no such gif: $GIF" >&2; exit 1; }

DURATION=$(ffprobe -v error -select_streams v:0 -show_entries stream=duration \
  -of default=noprint_wrappers=1:nokey=1 "$GIF")

# n = ceil(duration / interval); rows = ceil(n / cols). Grid must be >= n so the
# tile filter never drops frames off the end.
N=$(awk -v d="$DURATION" -v i="$INTERVAL" 'BEGIN{ printf "%d", (d/i)+0.999999 }')
ROWS=$(awk -v n="$N" -v c="$COLS" 'BEGIN{ printf "%d", (n/c)+0.999999 }')

OUT="/tmp/$(basename "${GIF%.*}")_sheet.png"

ffmpeg -y -loglevel error -i "$GIF" \
  -vf "fps=1/${INTERVAL},scale=300:-1,tile=${COLS}x${ROWS}" \
  -frames:v 1 "$OUT"

printf 'gif:      %s\n' "$GIF"
printf 'duration: %ss   sampled every %ss → %s frames, grid %sx%s\n' \
  "$DURATION" "$INTERVAL" "$N" "$COLS" "$ROWS"
printf 'sheet:    %s   (tile k ≈ k·%ss)\n' "$OUT" "$INTERVAL"
