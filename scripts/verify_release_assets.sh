#!/usr/bin/env bash

set -euo pipefail

ARTIFACT_ROOT="${1:-artifacts}"
ASSETS=(
    "cterm-linux-x86_64/cterm-linux-x86_64.tar.gz"
    "cterm-linux-arm64/cterm-linux-arm64.tar.gz"
    "cterm-windows-x86_64/cterm-windows-x86_64.zip"
    "cterm-windows-installer/cterm-windows-x86_64-setup.exe"
    "cterm-macos-universal/cterm-macos-universal.tar.gz"
    "cterm-macos-dmg/cterm-macos-universal.dmg"
    "ctermd-linux-x86_64/ctermd-linux-x86_64.tar.gz"
    "ctermd-linux-arm64/ctermd-linux-arm64.tar.gz"
    "ctermd-windows-x86_64/ctermd-windows-x86_64.zip"
    "ctermd-macos-universal/ctermd-macos-universal.tar.gz"
)

for relative_path in "${ASSETS[@]}"; do
    asset="$ARTIFACT_ROOT/$relative_path"
    sidecar="$asset.sha256"
    if [[ ! -s "$asset" ]]; then
        echo "release asset contract: missing or empty asset $asset" >&2
        exit 1
    fi
    if [[ ! -s "$sidecar" ]]; then
        echo "release asset contract: missing or empty checksum $sidecar" >&2
        exit 1
    fi

    asset_directory="$(dirname "$asset")"
    sidecar_name="$(basename "$sidecar")"
    (cd "$asset_directory" && sha256sum --check "$sidecar_name")
done

echo "release asset contract: verified ${#ASSETS[@]} assets and checksum sidecars"
