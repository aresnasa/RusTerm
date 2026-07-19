#!/usr/bin/env bash
set -euo pipefail

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
APP="${1:-$ROOT/dist/RusTerm.app}"
PLIST="$APP/Contents/Info.plist"
ICON="$APP/Contents/Resources/AppIcon.icns"
SVG="$APP/Contents/Resources/assets/gemini-svg.svg"
BINARY="$APP/Contents/MacOS/rusterm"

for path in "$PLIST" "$ICON" "$SVG" "$BINARY"; do
    if [[ ! -e "$path" ]]; then
        echo "error: missing bundle entry: $path" >&2
        exit 1
    fi
done

plutil -lint "$PLIST" >/dev/null
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundlePackageType' "$PLIST")" == "APPL" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$PLIST")" == "rusterm" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$PLIST")" == "AppIcon.icns" ]]
[[ -x "$BINARY" ]]
cmp -s "$ROOT/assets/gemini-svg.svg" "$SVG"
file "$ICON" | grep -q 'Mac OS X icon'
codesign --verify --deep --strict "$APP"

echo "Verified macOS app bundle: $APP"
