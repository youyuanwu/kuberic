# XdataApp Operator Quick Start Guide

This guide shows how to deploy and manage xdata-app instances using the Kubernetes operator.

## Overview

The xdata-operator manages xdata-app StatefulSet deployments declaratively through a custom resource (XdataApp). Instead of manually creating StatefulSets, Services, and RBAC resources, you simply define an XdataApp resource and the operator handles the rest.

## Prerequisites

1. Kubernetes cluster (1.19+)
2. kubectl configured
3. Docker (for building images)

## Step 1: Build Images

```bash
# From the project root
cd /home/user1/code/rs/xedio

# Build xdata-app image
cargo build --release --bin xdata-app
docker build -f apps/xdata-app/deploy/Dockerfile -t localhost/xdata-app:latest .

# Build xdata-op image
cargo build --release --bin xdata-op
docker build -f apps/xdata-op/deploy/Dockerfile -t localhost/xdata-op:latest .
```

## Step 2: Install the CRD

```bash
kubectl apply -f apps/xdata-op/deploy/xdataapp-crd.yaml
```

Verify:
```bash
kubectl get crd xdataapps.xedio.io
```

## Step 3: Deploy the Operator

```bash
kubectl apply -f apps/xdata-op/deploy/deployment.yaml
```

Wait for the operator to be ready:
```bash
kubectl wait --for=condition=ready pod -l app=xdata-operator -n xedio --timeout=60s
```

Check logs:
```bash
kubectl logs -n xedio -l app=xdata-operator -f
```

## Step 4: Create an XdataApp

```bash
kubectl apply -f apps/xdata-op/deploy/xdataapp-example.yaml
```

This creates:
- 1 XdataApp custom resource
- 1 StatefulSet with 3 replicas
- 1 Headless Service
- 3 NodePort Services
- 1 ServiceAccount
- 1 Role + RoleBinding

## Step 5: Verify Deployment

```bash
# Check the XdataApp
kubectl get xdataapp -n xedio
kubectl describe xdataapp xdata-app -n xedio

# Check the StatefulSet
kubectl get statefulset -n xedio
kubectl get pods -n xedio -l app=xdata-app

# Check the Services
kubectl get svc -n xedio

# Wait for all pods to be ready
kubectl wait --for=condition=ready pod -l app=xdata-app -n xedio --timeout=300s
```

## Step 6: Access the Application

### From within the cluster:

```bash
# Run a test pod
kubectl run -it --rm test --image=curlimages/curl --restart=Never -n xedio -- sh

# Inside the pod:
curl http://xdata-app.xedio.svc.cluster.local
curl http://xdata-app-0.xdata-app.xedio.svc.cluster.local:8080
```

### From outside the cluster (NodePort):

```bash
# Get node IP
NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')

# Access individual pods
curl http://$NODE_IP:30081  # Pod 0
curl http://$NODE_IP:30082  # Pod 1
curl http://$NODE_IP:30083  # Pod 2
```

## Common Operations

### Scale the Application

```bash
kubectl patch xdataapp xdata-app -n xedio -p '{"spec":{"replicas":5}}' --type=merge
```

### Update the Image

```bash
kubectl patch xdataapp xdata-app -n xedio -p '{"spec":{"image":"localhost/xdata-app:v2"}}' --type=merge
```

### Change Resource Limits

```bash
kubectl patch xdataapp xdata-app -n xedio -p '{"spec":{"resources":{"limits":{"memory":"512Mi"}}}}' --type=merge
```

### Add Environment Variables

```yaml
kubectl patch xdataapp xdata-app -n xedio --type=merge -p '
spec:
  env:
  - name: RUST_LOG
    value: debug
  - name: MY_CUSTOM_VAR
    value: custom-value
'
```

### View Status

```bash
# Short view
kubectl get xdataapp -n xedio

# Detailed view
kubectl get xdataapp xdata-app -n xedio -o yaml

# Watch for changes
kubectl get xdataapp -n xedio -w
```

## Cleanup

### Delete the XdataApp (keeps operator running)

```bash
kubectl delete xdataapp xdata-app -n xedio
```

This will:
1. Delete the StatefulSet (and all pods)
2. Delete all Services
3. Delete the ServiceAccount
4. Delete the Role and RoleBinding
5. Delete the PersistentVolumeClaims

### Delete Everything

```bash
# Delete all XdataApps
kubectl delete xdataapp --all -n xedio

# Delete the operator
kubectl delete -f apps/xdata-op/deploy/deployment.yaml

# Delete the CRD
kubectl delete -f apps/xdata-op/deploy/xdataapp-crd.yaml

# Delete the namespace (if desired)
kubectl delete namespace xedio
```

## Troubleshooting

### Pods not starting

```bash
# Check pod status
kubectl describe pod -n xedio -l app=xdata-app

# Check events
kubectl get events -n xedio --sort-by='.lastTimestamp'

# Check logs
kubectl logs -n xedio xdata-app-0
```

### Operator not reconciling

```bash
# Check operator logs
kubectl logs -n xedio -l app=xdata-operator

# Check for errors
kubectl logs -n xedio -l app=xdata-operator | grep -i error

# Restart operator
kubectl rollout restart deployment xdata-operator -n xedio
```

### RBAC issues

```bash
# Verify ClusterRole
kubectl describe clusterrole xdata-operator

# Verify ClusterRoleBinding
kubectl describe clusterrolebinding xdata-operator
```

## Advanced Configuration

### Disable NodePort Services

```yaml
apiVersion: xedio.io/v1
kind: XdataApp
metadata:
  name: xdata-app
  namespace: xedio
spec:
  nodePortEnabled: false
  # ... other settings
```

### Custom Storage Size

```yaml
spec:
  storage: "1Gi"
```

### Custom NodePort Range

```yaml
spec:
  nodePortBase: 31000
  # Will create services on ports 31000, 31001, 31002, etc.
```

## Next Steps

- Review the [full operator documentation](apps/xdata-op/deploy/README.md)
- Customize the XdataApp specification for your needs
- Set up monitoring and alerting for your StatefulSet
- Implement backup strategies for persistent data
