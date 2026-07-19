#!/usr/bin/env bash
set -euo pipefail

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
PROFILE="release"
CARGO_ARGS=(--release)

if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
    CARGO_ARGS=()
elif [[ -n "${1:-}" ]]; then
    echo "usage: $0 [--debug]" >&2
    exit 2
fi

cd "$ROOT"
cargo build -p rusterm-app "${CARGO_ARGS[@]}"

BINARY="$TARGET_DIR/$PROFILE/rusterm"
ICON_SOURCE="$(python3 - "$TARGET_DIR/$PROFILE/build" <<'PY'
import glob
import os
import sys

icons = glob.glob(os.path.join(sys.argv[1], "rusterm-app-*", "out", "AppIcon.icns"))
print(max(icons, key=os.path.getmtime) if icons else "")
PY
)"
APP="$ROOT/dist/RusTerm.app"
CONTENTS="$APP/Contents"

if [[ ! -x "$BINARY" || -z "$ICON_SOURCE" ]]; then
    echo "error: cargo build did not produce the expected binary and AppIcon.icns" >&2
    exit 1
fi

VERSION="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; data=json.load(sys.stdin); print(next(p["version"] for p in data["packages"] if p["name"] == "rusterm-app"))')"

rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources/assets"
cp "$BINARY" "$CONTENTS/MacOS/rusterm"
cp "$ICON_SOURCE" "$CONTENTS/Resources/AppIcon.icns"
cp "$ROOT/assets/gemini-svg.svg" "$CONTENTS/Resources/assets/gemini-svg.svg"

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>RusTerm</string>
    <key>CFBundleExecutable</key>
    <string>rusterm</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon.icns</string>
    <key>CFBundleIdentifier</key>
    <string>com.rusterm.app</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>RusTerm</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

plutil -lint "$CONTENTS/Info.plist" >/dev/null
xattr -cr "$APP"
codesign --force --deep --sign - --timestamp=none "$APP"
"$ROOT/scripts/verify-macos-bundle.sh" "$APP"

echo "Built $APP"
