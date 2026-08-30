#!/usr/bin/env bash
# Wayland-only pane action test for cterm's GTK backend.

set -euo pipefail

CTERM_PATH="${1:-target/debug/cterm}"
OUTPUT_DIR="${2:-test_output}"

mkdir -p "$OUTPUT_DIR"
LOG_FILE="$OUTPUT_DIR/test.log"

log() {
    printf '%s - %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" | tee -a "$LOG_FILE"
}

log "=== cterm Wayland pane action test ==="
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
export CTERM_WAYLAND_PANE_CI=1

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

surface_ready=0
for _attempt in $(seq 1 120); do
    if grep -q 'wl_compositor.*create_surface' "$OUTPUT_DIR/cterm.stderr.log"; then
        surface_ready=1
    fi
    if ! kill -0 "$CTERM_PID" 2>/dev/null; then
        break
    fi
    sleep 0.25
done

if kill -0 "$CTERM_PID" 2>/dev/null; then
    log "ERROR: cterm did not finish the pane action sequence"
    exit 1
fi

set +e
wait "$CTERM_PID"
exit_code=$?
set -e
if [ "$exit_code" -ne 0 ]; then
    log "ERROR: cterm pane driver exited with code $exit_code"
    exit 1
fi

if [ "$surface_ready" -ne 1 ]; then
    log "ERROR: cterm did not create a native Wayland surface"
    exit 1
fi

required_markers=(
    "CTERM_PANE_CI START backend=wayland"
    "CTERM_PANE_CI READY panes=1"
    "CTERM_PANE_CI SPLIT_HORIZONTAL_OK panes=2"
    "CTERM_PANE_CI SPLIT_VERTICAL_OK panes=3"
    "CTERM_PANE_CI FOCUS_OK direction=up"
    "CTERM_PANE_CI RESIZE_OK direction=left"
    "CTERM_PANE_CI ZOOM_OK"
    "CTERM_PANE_CI UNZOOM_OK"
    "CTERM_PANE_CI CLOSE_OK panes=2"
    "CTERM_PANE_CI COMPLETE"
)
for marker in "${required_markers[@]}"; do
    if ! grep -Fq "$marker" \
        "$OUTPUT_DIR/cterm.stderr.log" "$OUTPUT_DIR/cterm.log" 2>/dev/null; then
        log "ERROR: missing pane action marker: $marker"
        exit 1
    fi
done

if grep -Eqi 'panic|Gdk-CRITICAL|Gtk-ERROR|segmentation fault|CTERM_PANE_CI FAIL' \
    "$OUTPUT_DIR/cterm.stderr.log" "$OUTPUT_DIR/cterm.log" 2>/dev/null; then
    log "ERROR: cterm reported a fatal Wayland or pane diagnostic"
    exit 1
fi

log "cterm created a native Wayland surface and completed every pane action"
log "=== Test completed ==="
