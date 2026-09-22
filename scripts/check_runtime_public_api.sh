#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo doc -p kuberic-runtime --no-deps --quiet

doc_root="target/doc/kuberic_runtime"
forbidden=(
    ">PodRuntime<"
    ">RuntimeEffect<"
    ">AuthorityStore<"
    ">ManagedReplicator<"
    ">OutboundReplication<"
    'href="runtime/index.html"'
    'href="authority/index.html"'
    'href="effects/index.html"'
)

for value in "${forbidden[@]}"; do
    if grep -R -F -- "$value" "$doc_root/index.html" "$doc_root/all.html" >/dev/null; then
        echo "kuberic-runtime Rustdoc exposes internal API: $value" >&2
        exit 1
    fi
done

echo "kuberic-runtime Rustdoc exposes only the SF-shaped application API."
