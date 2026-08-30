#!/usr/bin/env bash
# Compare PTY protocol replies from cterm and foot under the same Wayland compositor.

set -euo pipefail

CTERM_PATH="${1:?cterm executable is required}"
CTERMD_PATH="${2:?ctermd executable is required}"
PROBE_PATH="${3:?compatibility probe executable is required}"
OUTPUT_DIR="${4:-test_output}"

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
CTERM_PATH=$(realpath "$CTERM_PATH")
CTERMD_PATH=$(realpath "$CTERMD_PATH")
PROBE_PATH=$(realpath "$PROBE_PATH")

for executable in "$CTERM_PATH" "$CTERMD_PATH" "$PROBE_PATH"; do
    if [ ! -x "$executable" ]; then
        printf 'ERROR: executable not found: %s\n' "$executable" >&2
        exit 1
    fi
done
if ! command -v foot >/dev/null 2>&1; then
    printf 'ERROR: foot is required for the differential compatibility gate\n' >&2
    exit 1
fi
if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    printf 'ERROR: a Wayland compositor is required\n' >&2
    exit 1
fi

SOURCE_WAYLAND_SOCKET="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
for attempt in $(seq 1 40); do
    if [ -S "$SOURCE_WAYLAND_SOCKET" ]; then
        break
    fi
    if [ "$attempt" -eq 40 ]; then
        printf 'ERROR: Wayland socket not found: %s\n' "$SOURCE_WAYLAND_SOCKET" >&2
        exit 1
    fi
    sleep 0.25
done

ISOLATED_RUNTIME="$OUTPUT_DIR/runtime"
mkdir -p "$ISOLATED_RUNTIME"
chmod 700 "$ISOLATED_RUNTIME"
ln -s "$SOURCE_WAYLAND_SOCKET" "$ISOLATED_RUNTIME/$WAYLAND_DISPLAY"
export XDG_RUNTIME_DIR="$ISOLATED_RUNTIME"
export XDG_CONFIG_HOME="$OUTPUT_DIR/config"
export GDK_BACKEND=wayland
export GSK_RENDERER=cairo
unset DISPLAY

app_pid=
cleanup() {
    if [ -n "$app_pid" ] && kill -0 "$app_pid" 2>/dev/null; then
        kill "$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
    fi
    local daemon_pid_file="$ISOLATED_RUNTIME/cterm/ctermd.pid"
    if [ -f "$daemon_pid_file" ]; then
        local daemon_pid
        daemon_pid=$(tr -cd '0-9' < "$daemon_pid_file")
        if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
            kill "$daemon_pid" 2>/dev/null || true
        fi
    fi
}
trap cleanup EXIT

run_probe() {
    local terminal=$1
    local report="$OUTPUT_DIR/$terminal.report"
    local stdout_log="$OUTPUT_DIR/$terminal.stdout.log"
    local stderr_log="$OUTPUT_DIR/$terminal.stderr.log"

    if [ "$terminal" = foot ]; then
        foot --config=/dev/null --window-size-chars=80x24 \
            "$PROBE_PATH" "$report" >"$stdout_log" 2>"$stderr_log" &
    else
        PATH="$(dirname "$CTERMD_PATH"):$PATH" \
            "$CTERM_PATH" -e "$PROBE_PATH" -- "$report" \
            >"$stdout_log" 2>"$stderr_log" &
    fi
    app_pid=$!

    for _attempt in $(seq 1 100); do
        if [ -s "$report" ]; then
            break
        fi
        if ! kill -0 "$app_pid" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done

    if [ ! -s "$report" ]; then
        printf 'ERROR: %s did not complete the compatibility probe\n' "$terminal" >&2
        tail -100 "$stderr_log" >&2 || true
        exit 1
    fi

    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
    app_pid=
}

run_probe foot
run_probe cterm

if ! diff -u "$OUTPUT_DIR/foot.report" "$OUTPUT_DIR/cterm.report" \
    >"$OUTPUT_DIR/difference.diff"; then
    printf 'ERROR: cterm PTY replies differ from foot\n' >&2
    cat "$OUTPUT_DIR/difference.diff" >&2
    exit 1
fi

printf 'cterm and foot produced identical PTY compatibility reports:\n'
cat "$OUTPUT_DIR/cterm.report"
