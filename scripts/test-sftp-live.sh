#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ORIGINAL_CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
ORIGINAL_RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
IMAGE_NAME=rusterm-sftp-live-test
CONTAINER_NAME="rusterm-sftp-live-$$"
EMPTY_HOME=$(mktemp -d)

cleanup() {
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
    rm -rf "$EMPTY_HOME"
}
trap cleanup EXIT INT TERM

docker build \
    --file "$ROOT_DIR/scripts/sftp-test.Dockerfile" \
    --tag "$IMAGE_NAME" \
    "$ROOT_DIR"

docker run --detach --rm \
    --name "$CONTAINER_NAME" \
    --publish 127.0.0.1::22 \
    "$IMAGE_NAME" >/dev/null

PORT=$(docker port "$CONTAINER_NAME" 22/tcp | sed 's/.*://')
ATTEMPT=0
until docker exec "$CONTAINER_NAME" sh -c 'test -f /etc/ssh/ssh_host_ed25519_key'; do
    ATTEMPT=$((ATTEMPT + 1))
    if [ "$ATTEMPT" -ge 100 ]; then
        echo "SSH/SFTP test server did not become ready" >&2
        docker logs "$CONTAINER_NAME" >&2 || true
        exit 1
    fi
    sleep 0.1
done

docker exec --user rusterm-test "$CONTAINER_NAME" sh -c \
    'printf "symlink target\n" > /home/rusterm-test/work/symlink-target.txt && ln -s symlink-target.txt /home/rusterm-test/work/symlink-link'

cd "$ROOT_DIR"
env \
    HOME="$EMPTY_HOME" \
    CARGO_HOME="$ORIGINAL_CARGO_HOME" \
    RUSTUP_HOME="$ORIGINAL_RUSTUP_HOME" \
    SSH_AUTH_SOCK= \
    RUSTERM_SFTP_HOST=127.0.0.1 \
    RUSTERM_SFTP_PORT="$PORT" \
    RUSTERM_SFTP_USER=rusterm-test \
    RUSTERM_SFTP_PASSWORD=rusterm-test-only \
    RUSTERM_SFTP_TEST_DIR=/home/rusterm-test/work \
    RUSTERM_SFTP_SYMLINK_PATH=/home/rusterm-test/work/symlink-link \
    RUSTERM_SFTP_SYMLINK_TARGET=/home/rusterm-test/work/symlink-target.txt \
    cargo test -p rusterm-ssh --test sftp_live live_sftp_round_trip -- --ignored --nocapture
