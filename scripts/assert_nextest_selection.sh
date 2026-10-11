#!/usr/bin/env bash

set -euo pipefail

label="nextest selection"
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

output="$(mktemp)"
trap 'rm -f "$output"' EXIT

"$@" >"$output"

if [[ ! -s "$output" ]]; then
  echo "$label: nextest selection is empty" >&2
  exit 1
fi

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
