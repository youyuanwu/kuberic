#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# The source nextest profile owns Rust inventory/privacy tests in this mode.
rustdoc_only=false
case "${1:-}" in
    "") ;;
    --rustdoc-only) rustdoc_only=true ;;
    *) echo "usage: $0 [--rustdoc-only]" >&2; exit 2 ;;
esac
if [[ $# -gt 1 ]]; then
    echo "usage: $0 [--rustdoc-only]" >&2
    exit 2
fi

target_dir="${CARGO_TARGET_DIR:-target}"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    doc_root="$target_dir/$CARGO_BUILD_TARGET/doc/kuberic_runtime"
else
    doc_root="$target_dir/doc/kuberic_runtime"
fi
allowlist="scripts/runtime_public_api.allowlist"
fixture="scripts/fixtures/runtime-api-leak"
scratch="target/runtime-api-check-$$"
mkdir -p target
mkdir "$scratch"
inventory="$scratch/inventory"
fixture_output="$scratch/fixture-output"
trap 'rm -f -- "$fixture/Cargo.lock"; rm -rf -- "$scratch" "$fixture/target"' EXIT

rm -rf -- "$doc_root"
cargo doc -p kuberic-runtime --no-deps --quiet

find "$doc_root" -type f -name '*.html' -printf 'page:%P\n' |
    sort -u > "$inventory"

if ! diff -u "$allowlist" "$inventory"; then
    echo "kuberic-runtime documented public API differs from the reviewed allowlist." >&2
    exit 1
fi

if cargo check --manifest-path "$fixture/Cargo.toml" --quiet >"$fixture_output" 2>&1; then
    echo "The external application fixture unexpectedly reached agent-owned runtime APIs." >&2
    exit 1
fi

for expected in \
    'struct `ReplicatorAttachment` is private' \
    'struct `ReplicatorCreationReservation` is private' \
    'trait `ReplicatorRegistration` is private' \
    'trait `PartitionAccessView` is private' \
    'module `capabilities` is private' \
    'module `hosting` is private'; do
    if ! grep -Fq "$expected" "$fixture_output"; then
        cat "$fixture_output" >&2
        echo "The external fixture failed for an unexpected reason: $expected" >&2
        exit 1
    fi
done

if [[ "$rustdoc_only" == true ]]; then
    echo "kuberic-runtime rustdoc allowlist and external API-leak fixture guards passed."
else
    cargo test -p kuberic-runtime --all-features \
        --test public_api_inventory --quiet
    cargo test -p kuberic-runtime --all-features \
        --test public_api_privacy --quiet -- --ignored

    echo "kuberic-runtime documented and source-public APIs match their allowlists; tested private-capability extraction, forging, attachment, guard, authority, and host-construction paths are unreachable from safe application code."
fi
