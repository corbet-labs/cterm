#!/usr/bin/env bash

set -euo pipefail

TAG_NAME="${1:-${GITHUB_REF_NAME:-}}"

if [[ -z "$TAG_NAME" ]]; then
    echo "release metadata check: a tag name is required" >&2
    exit 1
fi

WORKSPACE_VERSION="$({
    awk '
        /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
        /^\[/ { in_workspace_package = 0 }
        in_workspace_package && /^version = "/ {
            line = $0
            sub(/^version = "/, "", line)
            sub(/".*/, "", line)
            print line
            exit
        }
    ' Cargo.toml
})"

if [[ -z "$WORKSPACE_VERSION" ]]; then
    echo "release metadata check: workspace version is missing from Cargo.toml" >&2
    exit 1
fi

EXPECTED_TAG="v${WORKSPACE_VERSION}"
if [[ "$TAG_NAME" != "$EXPECTED_TAG" ]]; then
    echo "release metadata check: tag '$TAG_NAME' does not match workspace version '$EXPECTED_TAG'" >&2
    exit 1
fi

CHANGELOG_SECTION_COUNT="$({
    awk -v version="$WORKSPACE_VERSION" '
        BEGIN { prefix = "## [" version "]" }
        index($0, prefix) == 1 {
            suffix = substr($0, length(prefix) + 1)
            if (suffix == "" || suffix ~ /^ - [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) {
                count++
            }
        }
        END { print count + 0 }
    ' CHANGELOG.md
})"

if [[ "$CHANGELOG_SECTION_COUNT" != "1" ]]; then
    echo "release metadata check: expected exactly one '## [$WORKSPACE_VERSION]' CHANGELOG section, found $CHANGELOG_SECTION_COUNT" >&2
    exit 1
fi

if ! cargo metadata --locked --no-deps --format-version 1 >/dev/null; then
    echo "release metadata check: Cargo.lock does not match the release manifests" >&2
    exit 1
fi

echo "release metadata check: $TAG_NAME matches Cargo.toml, Cargo.lock, and CHANGELOG.md"
