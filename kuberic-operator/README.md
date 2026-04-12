# kuberic-operator

Kubernetes operator for [Kuberic](../README.md). Manages `KubericSet` custom resources — creates pods, orchestrates lifecycle, and handles failover.

## What It Does

1. Watches `KubericSet` CRDs (desired replica count, image, ports)
2. Creates/deletes bare pods to match desired state
3. Drives each pod through: Open → IdleSecondary → ActiveSecondary → Primary
4. Detects pod failures and triggers automatic failover
5. Supports scale up/down with copy-based replica building

## CRD Example

```yaml
apiVersion: kuberic.io/v1
kind: KubericSet
metadata:
  name: my-app
spec:
  replicas: 3
  image: my-app:latest
  controlPort: 50051
  dataPort: 50052
  clientPort: 50053
```

## Deployment

```bash
# Deploy the operator
kubectl apply -f kuberic-operator/deploy/deployment.yaml

# Create a replicated service
kubectl apply -f examples/kvstore/deploy/kubericset.yaml
```

## Architecture

| Module | Purpose |
|--------|---------|
| `crd.rs` | `KubericSet` CRD definition and status types |
| `reconciler.rs` | Main reconcile loop — pod management, lifecycle orchestration |
| `cluster_api.rs` | Kubernetes API helpers for pod/service operations |
