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
mkdir -p "$target_dir"
scratch="$target_dir/runtime-api-check-$$-$RANDOM"
mkdir "$scratch"
doc_target="$scratch/rustdoc"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    doc_root="$doc_target/$CARGO_BUILD_TARGET/doc/kuberic_runtime"
else
    doc_root="$doc_target/doc/kuberic_runtime"
fi
allowlist="scripts/runtime_public_api.allowlist"
fixture="$scratch/fixture"
inventory="$scratch/inventory"
fixture_output="$scratch/fixture-output"
cleanup() {
    rm -rf -- "$scratch"
}
trap cleanup EXIT
python3 -c '
from pathlib import Path
import shutil
import sys
source = Path("scripts/fixtures/runtime-api-leak")
destination = Path(sys.argv[1])
destination.mkdir()
shutil.copytree(source / "src", destination / "src")
manifest = (source / "Cargo.toml").read_text()
manifest = manifest.replace("../../../kuberic-runtime", (Path.cwd() / "kuberic-runtime").as_posix())
(destination / "Cargo.toml").write_text(manifest)
' "$fixture"

cargo doc --offline --locked -p kuberic-runtime --no-deps --quiet --target-dir "$doc_target"

find "$doc_root" -type f -name '*.html' -printf 'page:%P\n' |
    sort -u > "$inventory"

if ! diff -u "$allowlist" "$inventory"; then
    echo "kuberic-runtime documented public API differs from the reviewed allowlist." >&2
    exit 1
fi

if cargo check --offline --manifest-path "$fixture/Cargo.toml" --quiet \
    --target-dir "$scratch/private" >"$fixture_output" 2>&1; then
    echo "The external application fixture unexpectedly reached agent-owned runtime APIs." >&2
    exit 1
fi

for expected in \
    'struct `ReplicatorAttachment` is private' \
    'struct `ReplicatorCreationReservation` is private' \
    'trait `ReplicatorRegistration` is private' \
    'trait `PartitionAccessView` is private' \
    'struct `ReplicatorCreation` is private' \
    'module `capabilities` is private' \
    'module `hosting` is private' \
    'module `native_transport` is private' \
    'module `v1` is private' \
    'could not find `testing` in `kuberic_runtime`'; do
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
        --test public_api_inventory --test lifecycle_capability_boundaries --quiet
    cargo test -p kuberic-runtime --all-features \
        --test public_api_privacy --quiet -- --ignored

    echo "kuberic-runtime documented and source-public APIs match their allowlists; tested private-capability extraction, forging, attachment, guard, authority, and host-construction paths are unreachable from safe application code."
fi
