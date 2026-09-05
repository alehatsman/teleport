#!/usr/bin/env bash
# Regenerates desktop/src-tauri/icons/ from a small vector-ish mark drawn
# directly with ImageMagick -- issue #14 (replace the M10 scaffold's
# placeholder teal square). No source design file to keep in sync: this
# script *is* the source of truth, so the icon can be regenerated or tweaked
# without a design tool.
#
# Mark: a ">_" terminal-prompt glyph in the app's brand accent
# (web/src/app.css's --accent: #7c6cf0) on the brand background
# (--bg: #101014), rounded-rect app-icon style. Drawn as strokes, not text,
# so it centers on its own trimmed bounding box instead of font metrics
# (a bare `-gravity center -annotate` with this font left it visibly
# bottom-heavy -- ascender/descender space the glyph itself doesn't use).
#
# Requires: ImageMagick 6 (`convert`) and `png2icns` (Debian/Ubuntu:
# `apt install icnsutils`). Both only needed to regenerate; the generated
# files are committed, so a normal build never runs this.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="$repo_root/desktop/src-tauri/icons"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
cd "$work_dir"

bg="#101014"
accent="#7c6cf0"

# Rounded-square background.
convert -size 1024x1024 xc:none \
  -fill "$bg" -draw "roundrectangle 0,0,1023,1023,190,190" \
  bg.png

# ">_" glyph as round-capped strokes on its own transparent canvas, then
# trimmed to its bounding box so it can be recentered exactly regardless of
# the coordinates used to draw it.
convert -size 1024x1024 xc:none \
  -stroke "$accent" -strokewidth 92 -fill none \
  -draw "stroke-linecap round stroke-linejoin round polyline 300,300 560,512 300,724" \
  -draw "stroke-linecap round line 640,724 880,724" \
  glyph_raw.png
convert glyph_raw.png -trim +repage glyph.png

convert bg.png glyph.png -gravity center -compose over -composite PNG32:master.png

# 8-bit RGBA output is required -- Tauri's tray icon loader panics on a
# 16-bit-depth PNG (ImageMagick's default) with a buffer-size mismatch,
# hit scaffolding M10 (see desktop/README.md). `PNG32:` forces 8-bit.
for sz in 16 32 48 64 128 256 512 1024; do
  convert master.png -filter Lanczos -resize "${sz}x${sz}" "PNG32:icon_${sz}.png"
done

mkdir -p "$out_dir"
cp icon_32.png "$out_dir/32x32.png"
cp icon_128.png "$out_dir/128x128.png"
cp icon_256.png "$out_dir/128x128@2x.png"
convert icon_16.png icon_32.png icon_48.png icon_64.png icon_128.png icon_256.png \
  "$out_dir/icon.ico"
png2icns "$out_dir/icon.icns" \
  icon_16.png icon_32.png icon_48.png icon_128.png icon_256.png icon_512.png icon_1024.png

echo "wrote icons to $out_dir"
