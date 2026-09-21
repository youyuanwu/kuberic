#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

base_ref="${1:-origin/main}"
if ! git rev-parse --verify --quiet "$base_ref^{commit}" >/dev/null; then
    echo "Unable to resolve base ref: $base_ref" >&2
    exit 2
fi

merge_base=$(git merge-base "$base_ref" HEAD)
changed=$(mktemp)
trap 'rm -f -- "$changed"' EXIT

{
    git diff --name-only "$merge_base"...HEAD
    git diff --cached --name-only
    git diff --name-only
    git ls-files --others --exclude-standard
} | sort -u > "$changed"

protected_pattern='^(kuberic-core/|kuberic-operator/|examples/kvstore/|examples/sqlite/|examples/postgres/|kuberic-tests/)'
violations=$(grep -E "$protected_pattern" "$changed" || true)
if [[ -n "$violations" ]]; then
    echo "Level-triggered work modified protected v1 paths:" >&2
    printf '%s\n' "$violations" >&2
    exit 1
fi

echo "Protected v1 paths are unchanged."
