# xdata-operator Deployment

This directory contains the deployment manifests for the xdata-operator.

The xdata-operator manages xdata-app StatefulSet deployments through a custom resource definition (CRD).

## Prerequisites

- Kubernetes cluster (1.19+)
- kubectl configured to access the cluster
- Docker for building the operator image

## Building the Image

```bash
# Build the Rust binary
cargo build --release --bin xdata-op

# Build the Docker image
docker build -f apps/xdata-op/deploy/Dockerfile -t localhost/xdata-op:latest .
```

## Deploying to Kubernetes

### 1. Install the Custom Resource Definition

First, install the XdataApp CRD:

```bash
kubectl apply -f apps/xdata-op/deploy/xdataapp-crd.yaml
```

Verify the CRD is installed:

```bash
kubectl get crd xdataapps.xedio.io
```

### 2. Deploy the Operator

```bash
# Apply the operator deployment
kubectl apply -f apps/xdata-op/deploy/deployment.yaml

# Check the operator is running
kubectl get pods -n xedio -l app=xdata-operator

# View operator logs
kubectl logs -n xedio -l app=xdata-operator -f
```

### 3. Deploy an XdataApp

Create an XdataApp resource:

```bash
kubectl apply -f apps/xdata-op/deploy/xdataapp-example.yaml
```

Watch the operator create the StatefulSet and related resources:

```bash
# Watch the XdataApp status
kubectl get xdataapp -n xedio -w

# View the created StatefulSet
kubectl get statefulset -n xedio

# View the created Services
kubectl get svc -n xedio

# View the pods
kubectl get pods -n xedio -l app=xdata-app
```

## Custom Resource Specification

The XdataApp CRD supports the following fields:

```yaml
apiVersion: xedio.io/v1
kind: XdataApp
metadata:
  name: xdata-app
  namespace: xedio
spec:
  replicas: 3                              # Number of StatefulSet replicas (default: 3)
  image: localhost/xdata-app:latest        # Container image (default: localhost/xdata-app:latest)
  imagePullPolicy: IfNotPresent            # Image pull policy (default: IfNotPresent)
  storage: "256Mi"                         # PVC storage size (default: 256Mi)
  port: 8080                               # Application port (default: 8080)
  nodePortEnabled: true                    # Enable NodePort services (default: true)
  nodePortBase: 30081                      # Base NodePort number (default: 30081)
  resources:                               # Resource requirements
    requests:
      memory: "64Mi"
      cpu: "100m"
    limits:
      memory: "256Mi"
      cpu: "500m"
  env:                                     # Environment variables (optional)
  - name: RUST_LOG
    value: "info"
```

## What the Operator Creates

For each XdataApp resource, the operator creates:

1. **StatefulSet**: Manages the xdata-app pods with persistent storage
2. **Headless Service**: For StatefulSet DNS discovery (`xdata-app.xedio.svc.cluster.local`)
3. **NodePort Services**: Individual services for each pod (if `nodePortEnabled: true`)
   - `xdata-app-0` on port 30081
   - `xdata-app-1` on port 30082
   - `xdata-app-2` on port 30083
4. **ServiceAccount**: For pod identity
5. **Role & RoleBinding**: For leader election permissions

## Accessing the Application

### From within the cluster:

```bash
# Access the headless service (load-balanced)
curl http://xdata-app.xedio.svc.cluster.local

# Access a specific pod
curl http://xdata-app-0.xdata-app.xedio.svc.cluster.local:8080
```

### From outside the cluster (NodePort):

```bash
# Get the node IP
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')

# Access pod 0
curl http://$NODE_IP:30081

# Access pod 1
curl http://$NODE_IP:30082

# Access pod 2
curl http://$NODE_IP:30083
```

## Updating an XdataApp

Edit the XdataApp resource to update the deployment:

```bash
# Scale replicas
kubectl patch xdataapp xdata-app -n xedio -p '{"spec":{"replicas":5}}' --type=merge

# Update image
kubectl patch xdataapp xdata-app -n xedio -p '{"spec":{"image":"localhost/xdata-app:v2"}}' --type=merge

# Or edit directly
kubectl edit xdataapp xdata-app -n xedio
```

The operator will automatically reconcile the StatefulSet to match the desired state.

## Testing the Operator

```bash
# Create a test XdataApp
kubectl apply -f apps/xdata-op/deploy/xdataapp-example.yaml

# Wait for pods to be ready
kubectl wait --for=condition=ready pod -l app=xdata-app -n xedio --timeout=300s

# Check the status
kubectl get xdataapp xdata-app -n xedio -o yaml

# View the StatefulSet
kubectl describe statefulset xdata-app -n xedio

# Test cleanup (deletes all managed resources)
kubectl delete xdataapp xdata-app -n xedio
```

## Cleanup

```bash
# Delete all XdataApp resources
kubectl delete xdataapp --all -n xedio

# Delete the operator
kubectl delete -f apps/xdata-op/deploy/deployment.yaml

# Delete the CRD (will delete all XdataApp resources)
kubectl delete -f apps/xdata-op/deploy/xdataapp-crd.yaml
```

## Architecture

The operator runs as a stateless Deployment with:
- **1 replica** (recommended for simplicity)
- **ClusterRole** permissions to manage XdataApps, StatefulSets, Services, RBAC resources
- **ServiceAccount** for secure pod identity
- **Finalizers** for proper cleanup when XdataApps are deleted
- **Status updates** to track StatefulSet state

## Operator Behavior

- **Apply**: When a XdataApp is created or updated, the operator creates/updates all managed resources
- **Cleanup**: When a XdataApp is deleted, the operator removes all managed resources
- **Reconciliation**: The operator periodically checks and ensures the actual state matches the desired state
- **Owner References**: All managed resources have owner references to the XdataApp for garbage collection

## Troubleshooting

### Operator not starting:

```bash
kubectl describe pod -n xedio -l app=xdata-operator
kubectl logs -n xedio -l app=xdata-operator
```

### StatefulSet not created:

```bash
# Check XdataApp status
kubectl describe xdataapp xdata-app -n xedio

# Check operator logs
kubectl logs -n xedio -l app=xdata-operator | grep -i error
```

### RBAC issues:

```bash
# Verify ClusterRole exists
kubectl get clusterrole xdata-operator

# Verify ClusterRoleBinding
kubectl get clusterrolebinding xdata-operator
```
