#!/bin/bash
# macOS UI automation test for cterm
# Launches the app, types commands, takes screenshots, and closes

set -e

CTERM_PATH="${1:-target/debug/cterm}"
OUTPUT_DIR="${2:-test_output}"

# Create output directory
mkdir -p "$OUTPUT_DIR"

LOG_FILE="$OUTPUT_DIR/test.log"

log() {
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    echo "$timestamp - $1" | tee -a "$LOG_FILE"
}

take_screenshot() {
    local name="$1"
    local output_path="$OUTPUT_DIR/${name}.png"

    # Use screencapture to capture the screen
    # -x: no sound, -C: capture cursor
    screencapture -x "$output_path" 2>/dev/null || true

    if [ -f "$output_path" ]; then
        log "Screenshot saved: $output_path"
        return 0
    else
        log "WARNING: Failed to save screenshot: $output_path"
        return 1
    fi
}

send_keys() {
    local text="$1"
    focus_cterm
    # Use AppleScript to send keystrokes to the frontmost application
    osascript -e "tell application \"System Events\" to keystroke \"$text\""
}

focus_cterm() {
    osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $CTERM_PID) to true"
    sleep 0.15
}

send_key() {
    local key="$1"
    local modifiers="${2:-}"

    focus_cterm

    case "$key" in
        "Return"|"return"|"enter")
            osascript -e 'tell application "System Events" to keystroke return'
            ;;
        "Tab"|"tab")
            if [ "$modifiers" = "ctrl" ]; then
                osascript -e 'tell application "System Events" to keystroke tab using control down'
            elif [ "$modifiers" = "ctrl+shift" ]; then
                osascript -e 'tell application "System Events" to keystroke tab using {control down, shift down}'
            else
                osascript -e 'tell application "System Events" to keystroke tab'
            fi
            ;;
        "t")
            if [ "$modifiers" = "cmd" ]; then
                osascript -e 'tell application "System Events" to keystroke "t" using command down'
            else
                osascript -e "tell application \"System Events\" to keystroke \"$key\""
            fi
            ;;
        "w")
            if [ "$modifiers" = "cmd" ]; then
                osascript -e 'tell application "System Events" to keystroke "w" using command down'
            else
                osascript -e "tell application \"System Events\" to keystroke \"$key\""
            fi
            ;;
        "q")
            if [ "$modifiers" = "cmd" ]; then
                osascript -e 'tell application "System Events" to keystroke "q" using command down'
            else
                osascript -e "tell application \"System Events\" to keystroke \"$key\""
            fi
            ;;
        *)
            osascript -e "tell application \"System Events\" to keystroke \"$key\""
            ;;
    esac
}

send_pane_shortcut() {
    local key_code="$1"
    local modifiers="$2"

    # AppleScript keystrokes otherwise follow whichever process most recently
    # took focus. Screenshot capture and asynchronous native-tab creation can
    # temporarily resign cterm on hosted runners.
    focus_cterm

    case "$modifiers" in
        "ctrl+shift")
            osascript -e "tell application \"System Events\" to key code $key_code using {control down, shift down}"
            ;;
        "ctrl+option")
            osascript -e "tell application \"System Events\" to key code $key_code using {control down, option down}"
            ;;
        "ctrl+option+shift")
            osascript -e "tell application \"System Events\" to key code $key_code using {control down, option down, shift down}"
            ;;
        *)
            log "ERROR: Unsupported pane shortcut modifiers: $modifiers"
            exit 1
            ;;
    esac
}

select_pane_menu_item() {
    local item="$1"

    # macOS reserves some Control+Option combinations for system features, so
    # hosted runners may consume directional pane shortcuts before AppKit sees
    # them. Clicking the native menu item still exercises the same production
    # selector and first-responder path without depending on host key settings.
    focus_cterm
    osascript <<EOF
tell application "System Events"
    set ctermProcess to first process whose unix id is $CTERM_PID
    tell ctermProcess
        click menu item "$item" of menu 1 of menu bar item "Pane" of menu bar 1
    end tell
end tell
EOF
}

wait_for_cterm_log() {
    local pattern="$1"
    local description="$2"

    for _attempt in $(seq 1 40); do
        if [ -f "$CTERM_LOG_FILE" ] && grep -Eq "$pattern" "$CTERM_LOG_FILE"; then
            log "$description"
            return 0
        fi
        sleep 0.25
    done

    log "ERROR: Timed out waiting for cterm log pattern: $pattern"
    if [ -f "$CTERM_LOG_FILE" ]; then
        tail -100 "$CTERM_LOG_FILE" | while IFS= read -r line; do log "  $line"; done
    fi
    exit 1
}

cterm_log_line_count() {
    if [ -f "$CTERM_LOG_FILE" ]; then
        wc -l < "$CTERM_LOG_FILE" | tr -d ' '
    else
        echo 0
    fi
}

wait_for_new_cterm_log() {
    local pattern="$1"
    local description="$2"
    local after_line="$3"
    local first_line=$((after_line + 1))

    for _attempt in $(seq 1 40); do
        if [ -f "$CTERM_LOG_FILE" ] &&
            sed -n "${first_line},\$p" "$CTERM_LOG_FILE" | grep -Eq "$pattern"; then
            log "$description"
            return 0
        fi
        sleep 0.25
    done

    log "ERROR: Timed out waiting for new cterm log pattern: $pattern"
    if [ -f "$CTERM_LOG_FILE" ]; then
        sed -n "${first_line},\$p" "$CTERM_LOG_FILE" |
            tail -100 |
            while IFS= read -r line; do log "  $line"; done
    fi
    exit 1
}

wait_for_preferences_window() {
    for _attempt in $(seq 1 40); do
        if osascript -e "tell application \"System Events\" to tell (first process whose unix id is $CTERM_PID) to exists window \"Preferences\"" 2>/dev/null | grep -q true; then
            log "Preferences window opened"
            return 0
        fi
        sleep 0.25
    done

    log "ERROR: Preferences window did not open"
    exit 1
}

save_horizontal_pane_shortcut() {
    local old_shortcut="$1"
    local new_shortcut="$2"

    focus_cterm
    osascript <<EOF
tell application "System Events"
    set ctermProcess to first process whose unix id is $CTERM_PID
    tell ctermProcess
        set preferencesWindow to first window whose name is "Preferences"
        set paneTabPressed to false
        try
            set preferencesTabs to tab group 1 of preferencesWindow
            if exists radio button "Pane Shortcuts" of preferencesTabs then
                click radio button "Pane Shortcuts" of preferencesTabs
            else
                click radio button 4 of preferencesTabs
            end if
            set paneTabPressed to true
        end try
        if paneTabPressed is false then error "Pane Shortcuts tab not found"

        delay 0.25
        set focusedControl to value of attribute "AXFocusedUIElement" of ctermProcess
        if (role of focusedControl as text) is not "AXTextField" then
            key code 48
            delay 0.25
            set focusedControl to value of attribute "AXFocusedUIElement" of ctermProcess
        end if
        if (role of focusedControl as text) is not "AXTextField" then error "Pane shortcut field did not receive keyboard focus"
        if (value of focusedControl as text) is not "$old_shortcut" then error "Unexpected first pane shortcut value"
        set value of focusedControl to "$new_shortcut"

        -- Return invokes the native default OK button, which saves through
        -- the same callback as Apply and then closes Preferences.
        key code 36
    end tell
end tell
EOF
}

write_quick_open_template_fixture() {
    local config_file=""

    for _attempt in $(seq 1 40); do
        config_file=$(find "$CTERM_TEST_HOME" -type f -name config.toml -print -quit 2>/dev/null || true)
        if [ -n "$config_file" ]; then
            break
        fi
        sleep 0.25
    done
    if [ -z "$config_file" ]; then
        log "ERROR: Preferences did not create an isolated config file"
        exit 1
    fi

    local config_directory
    config_directory=$(dirname "$config_file")
    printf '%s\n' \
        '[[tabs]]' \
        'name = "UI Test Template"' \
        'command = "/bin/echo"' \
        'args = ["cterm-template-ui-test"]' \
        'color = "#336699"' \
        'theme = "light"' \
        'background_color = "#f0f0f0"' \
        'keep_open = true' \
        'unique = true' \
        > "$config_directory/sticky_tabs.toml"
    log "Installed isolated Quick Open template fixture"
}

log "=== cterm UI Automation Test (macOS) ==="
log "Executable: $CTERM_PATH"
log "Output: $OUTPUT_DIR"

# Check if executable exists
if [ ! -f "$CTERM_PATH" ]; then
    # Check if it's an app bundle
    if [ -d "$CTERM_PATH" ] && [ -f "$CTERM_PATH/Contents/MacOS/cterm" ]; then
        CTERM_PATH="$CTERM_PATH/Contents/MacOS/cterm"
        log "Using app bundle executable: $CTERM_PATH"
    else
        log "ERROR: cterm not found at $CTERM_PATH"
        exit 1
    fi
fi

# Set up environment
export RUST_LOG=debug
export CTERM_LOG_FILE="$OUTPUT_DIR/cterm.log"
export CTERM_TEST_HOME="$OUTPUT_DIR/home"
mkdir -p "$CTERM_TEST_HOME"

# Start cterm in background
log "Starting cterm..."
HOME="$CTERM_TEST_HOME" "$CTERM_PATH" &
CTERM_PID=$!
log "Process started with PID: $CTERM_PID"

# Wait for window to appear
log "Waiting for window..."
ATTEMPTS=0
MAX_ATTEMPTS=30
WINDOW_FOUND=false

while [ $ATTEMPTS -lt $MAX_ATTEMPTS ]; do
    sleep 0.5

    # Try multiple methods to detect the window
    # Method 1: By PID (may not work without accessibility permissions)
    WINDOW_COUNT=$(osascript -e 'tell application "System Events" to count windows of (processes whose unix id is '$CTERM_PID')' 2>/dev/null) || WINDOW_COUNT=0

    # Method 2: By process name "cterm"
    if [ "$WINDOW_COUNT" -eq 0 ]; then
        WINDOW_COUNT=$(osascript -e 'tell application "System Events" to count windows of (processes whose name is "cterm")' 2>/dev/null) || WINDOW_COUNT=0
    fi

    if [ "$WINDOW_COUNT" -gt 0 ]; then
        WINDOW_FOUND=true
        break
    fi

    ATTEMPTS=$((ATTEMPTS + 1))
    if [ $((ATTEMPTS % 5)) -eq 0 ]; then
        log "  Attempt $ATTEMPTS/$MAX_ATTEMPTS..."
    fi
done

# Even if window detection failed, check if process is running and try to proceed
if [ "$WINDOW_FOUND" != "true" ]; then
    if kill -0 $CTERM_PID 2>/dev/null; then
        log "Window detection failed but process is running - trying to proceed anyway"
        # Wait a bit more for window to be ready
        sleep 2
    else
        log "ERROR: Window not found and process is not running"
        take_screenshot "error_no_window"

        # Show cterm log if exists
        if [ -f "$CTERM_LOG_FILE" ]; then
            log "cterm log contents:"
            cat "$CTERM_LOG_FILE" | while read line; do log "  $line"; done
        fi

        exit 1
    fi
else
    log "Window found"
fi

# Activate cterm window
log "Activating cterm window..."
# Try by PID first, then by name
osascript -e "tell application \"System Events\"
    try
        set frontmost of (first process whose unix id is $CTERM_PID) to true
    on error
        set frontmost of (first process whose name is \"cterm\") to true
    end try
end tell" 2>/dev/null || true
sleep 1

# Take initial screenshot
take_screenshot "01_startup"

# Type command
log "Typing 'echo hello world'..."
send_keys "echo hello world"
sleep 0.5

# Take screenshot after typing
take_screenshot "02_after_typing"

# Press Enter
log "Pressing Enter..."
send_key "Return"
sleep 1

# Take screenshot after command execution
take_screenshot "03_after_enter"

# A screenshot artifact alone only proves that screencapture ran. Requiring the
# terminal window to change after input catches frozen/native-renderer failures.
if cmp -s "$OUTPUT_DIR/01_startup.png" "$OUTPUT_DIR/03_after_enter.png"; then
    log "ERROR: Terminal screenshot did not change after command input"
    exit 1
fi
log "Renderer produced a changed frame after command input"

# Type another command
log "Typing 'ls -la'..."
send_keys "ls -la"
sleep 0.5
send_key "Return"
sleep 1

# Take screenshot after ls
take_screenshot "04_after_ls"

# Test Cmd+T for new tab
log "Testing Cmd+T (new tab)..."
send_key "t" "cmd"
sleep 1

# Take screenshot showing tabs
take_screenshot "05_new_tab"

# Exercise the native AppKit pane host. Key codes avoid keyboard-layout
# ambiguity on the hosted runner (42=backslash, 27=minus, 36=return,
# 117=forward delete).
log "Testing native horizontal pane split..."
send_pane_shortcut 42 "ctrl+shift"
wait_for_cterm_log 'Split pane Horizontal' "Horizontal pane split succeeded"

log "Testing native vertical pane split..."
send_pane_shortcut 27 "ctrl+shift"
wait_for_cterm_log 'Split pane Vertical' "Vertical pane split succeeded"
take_screenshot "06_split_panes"

log "Testing directional pane focus..."
select_pane_menu_item "Focus Up"
wait_for_cterm_log 'Focused pane .*Up' "Directional pane focus succeeded"

log "Testing pane divider resize..."
select_pane_menu_item "Resize Down"
wait_for_cterm_log 'Resized pane .*Down' "Pane divider resize succeeded"

log "Testing pane zoom and unzoom..."
send_pane_shortcut 36 "ctrl+shift"
wait_for_cterm_log 'Pane zoom true' "Pane zoom succeeded"
send_pane_shortcut 36 "ctrl+shift"
wait_for_cterm_log 'Pane zoom false' "Pane unzoom succeeded"

log "Testing pane close..."
send_pane_shortcut 117 "ctrl+shift"
wait_for_cterm_log 'Closed pane' "Pane close succeeded"
take_screenshot "07_closed_pane"

# Preferences must replace ShortcutManager instances already owned by every
# open terminal view, including panes that were created before the dialog.
log "Testing live shortcut reload through Preferences save..."
focus_cterm
osascript -e 'tell application "System Events" to keystroke "," using command down'
wait_for_preferences_window
PREFERENCES_LOG_MARK=$(cterm_log_line_count)
save_horizontal_pane_shortcut "Ctrl+Shift+Backslash" "Ctrl+Shift+P"
wait_for_new_cterm_log \
    'Preferences saved; refreshed shortcuts for [1-9][0-9]* window\(s\) and ([2-9]|[1-9][0-9]+) terminal view\(s\)' \
    "Preferences save refreshed every pre-existing window and split pane" \
    "$PREFERENCES_LOG_MARK"
take_screenshot "08_preferences_saved"

# Focus a pane that was inactive while Preferences was open.
FOCUS_LOG_MARK=$(cterm_log_line_count)
select_pane_menu_item "Focus Left"
wait_for_new_cterm_log 'Focused pane .*Left' \
    "Focused a pre-existing inactive pane after Preferences Apply" \
    "$FOCUS_LOG_MARK"

CUSTOM_SPLIT_LOG_MARK=$(cterm_log_line_count)
send_pane_shortcut 35 "ctrl+shift"
wait_for_new_cterm_log 'Split pane Horizontal' \
    "Updated Ctrl+Shift+P binding split the pre-existing pane" \
    "$CUSTOM_SPLIT_LOG_MARK"
take_screenshot "09_reloaded_shortcut"

# Quick Open reloads templates when shown, so an isolated local fixture can
# exercise its real callback and the shared Cocoa launch plan without network
# access, Docker, or host configuration.
write_quick_open_template_fixture
QUICK_OPEN_LOG_MARK=$(cterm_log_line_count)
log "Testing template launch through Quick Open..."
focus_cterm
osascript -e 'tell application "System Events" to keystroke "g" using command down'
sleep 0.5
send_keys "UI Test Template"
send_key "Return"
wait_for_new_cterm_log 'Created daemon tab: UI Test Template' \
    "Quick Open launched the isolated local template" \
    "$QUICK_OPEN_LOG_MARK"
take_screenshot "10_quick_open_template"

# Close the window
log "Closing window..."
send_key "q" "cmd"

# Wait for process to exit
sleep 2
if kill -0 $CTERM_PID 2>/dev/null; then
    log "Process did not exit gracefully, killing..."
    kill $CTERM_PID 2>/dev/null || true
    sleep 1
    kill -9 $CTERM_PID 2>/dev/null || true
fi

# Copy cterm log
if [ -f "$CTERM_LOG_FILE" ]; then
    log ""
    log "=== cterm application log ==="
    cat "$CTERM_LOG_FILE" | while read line; do log "$line"; done
fi

log ""
log "=== Test completed ==="
log "Screenshots saved to: $OUTPUT_DIR"
ls -la "$OUTPUT_DIR"/*.png 2>/dev/null | while read line; do log "  $line"; done

exit 0
