#!/usr/bin/env bash
# Renders the fire emoji into assets/lighter.icns.
#
# Run rarely, by hand, when the icon should change; the icns is committed so
# nothing at build or install time needs Swift or an emoji font.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/draw.swift" <<'SWIFT'
import AppKit
let size = CGFloat(1024)
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()
let text = "🔥" as NSString
let font = NSFont.systemFont(ofSize: 820)
let attrs: [NSAttributedString.Key: Any] = [.font: font]
let bounds = text.size(withAttributes: attrs)
text.draw(at: NSPoint(x: (size - bounds.width) / 2, y: (size - bounds.height) / 2), withAttributes: attrs)
image.unlockFocus()
let tiff = image.tiffRepresentation!
let rep = NSBitmapImageRep(data: tiff)!
let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
SWIFT
swift "$WORK/draw.swift" "$WORK/icon-1024.png"

ICONSET="$WORK/lighter.iconset"
mkdir "$ICONSET"
for s in 16 32 64 128 256 512; do
	sips -z "$s" "$s" "$WORK/icon-1024.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
	d=$((s * 2))
	sips -z "$d" "$d" "$WORK/icon-1024.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$ROOT/assets/lighter.icns"
echo "wrote $ROOT/assets/lighter.icns ($(du -h "$ROOT/assets/lighter.icns" | cut -f1))"
