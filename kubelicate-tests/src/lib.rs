#[cfg(test)]
mod tests;

#[cfg(test)]
pub mod test_utils {

    /// Get the root directory of the repository
    pub fn get_repo_root() -> std::path::PathBuf {
        let dir = std::env::current_dir().expect("Failed to get current dir");
        dir.parent().unwrap().to_path_buf()
    }

    pub async fn ensure_op_deployed() {
        static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        INIT.get_or_init(async || {
            tracing::debug!("Ensuring operator is deployed...");
            ensure_op_deployed_internal().await;
        })
        .await;
    }

    pub async fn ensure_op_deployed_internal() {
        // Use k8s client to deploy the operator if not already deployed
        // get the default client
        let client = kube::Client::try_default()
            .await
            .expect("Failed to create k8s client");
        // check if the deployment exists
        let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
            kube::Api::namespaced(client.clone(), kubelicate_shared::NAME_XEDIO);
        let deployment_name = "xdata-operator";
        match deployments.get(deployment_name).await {
            Ok(_) => {
                tracing::info!("Operator deployment '{}' already exists", deployment_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                tracing::info!(
                    "Operator deployment '{}' not found, deploying...",
                    deployment_name
                );
                // Apply the deployment yaml
                let repo_root = crate::test_utils::get_repo_root();
                let deploy_path = repo_root
                    .join("apps")
                    .join("xdata-operator")
                    .join("deploy")
                    .join("deployment.yaml");
                crate::test_utils::kubectl_apply(&deploy_path).await;

                // Wait for deployment to be ready
                crate::test_utils::wait_deployment_replica_ready(
                    kubelicate_shared::NAME_XEDIO,
                    deployment_name,
                    1,
                    120,
                )
                .await
                .expect("Failed to wait for operator deployment to be ready");
                tracing::info!(
                    "Operator deployment '{}' deployed successfully",
                    deployment_name
                );
            }
            Err(e) => {
                panic!("Failed to get deployment: {}", e);
            }
        }
    }

    pub async fn ensure_xdata_app_deployed() {
        static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        INIT.get_or_init(async || {
            tracing::debug!("Ensuring example XdataApp is deployed...");
            ensure_xdata_app_deployed_internal().await;
        })
        .await;
    }

    /// apply this yml: apps/xdata-operator/deploy/xdataapp-example.yaml
    pub async fn ensure_xdata_app_deployed_internal() {
        tracing::info!("Ensuring example XdataApp is deployed...");
        // Check if xdata-app statefulset exists
        let client = kube::Client::try_default()
            .await
            .expect("Failed to create k8s client");
        let statefulsets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
            kube::Api::namespaced(client.clone(), kubelicate_shared::NAME_XEDIO);
        let statefulset_name = "xdata-app";
        match statefulsets.get(statefulset_name).await {
            Ok(_) => {
                tracing::info!("XdataApp StatefulSet '{}' already exists", statefulset_name);
                return;
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                tracing::info!(
                    "XdataApp StatefulSet '{}' not found, deploying...",
                    statefulset_name
                );
            }
            Err(e) => {
                panic!("Failed to get StatefulSet: {}", e);
            }
        }

        let repo_root = get_repo_root();
        let deploy_path = repo_root
            .join("apps")
            .join("xdata-operator")
            .join("deploy")
            .join("xdataapp-example.yaml");
        kubectl_apply(&deploy_path).await;

        // Statefulset takes some time to show up.
        let mut attempts = 0;
        while wait_statefulset_replica_ready(kubelicate_shared::NAME_XEDIO, "xdata-app", 3, 180)
            .await
            .is_err()
        {
            attempts += 1;
            if attempts >= 5 {
                panic!("XdataApp StatefulSet 'xdata-app' failed to be ready in time");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await
        }

        tracing::info!("Example XdataApp deployed successfully");
    }

    pub async fn ensure_xdata_crd_deployed() {
        static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        INIT.get_or_init(async || {
            tracing::debug!("Ensuring XdataApp CRD is deployed...");
            ensure_xdata_crd_deployed_internal().await;
        })
        .await;
    }

    pub async fn ensure_xdata_crd_deployed_internal() {
        tracing::info!("Ensuring XdataApp CRD is deployed...");
        // Check if the CRD exists
        let client = kube::Client::try_default()
            .await
            .expect("Failed to create k8s client");
        let crds: kube::Api<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition> =
            kube::Api::all(client.clone());
        let crd_name = "xdataapps.xedio.io";
        match crds.get(crd_name).await {
            Ok(_) => {
                tracing::info!("XdataApp CRD '{}' already exists", crd_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                tracing::info!("XdataApp CRD '{}' not found, deploying...", crd_name);
                // Apply the CRD yaml
                let repo_root = get_repo_root();
                let crd_path = repo_root
                    .join("apps")
                    .join("xdata-operator")
                    .join("deploy")
                    .join("xdataapp-crd.yaml");
                kubectl_apply(&crd_path).await;

                tracing::info!("XdataApp CRD '{}' deployed successfully", crd_name);
            }
            Err(e) => {
                panic!("Failed to get CRD: {}", e);
            }
        }
    }

    pub async fn kubectl_apply(path: &std::path::Path) {
        run_kubectl_cmd(&["apply", "-f", path.to_str().unwrap()])
            .await
            .expect("Failed to apply kubectl manifest");
    }

    pub async fn run_kubectl_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Running kubectl command: kubectl {:?}", args.join(" "));
        let output = tokio::process::Command::new("kubectl")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("Failed to execute kubectl command");

        if !output.status.success() {
            // get stderr and stdout
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            tracing::error!(
                "kubectl command failed. stdout: {}, stderr: {}",
                stdout,
                stderr
            );
            return Err(format!("kubectl command failed: {:?}", args).into());
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            tracing::info!("kubectl command succeeded. stdout: {}", stdout);
        }

        Ok(())
    }

    pub async fn wait_statefulset_replica_ready(
        namespace: &str,
        statefulset_name: &str,
        expected_replicas: i32,
        timeout_seconds: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let jsonpath = format!("{{.status.readyReplicas}}={}", expected_replicas);
        let timeout_arg = format!("{}s", timeout_seconds);
        let resource = format!("statefulset/{}", statefulset_name);
        run_kubectl_cmd(&[
            "wait",
            &resource,
            "--namespace",
            namespace,
            format!("--for=jsonpath={}", jsonpath).as_str(),
            "--timeout",
            &timeout_arg,
        ])
        .await
    }

    pub async fn wait_deployment_replica_ready(
        namespace: &str,
        deployment_name: &str,
        expected_replicas: i32,
        timeout_seconds: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let jsonpath = format!("{{.status.readyReplicas}}={}", expected_replicas);
        let timeout_arg = format!("{}s", timeout_seconds);
        let resource = format!("deployment/{}", deployment_name);
        run_kubectl_cmd(&[
            "wait",
            &resource,
            "--namespace",
            namespace,
            format!("--for=jsonpath={}", jsonpath).as_str(),
            "--timeout",
            &timeout_arg,
        ])
        .await
    }

    pub async fn ensure_test_env_deployed() {
        ensure_xdata_crd_deployed().await;
        ensure_op_deployed().await;
        ensure_xdata_app_deployed().await;
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_ensure_xdata_app_deployed() {
        ensure_test_env_deployed().await;
    }
}
