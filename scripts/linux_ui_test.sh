#!/usr/bin/env bash
# Wayland-only pane and template action test for cterm's GTK backend.

set -euo pipefail

CTERM_PATH="${1:-target/debug/cterm}"
OUTPUT_DIR="${2:-test_output}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd -P)"
if [[ "$CTERM_PATH" != /* ]]; then
    CTERM_PATH="$(cd "$(dirname "$CTERM_PATH")" && pwd -P)/$(basename "$CTERM_PATH")"
fi
LOG_FILE="$OUTPUT_DIR/test.log"

log() {
    printf '%s - %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" | tee -a "$LOG_FILE"
}

log "=== cterm Wayland pane and template action test ==="
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

toml_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '%s' "$value"
}

TEMPLATE_NAME="GTK CI Template"
TEMPLATE_COMMAND="$OUTPUT_DIR/template-command.sh"
TEMPLATE_READY="$OUTPUT_DIR/template.ready"
TEMPLATE_VISIBLE="$OUTPUT_DIR/template.visible"
TEMPLATE_DONE="$OUTPUT_DIR/template.done"
TEMPLATE_WORKSPACE="$OUTPUT_DIR/template-workspace"
TEMPLATE_MARKER="CTERM_TEMPLATE_OK <alpha beta>|<gamma>|plan-env|config-env"
CONFIG_ROOT="$OUTPUT_DIR/xdg-config"
CONFIG_DIR="$CONFIG_ROOT/cterm"
mkdir -p "$CONFIG_DIR"
rm -f "$TEMPLATE_READY" "$TEMPLATE_VISIBLE" "$TEMPLATE_DONE"

cat >"$TEMPLATE_COMMAND" <<'EOF'
#!/bin/sh
attempt=0
while [ ! -f "$CTERM_TEMPLATE_READY" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 200 ]; then
        exit 91
    fi
    sleep 0.05
done
printf 'CTERM_TEMPLATE_OK <%s>|<%s>|%s|%s\n' \
    "$1" "$2" "$CTERM_TEMPLATE_ENV" "$CTERM_CONFIG_ENV"
attempt=0
while [ ! -f "$CTERM_TEMPLATE_VISIBLE" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 200 ]; then
        exit 92
    fi
    sleep 0.05
done
pwd >"$CTERM_TEMPLATE_DONE"
exit 23
EOF
chmod +x "$TEMPLATE_COMMAND"

cat >"$CONFIG_DIR/config.toml" <<'EOF'
[general]
term = "cterm-ci"

[general.env]
CTERM_CONFIG_ENV = "config-env"
EOF

cat >"$CONFIG_DIR/sticky_tabs.toml" <<EOF
[[tabs]]
name = "$TEMPLATE_NAME"
command = "$(toml_escape "$TEMPLATE_COMMAND")"
args = ["alpha beta", "gamma"]
working_directory = "$(toml_escape "$TEMPLATE_WORKSPACE")"
color = "#2a7fff"
theme = "nord"
background_color = "#102030"
keep_open = true
unique = true

[tabs.env]
CTERM_TEMPLATE_ENV = "plan-env"
CTERM_TEMPLATE_READY = "$(toml_escape "$TEMPLATE_READY")"
CTERM_TEMPLATE_VISIBLE = "$(toml_escape "$TEMPLATE_VISIBLE")"
CTERM_TEMPLATE_DONE = "$(toml_escape "$TEMPLATE_DONE")"
EOF

export XDG_CONFIG_HOME="$CONFIG_ROOT"
export CTERM_WAYLAND_TEMPLATE_CI_NAME="$TEMPLATE_NAME"
export CTERM_WAYLAND_TEMPLATE_CI_MARKER="$TEMPLATE_MARKER"
export CTERM_WAYLAND_TEMPLATE_CI_READY="$TEMPLATE_READY"
export CTERM_WAYLAND_TEMPLATE_CI_VISIBLE="$TEMPLATE_VISIBLE"
export CTERM_WAYLAND_TEMPLATE_CI_DONE="$TEMPLATE_DONE"
export CTERM_WAYLAND_TEMPLATE_CI_WORKSPACE="$TEMPLATE_WORKSPACE"

log "Prepared isolated Quick Open template fixture"

export XDG_DATA_HOME="$OUTPUT_DIR/xdg-data"
PLUGIN_DIR="$XDG_DATA_HOME/cterm/plugins/org.example.ui"
PLUGIN_GRANT="$XDG_DATA_HOME/cterm/plugin-grants.toml"
mkdir -p "$PLUGIN_DIR"
cat >"$PLUGIN_DIR/cterm-plugin.toml" <<'EOF'
manifest_version = 1
id = "org.example.ui"
name = "UI Test Plugin"
version = "1.0.0"
abi = "1.0"

[[commands]]
id = "new-tab"
title = "Open Test Tab"

[capabilities.invoke-actions]
allow = ["cterm:new-tab"]
EOF
base64 -d \
    <"$SCRIPT_DIR/../crates/cterm-plugin-host/tests/fixtures/ui_new_tab.wasm.base64" \
    >"$PLUGIN_DIR/plugin.wasm"
export CTERM_WAYLAND_PLUGIN_CI_ACTION_ID="plugin:org.example.ui/new-tab"

log "Prepared isolated native plugin fixture"

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
for _attempt in $(seq 1 240); do
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
    "CTERM_TEMPLATE_CI INGRESS_OK source=quick-open"
    "CTERM_TEMPLATE_CI LAUNCH_OK argv=visible cwd=prepared keep_open=true color=#2a7fff"
    "CTERM_TEMPLATE_CI UNIQUE_OK tabs=2 session=reused"
    "CTERM_PLUGIN_CI PROMPT_OK backend=wayland"
    "CTERM_PLUGIN_CI EXECUTION_OK action=cterm:new-tab"
    "CTERM_PANE_CI COMPLETE"
)
for marker in "${required_markers[@]}"; do
    if ! grep -Fq "$marker" \
        "$OUTPUT_DIR/cterm.stderr.log" "$OUTPUT_DIR/cterm.log" 2>/dev/null; then
        log "ERROR: missing pane action marker: $marker"
        exit 1
    fi
done

if [ ! -s "$PLUGIN_GRANT" ] || ! grep -Fq 'plugin = "org.example.ui"' "$PLUGIN_GRANT"; then
    log "ERROR: accepted native plugin prompt did not persist its exact local grant"
    exit 1
fi

if grep -Eqi 'panic|Gdk-CRITICAL|Gtk-ERROR|segmentation fault|CTERM_PANE_CI FAIL' \
    "$OUTPUT_DIR/cterm.stderr.log" "$OUTPUT_DIR/cterm.log" 2>/dev/null; then
    log "ERROR: cterm reported a fatal Wayland or pane diagnostic"
    exit 1
fi

if [ ! -f "$TEMPLATE_DONE" ]; then
    log "ERROR: template command did not leave completion evidence"
    exit 1
fi

if [ "$(tr -d '\r\n' <"$TEMPLATE_DONE")" != "$TEMPLATE_WORKSPACE" ]; then
    log "ERROR: template command did not run in the prepared workspace"
    exit 1
fi

log "cterm created a native Wayland surface and completed pane/template actions"
log "=== Test completed ==="
