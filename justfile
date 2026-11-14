kind_command := "cargo run --locked -p kuberic-kind-tests --bin kuberic-kind --"

default:
    @just --list

# Create an isolated cluster using explicit KIND_CLUSTER_NAME, KUBE_CONTEXT and KUBECONFIG.
create-kind-cluster:
    {{kind_command}} create

# Verify the archived-style cluster/context/kubeconfig ownership contract.
verify-kind-context:
    {{kind_command}} verify

# Build fresh operator/supervisor/test binaries and load the sample image.
kind-images:
    {{kind_command}} images

# Install the generated current CRD into the owned cluster.
kind-install:
    {{kind_command}} install

# Run real-cluster journeys; binary/image preparation happens once per test executable.
kind-test:
    cargo test --locked -p kuberic-kind-tests --test kind -- --ignored --nocapture

# Gather owned namespaces, workload state, events and container logs.
kind-diagnostics:
    {{kind_command}} diagnostics

# Delete only the explicitly owned cluster and its dedicated kubeconfig/receipt.
delete-kind-cluster:
    {{kind_command}} delete
