#!/usr/bin/env bash
# Regenerates the tray icon alpha masks from assets/tray.svg (black lines over a white fill).
# Each size is rendered from the vector, then alpha * (1 - luminance) keeps only the lines,
# written as raw 8-bit masks that the client tints black or white.
# Needs inkscape and ImageMagick (magick).
set -euo pipefail
assets=$(cd "$(dirname "$0")/../crates/client/assets" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for size in 22 32 48 64 128; do
    inkscape "$assets/tray.svg" --export-type=png --export-filename="$tmp/$size.png" -w "$size" -h "$size" >/dev/null 2>&1
    magick "$tmp/$size.png" -alpha on \( +clone -alpha extract \) \( -clone 0 -alpha off -colorspace Gray -negate \) \
        -delete 0 -compose Multiply -composite -level 0%,50% -depth 8 "gray:$assets/tray-$size.mask"
    test "$(stat -c %s "$assets/tray-$size.mask")" -eq $((size * size))
done
echo "masks written to $assets"
