#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d)
CONFIG_DIR="$TEST_ROOT/config"
HOME_DIR="$TEST_ROOT/home"
mkdir -p "$CONFIG_DIR" "$HOME_DIR"

cleanup() {
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT INT TERM

cd "$ROOT_DIR"
cargo build -p rusterm-app

env \
    HOME="$HOME_DIR" \
    RUSTERM_CONFIG_DIR="$CONFIG_DIR" \
    RUSTERM_E2E_SCRIPT_PATH="$ROOT_DIR/scripts/native-smoke.js" \
    RUST_LOG=rusterm_ui=info \
    "$ROOT_DIR/target/debug/rusterm"

DB_PATH="$HOME_DIR/Library/Application Support/rusterm/rusterm.db"
test -f "$DB_PATH"
SUCCESS_COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM history WHERE command IN ('printf RUSTERM_MAIN_E2E_OK', 'printf RUSTERM_AFTER_CANCEL_OK', 'printf RUSTERM_BOTTOM_E2E_OK') AND exit_code = 0;")
DANGEROUS_COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM history WHERE command = 'mkfs.ext4 /dev/sda';")
[ "$SUCCESS_COUNT" = "3" ]
[ "$DANGEROUS_COUNT" = "0" ]

env \
    HOME="$HOME_DIR" \
    RUSTERM_CONFIG_DIR="$CONFIG_DIR" \
    RUSTERM_E2E_SCRIPT_PATH="$ROOT_DIR/scripts/native-restore-smoke.js" \
    RUST_LOG=rusterm_ui=info \
    "$ROOT_DIR/target/debug/rusterm"
