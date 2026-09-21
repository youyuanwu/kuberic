#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

metadata=$(mktemp)
trap 'rm -f -- "$metadata"' EXIT
cargo metadata --format-version 1 --no-deps > "$metadata"

python3 - "$metadata" <<'PY'
import json
import pathlib
import sys

metadata_path = pathlib.Path(sys.argv[1])
metadata = json.loads(metadata_path.read_text())
new_packages = {
    "kuberic-protocol",
    "kuberic-wire",
    "kuberic-runtime",
    "kuberic-agent",
    "kuberic-controller",
    "kvstore2",
    "kuberic-level-tests",
}
protected_packages = {
    "kuberic-core",
    "kuberic-operator",
    "kvstore",
    "sqlite-replicated",
    "postgres-replicated",
    "kuberic-tests",
}

violations = []
for package in metadata["packages"]:
    if package["name"] not in new_packages:
        continue
    for dependency in package["dependencies"]:
        if dependency["name"] in protected_packages:
            violations.append(
                f'{package["name"]} depends on protected package {dependency["name"]}'
            )

if violations:
    print("Level-triggered dependency boundary violations:", file=sys.stderr)
    for violation in violations:
        print(f"- {violation}", file=sys.stderr)
    raise SystemExit(1)
PY

new_paths=(
    kuberic-protocol
    kuberic-wire
    kuberic-runtime
    kuberic-agent
    kuberic-controller
    examples/kvstore2
    kuberic-level-tests
)
existing_paths=()
for path in "${new_paths[@]}"; do
    [[ -e "$path" ]] && existing_paths+=("$path")
done

if ((${#existing_paths[@]} > 0)); then
    include_pattern='(include|include_str|include_bytes)!\([^)]*(kuberic-core|kuberic-operator|examples/(kvstore|sqlite|postgres)|kuberic-tests)'
    if grep -R -n -E "$include_pattern" "${existing_paths[@]}"; then
        echo "Level-triggered source imports protected v1 files." >&2
        exit 1
    fi
fi

echo "Level-triggered crates have no protected v1 dependencies."
