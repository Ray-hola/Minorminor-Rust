#!/bin/sh
set -eu
cd -- "$(dirname "$0")/.."
# Render the editable SVG once at its intrinsic 1024px size, then export macOS sizes.
MINOR_ICON_TMP=$(mktemp -d "${TMPDIR:-/tmp}/minorminor-icon.XXXXXX")
trap 'rm -rf "$MINOR_ICON_TMP"' EXIT HUP INT TERM
MINOR_ICON_SET="$MINOR_ICON_TMP/Minorminor.iconset"
mkdir -p "$MINOR_ICON_SET" native/icons
sips -s format png public/icon.svg --out "$MINOR_ICON_TMP/master.png" >/dev/null
for MINOR_ICON_SIZE in 16 32 128 256 512; do
  sips -z "$MINOR_ICON_SIZE" "$MINOR_ICON_SIZE" "$MINOR_ICON_TMP/master.png" --out "$MINOR_ICON_SET/icon_${MINOR_ICON_SIZE}x${MINOR_ICON_SIZE}.png" >/dev/null
  MINOR_ICON_RETINA=$((MINOR_ICON_SIZE * 2))
  sips -z "$MINOR_ICON_RETINA" "$MINOR_ICON_RETINA" "$MINOR_ICON_TMP/master.png" --out "$MINOR_ICON_SET/icon_${MINOR_ICON_SIZE}x${MINOR_ICON_SIZE}@2x.png" >/dev/null
done
iconutil -c icns "$MINOR_ICON_SET" -o native/icons/icon.icns
cp "$MINOR_ICON_TMP/master.png" native/icons/icon.png
