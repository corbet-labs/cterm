#!/usr/bin/env bash

set -euo pipefail

VERIFY_TMP="$(mktemp -d)"
MOUNT_POINT=""
DMG_MOUNTED=false

cleanup() {
    if [[ "$DMG_MOUNTED" == true ]]; then
        hdiutil detach "$MOUNT_POINT" -quiet || true
    fi
    rm -rf "$VERIFY_TMP"
}
trap cleanup EXIT

fail() {
    echo "release package contract: $*" >&2
    exit 1
}

require_file() {
    local path="$1"
    [[ -s "$path" ]] || fail "missing or empty file: $path"
}

require_executable() {
    local path="$1"
    require_file "$path"
    [[ -x "$path" ]] || fail "file is not executable: $path"
}

verify_licenses() {
    local root="$1"
    require_file "$root/LICENSE"
    require_file "$root/THIRD_PARTY_LICENSES.md"
    require_file "$root/LICENSES/KARPELESLAB-CTERM-MIT.txt"
}

verify_client_directory() {
    local root="$1"
    require_executable "$root/cterm"
    require_executable "$root/ctermd"
    require_executable "$root/cterm-plugin-host"
    require_file "$root/README.md"
    verify_licenses "$root"
}

verify_daemon_directory() {
    local root="$1"
    require_executable "$root/ctermd"
    require_file "$root/README.md"
    verify_licenses "$root"
}

verify_macos_app() {
    local app="$1"
    require_executable "$app/Contents/MacOS/cterm"
    require_executable "$app/Contents/MacOS/ctermd"
    require_executable "$app/Contents/MacOS/cterm-plugin-host"
    require_file "$app/Contents/Info.plist"
    require_file "$app/Contents/Resources/LICENSE"
    require_file "$app/Contents/Resources/THIRD_PARTY_LICENSES.md"
    require_file "$app/Contents/Resources/LICENSES/KARPELESLAB-CTERM-MIT.txt"
    codesign --verify --deep --strict --verbose=2 "$app"
}

MODE="${1:-}"
ARCHIVE="${2:-}"
ROOT_NAME="${3:-}"

[[ -n "$MODE" && -n "$ARCHIVE" ]] || fail "usage: $0 MODE ARCHIVE [ROOT_NAME]"
require_file "$ARCHIVE"

case "$MODE" in
    client-tar)
        [[ -n "$ROOT_NAME" ]] || fail "client-tar requires the archive root name"
        tar -xzf "$ARCHIVE" -C "$VERIFY_TMP"
        verify_client_directory "$VERIFY_TMP/$ROOT_NAME"
        ;;
    daemon-tar)
        [[ -n "$ROOT_NAME" ]] || fail "daemon-tar requires the archive root name"
        tar -xzf "$ARCHIVE" -C "$VERIFY_TMP"
        verify_daemon_directory "$VERIFY_TMP/$ROOT_NAME"
        ;;
    macos-app-tar)
        tar -xzf "$ARCHIVE" -C "$VERIFY_TMP"
        verify_macos_app "$VERIFY_TMP/cterm.app"
        ;;
    macos-dmg)
        command -v hdiutil >/dev/null || fail "hdiutil is required to verify a DMG"
        MOUNT_POINT="$VERIFY_TMP/dmg"
        mkdir -p "$MOUNT_POINT"
        hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT_POINT" "$ARCHIVE" -quiet
        DMG_MOUNTED=true
        verify_macos_app "$MOUNT_POINT/cterm.app"
        hdiutil detach "$MOUNT_POINT" -quiet
        DMG_MOUNTED=false
        MOUNT_POINT=""
        ;;
    *)
        fail "unknown verification mode: $MODE"
        ;;
esac

echo "release package contract: $ARCHIVE is complete"
