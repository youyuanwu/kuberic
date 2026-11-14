use anyhow::{ensure, Result};
use serde_json::{json, Value};

pub fn installation(namespace: &str, image: &str) -> Result<Value> {
    ensure!(
        !namespace.is_empty() && !image.is_empty(),
        "namespace and operator image are required"
    );
    Ok(json!({"apiVersion": "v1", "kind": "List", "items": [
        {"apiVersion": "v1", "kind": "ServiceAccount",
         "metadata": {"name": "kuberic-operator", "namespace": namespace}},
        {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
         "metadata": {"name": "kuberic-operator", "namespace": namespace},
         "rules": [
            {"apiGroups": ["operator.kuberic.io"], "resources": ["kubericsets", "kubericsets/status", "kubericsets/finalizers"],
             "verbs": ["get", "list", "watch", "update", "patch"]},
            {"apiGroups": [""], "resources": ["pods", "pods/finalizers", "configmaps", "services"],
             "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"]}
         ]},
        {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
         "metadata": {"name": "kuberic-operator", "namespace": namespace},
         "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "kuberic-operator"},
         "subjects": [{"kind": "ServiceAccount", "name": "kuberic-operator", "namespace": namespace}]},
        {"apiVersion": "apps/v1", "kind": "Deployment",
         "metadata": {"name": "kuberic-operator", "namespace": namespace},
         "spec": {"replicas": 1, "strategy": {"type": "Recreate"},
            "selector": {"matchLabels": {"app.kubernetes.io/name": "kuberic-operator"}},
            "template": {"metadata": {"labels": {"app.kubernetes.io/name": "kuberic-operator"}},
                "spec": {"serviceAccountName": "kuberic-operator", "terminationGracePeriodSeconds": 60,
                    "containers": [{"name": "operator", "image": image,
                        "command": ["/sbin/tini", "-g", "--", "/opt/kuberic/kuberic-operator"],
                        "args": ["run"],
                        "env": [
                            {"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}},
                            {"name": "POD_IP", "valueFrom": {"fieldRef": {"fieldPath": "status.podIP"}}}
                        ],
                        "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}
                    }]
                }
            }
         }}
    ]}))
}
