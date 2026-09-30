#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

base_ref="${1:-origin/main}"
if ! git rev-parse --verify --quiet "$base_ref^{commit}" >/dev/null; then
    echo "Unable to resolve base ref: $base_ref" >&2
    exit 2
fi

merge_base=$(git merge-base "$base_ref" HEAD)
scratch="target/level-scope-$$"
mkdir -p "$scratch"
changed="$scratch/changed"
trap 'rm -rf -- "$scratch"' EXIT

{
    git diff --no-renames --name-only -z "$merge_base"...HEAD
    git diff --cached --no-renames --name-only -z
    git diff --no-renames --name-only -z
    git ls-files --others --exclude-standard -z
} | sort -zu > "$changed"

violations=()
# SQLite and PostgreSQL are migrated in place; remaining classic paths stay protected.
while IFS= read -r -d '' path; do
    case "$path" in
        kuberic-core/* | kuberic-operator/* | examples/kvstore/* | \
            kuberic-tests/*)
            violations+=("$path")
            ;;
    esac
done < "$changed"

if ((${#violations[@]} > 0)); then
    echo "Level-triggered work modified protected v1 paths:" >&2
    printf '%s\n' "${violations[@]}" >&2
    exit 1
fi

echo "Protected v1 paths are unchanged."
