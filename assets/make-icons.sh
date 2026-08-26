#!/usr/bin/env bash
#
# Generate the application icon set from assets/snatch.png.
#
# The source art is a light-grey line drawing on transparency. On its own it
# is close to invisible against a light panel or a light-theme dash, so it is
# composited onto a dark rounded tile: that is what makes it legible on any
# background, and it is what every other icon in a modern Linux dock does.
#
# Re-run after changing the source art. Output is committed so a build does
# not need ImageMagick.
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="${HERE}/snatch.png"
OUT="${HERE}/icons"
BASE=512
# Rounded-square corner radius and how much of the tile the art occupies.
RADIUS=$(( BASE * 22 / 100 ))
INSET=$(( BASE * 74 / 100 ))

command -v magick >/dev/null 2>&1 || { echo "ImageMagick 7 (magick) is required" >&2; exit 1; }
[ -f "${SOURCE}" ] || { echo "missing ${SOURCE}" >&2; exit 1; }

rm -rf "${OUT}"
mkdir -p "${OUT}"

# The tile: a dark slate rounded square with a subtle vertical lift, so the
# icon has some depth without turning into a gradient show.
magick -size "${BASE}x${BASE}" \
  gradient:'#2b3242-#171a21' \
  \( -size "${BASE}x${BASE}" xc:none \
     -draw "roundrectangle 0,0,$((BASE-1)),$((BASE-1)),${RADIUS},${RADIUS}" \
     -alpha extract \) \
  -alpha off -compose CopyOpacity -composite \
  "${OUT}/tile.png"

# The art: trimmed, scaled to fit the inset, and brightened so the bones read
# against the dark tile.
magick "${SOURCE}" \
  -trim +repage \
  -resize "${INSET}x${INSET}" \
  -channel RGB -evaluate multiply 1.18 +channel \
  "${OUT}/art.png"

magick "${OUT}/tile.png" "${OUT}/art.png" -gravity center -composite \
  "${OUT}/com.snatch.dl-512.png"

# Every size a desktop actually asks for.
for size in 256 128 96 64 48 32 24 16; do
  magick "${OUT}/com.snatch.dl-512.png" \
    -filter Lanczos -resize "${size}x${size}" \
    "${OUT}/com.snatch.dl-${size}.png"
done

rm -f "${OUT}/tile.png" "${OUT}/art.png"
echo "wrote $(ls "${OUT}" | wc -l) icons to ${OUT}"
