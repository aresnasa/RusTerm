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

env \
    HOME="$HOME_DIR" \
    RUSTERM_CONFIG_DIR="$CONFIG_DIR" \
    RUSTERM_E2E_SCRIPT_PATH="$ROOT_DIR/scripts/native-restore-smoke.js" \
    RUST_LOG=rusterm_ui=info \
    "$ROOT_DIR/target/debug/rusterm"
