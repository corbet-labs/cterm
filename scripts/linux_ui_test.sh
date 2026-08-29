#!/usr/bin/env bash
# Wayland-only startup smoke test for cterm's GTK backend.

set -euo pipefail

CTERM_PATH="${1:-target/debug/cterm}"
OUTPUT_DIR="${2:-test_output}"

mkdir -p "$OUTPUT_DIR"
LOG_FILE="$OUTPUT_DIR/test.log"

log() {
    printf '%s - %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" | tee -a "$LOG_FILE"
}

log "=== cterm Wayland startup smoke test ==="
log "Executable: $CTERM_PATH"
log "Output: $OUTPUT_DIR"

if [ ! -x "$CTERM_PATH" ]; then
    log "ERROR: cterm not found at $CTERM_PATH"
    exit 1
fi

if [ -n "${DISPLAY:-}" ]; then
    log "ERROR: DISPLAY is set; Linux CI must not fall back to X11"
    exit 1
fi

if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    log "ERROR: XDG_RUNTIME_DIR and WAYLAND_DISPLAY must identify the test compositor"
    exit 1
fi

WAYLAND_SOCKET="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
for attempt in $(seq 1 40); do
    if [ -S "$WAYLAND_SOCKET" ]; then
        break
    fi
    if [ "$attempt" -eq 40 ]; then
        log "ERROR: Wayland compositor socket did not appear: $WAYLAND_SOCKET"
        exit 1
    fi
    sleep 0.25
done

log "Wayland compositor socket is ready"

export RUST_LOG=debug
export CTERM_LOG_FILE="$OUTPUT_DIR/cterm.log"
export WAYLAND_DEBUG=client

log "Starting cterm..."
"$CTERM_PATH" >"$OUTPUT_DIR/cterm.stdout.log" 2>"$OUTPUT_DIR/cterm.stderr.log" &
CTERM_PID=$!
log "Process started with PID: $CTERM_PID"

cleanup() {
    if kill -0 "$CTERM_PID" 2>/dev/null; then
        kill "$CTERM_PID" 2>/dev/null || true
        wait "$CTERM_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

for _attempt in $(seq 1 30); do
    if ! kill -0 "$CTERM_PID" 2>/dev/null; then
        wait "$CTERM_PID" || exit_code=$?
        log "ERROR: cterm exited during startup with code ${exit_code:-0}"
        exit 1
    fi

    if grep -q 'wl_compositor.*create_surface' "$OUTPUT_DIR/cterm.stderr.log"; then
        log "cterm created a native Wayland surface"
        log "=== Test completed ==="
        exit 0
    fi
    sleep 0.5
done

log "ERROR: cterm stayed alive but did not create a Wayland surface"
exit 1
