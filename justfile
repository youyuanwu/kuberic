cluster_name := env_var_or_default("KIND_CLUSTER_NAME", "")
kubeconfig := env_var_or_default("KUBECONFIG", "")
cluster_context := "kind-" + cluster_name
kind_config := env_var_or_default("KIND_CONFIG", "deploy/kind-config.yaml")
ownership_receipt := kubeconfig + ".kuberic-owner"
level_token := env_var_or_default("KUBERIC_AGENT_BEARER_TOKEN", "")
nextest_archive := env_var_or_default("NEXTEST_ARCHIVE", "target/nextest/kuberic-tests.tar.zst")

# Build the retained level-triggered binaries.
default: level-triggered-build

# Install the repository-pinned cargo-nextest binary.
install-nextest:
    scripts/install_nextest.sh

# Verify guards that need the build environment rather than an archive-only runner.
nextest-build-only: install-nextest
    scripts/check_runtime_public_api.sh
    cargo nextest run -j 1 -p kuberic-runtime --all-features \
        --profile build-only --run-ignored only \
        -E 'test(=public_v1_conformance_executes_built_in_replacement_scenarios)'

# Verify build-only guards and build one reusable all-features test archive.
nextest-archive: nextest-build-only
    cargo build -p postgres-replicated --bin postgres-replicated --all-features
    mkdir -p "$(dirname "{{ nextest_archive }}")"
    cargo nextest archive --workspace --all-features --profile ci \
        --archive-file "{{ nextest_archive }}"

# Validate exact-once repository tiers and all configured partitions.
nextest-validate: install-nextest
    python3 scripts/validate_nextest_partitions.py

# Validate exact-once tiers and partitions from the reusable archive.
nextest-validate-archive: install-nextest
    python3 scripts/validate_nextest_partitions.py \
        --archive-file "{{ nextest_archive }}"

# List one semantic repository tier: all, ordinary, build-only, postgres, kind, or dex-live.
nextest-list tier="all": install-nextest
    #!/usr/bin/env bash
    set -euo pipefail
    ignored_args=()
    case "{{ tier }}" in
      all) profile=ci ;;
      ordinary) profile=ordinary ;;
      build-only) profile=build-only; ignored_args=(--run-ignored only) ;;
      postgres) profile=postgres ;;
      kind) profile=kind; ignored_args=(--run-ignored only) ;;
      dex-live) profile=dex-live ;;
      *) echo "unknown nextest tier: {{ tier }}" >&2; exit 2 ;;
    esac
    cargo nextest list --workspace --all-features --profile "$profile" "${ignored_args[@]}"

# Run the cluster-free repository tier, optionally as slice N/M.
nextest-test partition="": install-nextest
    #!/usr/bin/env bash
    set -euo pipefail
    partition_args=()
    if [[ -n "{{ partition }}" ]]; then
      partition_args=(--partition "slice:{{ partition }}")
    fi
    ordinary_target="$PWD/target/nextest-ordinary"
    rm -rf "$ordinary_target"
    CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 \
      CARGO_TARGET_DIR="$ordinary_target" cargo nextest run -j 2 \
      --workspace --all-features --profile ordinary "${partition_args[@]}"

# Run the PostgreSQL tier serially, optionally as hash shard N/M.
nextest-postgres partition="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "${WSL_INTEROP:-}" || -n "${WSL_DISTRO_NAME:-}" ]] \
      || grep -qi 'microsoft-standard-WSL' /proc/sys/kernel/osrelease 2>/dev/null; then
      echo "Skipping PostgreSQL tests: WSL does not provide the required pidfd reaping semantics."
      exit 0
    fi
    scratch="${TMPDIR:-$PWD/target/paw-tmp}"
    mkdir -p "$scratch"
    TMPDIR="$scratch" scripts/install_nextest.sh
    partition_args=()
    if [[ -n "{{ partition }}" ]]; then
      partition_args=(--partition "hash:{{ partition }}")
    fi
    TMPDIR="$scratch" cargo nextest run \
      --workspace --all-features --profile postgres "${partition_args[@]}"

# Prove the Public V1 PostgreSQL oracle is present before starting PostgreSQL.
nextest-postgres-smoke-inventory:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch="${TMPDIR:-$PWD/target/paw-tmp}"
    mkdir -p "$scratch"
    TMPDIR="$scratch" scripts/install_nextest.sh
    public_v1_filter="$(python3 scripts/validate_nextest_partitions.py --postgres-smoke-filter)"
    TMPDIR="$scratch" scripts/assert_nextest_selection.sh \
      --label postgres-smoke \
      --require-binary postgres-replicated -- \
      cargo nextest list -p postgres-replicated --all-features \
        --profile postgres -E "$public_v1_filter"

# Run a representative real-PostgreSQL smoke selection for routine local checks.
nextest-postgres-smoke: nextest-postgres-smoke-inventory
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "${WSL_INTEROP:-}" || -n "${WSL_DISTRO_NAME:-}" ]] \
      || grep -qi 'microsoft-standard-WSL' /proc/sys/kernel/osrelease 2>/dev/null; then
      echo "Skipping PostgreSQL smoke tests: WSL does not provide the required pidfd reaping semantics."
      exit 0
    fi
    scratch="${TMPDIR:-$PWD/target/paw-tmp}"
    mkdir -p "$scratch"
    TMPDIR="$scratch" scripts/install_nextest.sh
    public_v1_tmp="${KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR:-}"
    if [[ -z "$public_v1_tmp" ]]; then
      public_v1_tmp="/tmp"
      if [[ -d /dev/shm && -w /dev/shm ]] \
        && [[ "$(df --output=avail -B1 /dev/shm | tail -1)" -ge 2147483648 ]]; then
        public_v1_tmp="/dev/shm"
      fi
    fi
    mkdir -p "$public_v1_tmp"
    TMPDIR="$scratch" cargo build -p postgres-replicated --bin postgres-replicated
    public_v1_filter="$(python3 scripts/validate_nextest_partitions.py --postgres-smoke-filter)"
    KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$public_v1_tmp" TMPDIR="$scratch" cargo nextest run -j 1 \
      -p postgres-replicated --all-features --profile postgres -E "$public_v1_filter"

# Create the local Kind cluster and write its kubeconfig.
create-kind-cluster:
    test -n "{{ cluster_name }}"
    test -n "{{ kubeconfig }}"
    test "{{ cluster_name }}" != "kind"
    test "{{ kubeconfig }}" != "$HOME/.kube/config"
    test "$(printf %s "{{ cluster_name }}" | wc -c)" -le 40
    kind create cluster --name {{ cluster_name }} \
        --config "{{ kind_config }}" \
        --kubeconfig "{{ kubeconfig }}"
    kind export kubeconfig --name {{ cluster_name }} --kubeconfig "{{ kubeconfig }}"
    printf '%s\n' \
        "cluster={{ cluster_name }}" \
        "context={{ cluster_context }}" \
        "kubeconfig={{ kubeconfig }}" \
        | install -m 600 /dev/stdin "{{ ownership_receipt }}"
    just verify-kind-context

# Verify the exact cluster/kubeconfig pair was created by this workflow.
verify-kind-ownership:
    test -n "{{ cluster_name }}"
    test -n "{{ kubeconfig }}"
    test -f "{{ ownership_receipt }}"
    grep -Fx "cluster={{ cluster_name }}" "{{ ownership_receipt }}"
    grep -Fx "context={{ cluster_context }}" "{{ ownership_receipt }}"
    grep -Fx "kubeconfig={{ kubeconfig }}" "{{ ownership_receipt }}"

# Verify every Kubernetes mutation targets the dedicated Kind checkout.
verify-kind-context: verify-kind-ownership
    test "$(kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" config current-context)" = "{{ cluster_context }}"
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" cluster-info

# Delete the local Kind cluster.
delete-kind-cluster: verify-kind-context
    kind delete cluster --name {{ cluster_name }}
    rm -f "{{ kubeconfig }}" "{{ ownership_receipt }}"

# Build the isolated level-triggered packages.
level-triggered-build:
    cargo build -p kuberic-controller -p kvstore2 -p kuberic-level-tests

# Build and load only the level-triggered controller and application images.
level-triggered-images: verify-kind-context
    docker build -t localhost/kuberic-controller:level-triggered-v1 \
        -f kuberic-controller/Dockerfile .
    docker build -t localhost/kvstore2:level-triggered-v1 \
        -f examples/kvstore2/deploy/Dockerfile .
    kind load docker-image localhost/kuberic-controller:level-triggered-v1 --name {{ cluster_name }}
    kind load docker-image localhost/kvstore2:level-triggered-v1 --name {{ cluster_name }}

# Install the level-triggered controller and sample.
level-triggered-install: verify-kind-context
    test -n "{{ level_token }}"
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        create namespace kuberic-system --dry-run=client -o yaml | \
        kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" apply -f -
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system create secret generic kuberic-agent-credentials \
        --from-literal=bearer-token="{{ level_token }}" \
        --dry-run=client -o yaml | \
        kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" apply -f -
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        apply -k kuberic-controller/deploy
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system set image deployment/kuberic-controller \
        controller=localhost/kuberic-controller:level-triggered-v1
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system rollout status deployment/kuberic-controller --timeout=180s
    awk '1; /^metadata:$/ { print "  annotations:"; print "    testing.kuberic.io/live-copy-gate: enabled" }' \
        examples/kvstore2/deploy/sample.yaml | \
        kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" apply -f -

# Run one or more explicit isolated level-triggered KinD scenarios.
level-triggered-kind-test *scenarios: verify-kind-context
    #!/usr/bin/env bash
    set -euo pipefail
    requested=({{ scenarios }})
    if [[ ${#requested[@]} -eq 0 ]]; then
      requested=(all)
    fi
    expanded=()
    for scenario in "${requested[@]}"; do
      if [[ "$scenario" == "all" ]]; then
        expanded+=(bootstrap replacement failover)
      else
        expanded+=("$scenario")
      fi
    done
    for scenario in "${expanded[@]}"; do
      case "$scenario" in
        bootstrap) test_names=("level_triggered_k8s::bootstrap_reaches_three_member_topology_and_quorum_write") ;;
        replacement) test_names=("level_triggered_k8s::replacement_preserves_quorum_write_and_retires_old_incarnation") ;;
        failover) test_names=("level_triggered_k8s::failover_fences_old_primary_and_preserves_committed_data") ;;
        *) echo "unknown level-triggered scenario: $scenario" >&2; exit 2 ;;
      esac
      started=$SECONDS
      echo "=== level-triggered scenario: $scenario ==="
      for test_name in "${test_names[@]}"; do
        cargo nextest run -p kuberic-level-tests --profile kind --run-ignored only \
          -E "group(=kind-live) and test(=${test_name})" --no-capture
      done
      echo "=== $scenario passed in $((SECONDS - started))s ==="
    done

# Collect level-triggered controller, resource, and replica diagnostics.
level-triggered-diagnostics: verify-kind-context
    #!/usr/bin/env bash
    set -uo pipefail
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system logs deployment/kuberic-controller --all-containers --tail=-1 || true
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system get kubericset kvstore2 -o yaml || true
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system get kubericset kvstore2 \
        -o jsonpath='{range .status.conditions[*]}{.type}{"\t"}{.status}{"\t"}{.reason}{"\t"}{.message}{"\n"}{end}' || true
    echo
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system get pods,pvc,services -o wide || true
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system get pods,pvc,services -l operator.kuberic.io/set-name=kvstore2 -o yaml || true
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system get events --sort-by=.lastTimestamp || true
    kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system logs -l operator.kuberic.io/set-name=kvstore2 --all-containers --tail=-1 || true
    # Replica /status combines durable agent identity/PC/CC/pending work with runtime progress/access.
    for pod in $(kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
      -n kuberic-system get pods -l operator.kuberic.io/set-name=kvstore2 -o name); do
      echo "=== ${pod}: agent/runtime evidence ==="
      timeout --kill-after=2s 15s kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system exec "${pod}" -- curl --fail --silent --show-error --max-time 5 \
        http://127.0.0.1:8080/status || true
      echo
      kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system logs "${pod}" --all-containers --previous --tail=200 || true
      echo "=== ${pod}: storage layout ==="
      timeout --kill-after=2s 15s kubectl --kubeconfig "{{ kubeconfig }}" --context "{{ cluster_context }}" \
        -n kuberic-system exec "${pod}" -- sh -c \
        'find /var/lib/kuberic -maxdepth 4 -printf "%y %s %p\n" 2>/dev/null | sort; du -ah /var/lib/kuberic 2>/dev/null | sort -h | tail -100' || true
    done
