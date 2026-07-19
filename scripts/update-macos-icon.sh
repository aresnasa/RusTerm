#!/usr/bin/env bash
set -euo pipefail

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

cd "$ROOT"
cargo build -p rusterm-app

ICON_SOURCE="$(python3 - "$TARGET_DIR/debug/build" <<'PY'
import glob
import os
import sys

icons = glob.glob(os.path.join(sys.argv[1], "rusterm-app-*", "out", "AppIcon.icns"))
print(max(icons, key=os.path.getmtime) if icons else "")
PY
)"
if [[ -z "$ICON_SOURCE" ]]; then
    echo "error: build completed without producing AppIcon.icns" >&2
    exit 1
fi

cp "$ICON_SOURCE" "$ROOT/assets/AppIcon.icns"
file "$ROOT/assets/AppIcon.icns"
echo "Updated assets/AppIcon.icns from assets/gemini-svg.svg"
