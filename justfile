cluster_name := "kind"

# Build and load all container images into Kind.
default: images

# Create the local Kind cluster and write its kubeconfig.
create-kind-cluster:
    kind create cluster --name {{ cluster_name }} \
        --config deploy/kind-config.yaml \
        --kubeconfig "$HOME/.kube/config"
    kind export kubeconfig --name {{ cluster_name }} --kubeconfig "$HOME/.kube/config"

# Delete the local Kind cluster.
delete-kind-cluster:
    kind delete cluster --name {{ cluster_name }}

# Build all workspace binaries used by the container images.
build-rust-bins:
    cargo build --bins --workspace

# Build and load all container images.
images: kuberic-operator-image kvstore-image

# Build and load the kuberic-operator image.
kuberic-operator-image: build-rust-bins
    docker build -t localhost/kuberic-operator \
        -f kuberic-operator/deploy/Dockerfile .
    kind load docker-image localhost/kuberic-operator:latest --name {{ cluster_name }}

# Deploy kuberic-operator.
kuberic-operator-deploy:
    kubectl apply -f kuberic-operator/deploy/deployment.yaml

# Delete kuberic-operator.
kuberic-operator-delete:
    kubectl delete -f kuberic-operator/deploy/deployment.yaml

# Build and load the kvstore image.
kvstore-image: build-rust-bins
    docker build -t localhost/kvstore \
        -f examples/kvstore/deploy/Dockerfile .
    kind load docker-image localhost/kvstore:latest --name {{ cluster_name }}

# Deploy kvstore.
kvstore-deploy:
    kubectl apply -f examples/kvstore/deploy/kubericset.yaml

# Delete kvstore.
kvstore-delete:
    kubectl delete -f examples/kvstore/deploy/kubericset.yaml
