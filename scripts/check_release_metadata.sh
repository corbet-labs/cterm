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

if ! command -v jq >/dev/null 2>&1; then
    echo "release metadata check: jq is required to inspect Cargo workspace metadata" >&2
    exit 1
fi

if ! WORKSPACE_METADATA="$(cargo metadata --locked --no-deps --format-version 1)"; then
    echo "release metadata check: Cargo.lock does not match the release manifests" >&2
    exit 1
fi

WORKSPACE_PACKAGE_COUNT="$({
    jq -er '.workspace_members | length' <<<"$WORKSPACE_METADATA"
})"
DISCOVERED_PACKAGE_COUNT="$({
    jq -er '
        .workspace_members as $members
        | [.packages[] | select(.id as $id | $members | index($id))]
        | length
    ' <<<"$WORKSPACE_METADATA"
})"

if [[ "$DISCOVERED_PACKAGE_COUNT" != "$WORKSPACE_PACKAGE_COUNT" ]]; then
    echo "release metadata check: cargo metadata described $DISCOVERED_PACKAGE_COUNT of $WORKSPACE_PACKAGE_COUNT workspace packages" >&2
    exit 1
fi

VERSION_MISMATCHES="$({
    jq -r --arg expected "$WORKSPACE_VERSION" '
        .workspace_members as $members
        | .packages[]
        | select(.id as $id | $members | index($id))
        | select(.version != $expected)
        | "\(.name)@\(.version)"
    ' <<<"$WORKSPACE_METADATA"
})"

if [[ -n "$VERSION_MISMATCHES" ]]; then
    echo "release metadata check: workspace packages do not match version '$WORKSPACE_VERSION':" >&2
    printf '%s\n' "$VERSION_MISMATCHES" >&2
    exit 1
fi

WORKSPACE_PACKAGE_NAMES="$({
    jq -r '
        .workspace_members as $members
        | [.packages[] | select(.id as $id | $members | index($id)) | .name]
        | sort
        | join(", ")
    ' <<<"$WORKSPACE_METADATA"
})"

echo "release metadata check: $TAG_NAME matches Cargo.toml, Cargo.lock, CHANGELOG.md, and $WORKSPACE_PACKAGE_COUNT workspace packages ($WORKSPACE_PACKAGE_NAMES)"
