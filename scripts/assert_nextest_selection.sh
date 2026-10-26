#!/usr/bin/env bash

set -euo pipefail

label="nextest selection"
required_binaries=()
required_tests=()
required_tokens=()

while (($#)); do
  case "$1" in
    --label)
      label="${2:?missing value for --label}"
      shift 2
      ;;
    --require-test)
      required_tests+=("${2:?missing value for --require-test}")
      shift 2
      ;;
    --require-binary)
      required_binaries+=("${2:?missing value for --require-binary}")
      shift 2
      ;;
    --require-token)
      required_tokens+=("${2:?missing value for --require-token}")
      shift 2
      ;;
    --)
      shift
      break
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if (($# == 0)); then
  echo "$label: missing nextest list command after --" >&2
  exit 2
fi

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$root/target"
if [[ -L "$root/target" || ! -d "$root/target" ]]; then
  echo "$label: repository-local target directory is required" >&2
  exit 1
fi
owned=""
for ((attempt = 0; attempt < 64; attempt++)); do
  candidate="$root/target/nextest-selection-$$-$RANDOM-$attempt"
  if mkdir -m 700 -- "$candidate" 2>/dev/null; then
    owned="$candidate"
    break
  fi
done
if [[ -z "$owned" ]]; then
  echo "$label: cannot allocate owned repository-local inventory directory" >&2
  exit 1
fi
output="$owned/inventory"
trap 'rm -f -- "$output"; rmdir -- "$owned"' EXIT

"$@" >"$output"

if [[ ! -s "$output" ]]; then
  echo "$label: nextest selection is empty" >&2
  exit 1
fi

for required in "${required_binaries[@]}"; do
  if ! awk -v required="$required" '
    {
      binary_name = $1
      count = split(binary_name, components, "::")
      if (binary_name == required || components[count] == required) {
        found = 1
      }
    }
    END { exit found ? 0 : 1 }
  ' "$output"; then
    echo "$label: required binary is not selected: $required" >&2
    exit 1
  fi
done

for required in "${required_tests[@]}"; do
  if ! awk -v required="$required" '
    {
      test_name = $NF
      count = split(test_name, components, "::")
      if (test_name == required || components[count] == required) {
        found = 1
      }
    }
    END { exit found ? 0 : 1 }
  ' "$output"; then
    echo "$label: required test is not selected: $required" >&2
    exit 1
  fi
done

for required in "${required_tokens[@]}"; do
  if ! grep -Fq -- "$required" "$output"; then
    echo "$label: required inventory token is not selected: $required" >&2
    exit 1
  fi
done

count="$(wc -l <"$output" | tr -d '[:space:]')"
echo "$label: selected $count tests"
