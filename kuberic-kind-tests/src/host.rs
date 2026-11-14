use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use kube::{CustomResourceExt, ResourceExt};
use serde_json::{json, Value};

use crate::Scenario;

#[derive(Clone, Debug)]
pub struct Cluster {
    name: String,
    context: String,
    kubeconfig: PathBuf,
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

fn checked(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("starting {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

impl Cluster {
    pub fn requested() -> Result<Self> {
        let cluster = Self {
            name: std::env::var("KIND_CLUSTER_NAME").context("KIND_CLUSTER_NAME is required")?,
            context: std::env::var("KUBE_CONTEXT").context("KUBE_CONTEXT is required")?,
            kubeconfig: std::env::var("KUBECONFIG")
                .context("KUBECONFIG is required")?
                .into(),
        };
        cluster.validate()?;
        Ok(cluster)
    }

    fn validate(&self) -> Result<()> {
        let config = self
            .kubeconfig
            .to_str()
            .context("kubeconfig is not UTF-8")?;
        ensure!(
            self.name.starts_with("kuberic-")
                && self.name.len() <= 40
                && !self.name.ends_with('-')
                && self.name.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit() || byte == b'-')
                && self.context == format!("kind-{}", self.name)
                && self.kubeconfig.is_absolute()
                && !config.contains([':', '\n', '\r'])
                && !self.kubeconfig.components().any(|component|
                    component == std::path::Component::ParentDir)
                && self.kubeconfig.file_name().is_some()
                && self.kubeconfig != Path::new(&std::env::var("HOME")?).join(".kube/config")
                && !self.kubeconfig.is_symlink(),
            "KinD tests require an isolated kuberic-* cluster, matching KUBE_CONTEXT and dedicated absolute KUBECONFIG"
        );
        Ok(())
    }

    fn receipt_path(&self) -> PathBuf {
        let mut path = self.kubeconfig.as_os_str().to_os_string();
        path.push(".kuberic-owner");
        path.into()
    }

    fn receipt(&self) -> String {
        format!(
            "cluster={}\ncontext={}\nkubeconfig={}\n",
            self.name,
            self.context,
            self.kubeconfig.display()
        )
    }

    pub fn from_env() -> Result<Self> {
        let cluster = Self::requested()?;
        ensure!(
            fs::read_to_string(cluster.receipt_path())
                .context("reading dedicated cluster ownership receipt")?
                == cluster.receipt(),
            "cluster ownership receipt does not match the requested cluster/context/kubeconfig"
        );
        ensure!(
            cluster.kubectl(&["config", "current-context"])?.trim() == cluster.context,
            "owned kubeconfig current context changed"
        );
        Ok(cluster)
    }

    fn kubectl_command(&self) -> Command {
        let mut command = Command::new("kubectl");
        command.arg("--kubeconfig").arg(&self.kubeconfig).args([
            "--context",
            &self.context,
            "--request-timeout=15s",
        ]);
        command
    }

    pub fn kubectl(&self, arguments: &[&str]) -> Result<String> {
        Ok(String::from_utf8(
            checked(self.kubectl_command().args(arguments))?.stdout,
        )?)
    }

    fn apply(&self, value: &Value) -> Result<()> {
        let mut command = self.kubectl_command();
        command
            .args([
                "apply",
                "--server-side",
                "--field-manager=kuberic-kind-tests",
                "-f",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let written = child
            .stdin
            .take()
            .context("kubectl stdin")?
            .write_all(&serde_json::to_vec(value)?);
        let status = child.wait()?;
        written?;
        ensure!(
            status.success(),
            "applying KinD test resources failed: {status}"
        );
        Ok(())
    }

    pub fn create(&self) -> Result<()> {
        ensure!(
            !self.kubeconfig.try_exists()? && !self.receipt_path().try_exists()?,
            "refusing to overwrite an existing kubeconfig or ownership receipt"
        );
        let clusters = checked(Command::new("kind").args(["get", "clusters"]))?;
        ensure!(
            !String::from_utf8(clusters.stdout)?
                .lines()
                .any(|name| name == self.name),
            "refusing to reuse an existing cluster"
        );
        fs::create_dir_all(self.kubeconfig.parent().context("kubeconfig parent")?)?;
        checked(
            Command::new("kind")
                .args([
                    "create",
                    "cluster",
                    "--name",
                    &self.name,
                    "--image",
                    "kindest/node:v1.34.0",
                    "--wait",
                    "120s",
                    "--config",
                ])
                .arg(root().join("kuberic-kind-tests/deploy/kind-config.yaml"))
                .arg("--kubeconfig")
                .arg(&self.kubeconfig),
        )?;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.receipt_path())?
            .write_all(self.receipt().as_bytes())?;
        self.verify()
    }

    pub fn verify(&self) -> Result<()> {
        let current = Self::from_env()?;
        ensure!(
            current.name == self.name && current.kubeconfig == self.kubeconfig,
            "cluster environment changed"
        );
        self.kubectl(&["cluster-info"])?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        let current = Self::from_env()?;
        ensure!(
            current.name == self.name && current.kubeconfig == self.kubeconfig,
            "cluster environment changed"
        );
        checked(Command::new("kind").args(["delete", "cluster", "--name", &self.name]))?;
        fs::remove_file(&self.kubeconfig)?;
        fs::remove_file(self.receipt_path())?;
        Ok(())
    }

    pub fn install(&self) -> Result<()> {
        self.verify()?;
        self.apply(&serde_json::to_value(kuberic_operator::KubericSet::crd())?)?;
        self.kubectl(&[
            "wait",
            "--for=condition=Established",
            "--timeout=60s",
            "crd/kubericsets.operator.kuberic.io",
        ])?;
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<()> {
        let namespaces: Value = serde_json::from_str(&self.kubectl(&[
            "get",
            "namespaces",
            "-l",
            &format!("testing.kuberic.io/kind-owned={}", self.name),
            "-o",
            "json",
        ])?)?;
        let mut errors = Vec::new();
        for namespace in namespaces["items"].as_array().context("namespace list")? {
            let name = namespace["metadata"]["name"]
                .as_str()
                .context("namespace name")?;
            if let Err(error) = self.diagnose_namespace(name) {
                errors.push(format!("{error:#}"));
            }
        }
        ensure!(
            errors.is_empty(),
            "some diagnostics failed:\n{}",
            errors.join("\n")
        );
        Ok(())
    }

    fn diagnose_namespace(&self, name: &str) -> Result<()> {
        let mut errors = Vec::new();
        for arguments in [
            vec![
                "-n",
                name,
                "get",
                "jobs,pods,kubericsets,services",
                "-o",
                "yaml",
            ],
            vec![
                "-n",
                name,
                "get",
                "events",
                "--sort-by=.metadata.creationTimestamp",
            ],
        ] {
            match self.kubectl(&arguments) {
                Ok(output) => eprintln!("{output}"),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        let pods: Value =
            serde_json::from_str(&self.kubectl(&["-n", name, "get", "pods", "-o", "json"])?)?;
        for pod in pods["items"].as_array().context("Pod list")? {
            let pod_name = pod["metadata"]["name"].as_str().context("Pod name")?;
            match self.kubectl(&[
                "-n",
                name,
                "logs",
                pod_name,
                "--all-containers=true",
                "--tail=-1",
            ]) {
                Ok(output) => eprintln!("{pod_name}:\n{output}"),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        ensure!(
            errors.is_empty(),
            "some diagnostics failed:\n{}",
            errors.join("\n")
        );
        Ok(())
    }

    pub fn scenario(&self, scenario: Scenario) -> Result<()> {
        self.verify()?;
        static IMAGE: OnceLock<Result<String, String>> = OnceLock::new();
        let image = IMAGE
            .get_or_init(|| {
                self.install()
                    .and_then(|()| prepare_image(self))
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let namespace = format!("kuberic-e2e-{}", uuid::Uuid::new_v4().simple());
        let resources = scenario_resources(&namespace, &self.name, image, scenario)?;
        self.apply(&resources)?;
        let result = self.wait_for_scenario(&namespace, scenario);
        if let Err(error) = result {
            if let Err(diagnostics) = self.diagnose_namespace(&namespace) {
                eprintln!("Additional diagnostic failure: {diagnostics:#}");
            }
            bail!("{error:#}; failed namespace retained: {namespace}");
        }
        self.kubectl(&[
            "delete",
            "namespace",
            &namespace,
            "--wait=true",
            "--timeout=60s",
        ])?;
        Ok(())
    }

    fn wait_for_scenario(&self, namespace: &str, scenario: Scenario) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(600);
        let mut original_operator = None;
        loop {
            let job: Value = serde_json::from_str(
                &self.kubectl(&["-n", namespace, "get", "job", "driver", "-o", "json"])?,
            )?;
            ensure!(
                job["status"]["failed"].as_u64().unwrap_or(0) == 0,
                "in-cluster driver failed: {}",
                job["status"]
            );
            if scenario == Scenario::PrimaryAppFailover && original_operator.is_none() {
                let pods: Value = serde_json::from_str(&self.kubectl(&[
                    "-n",
                    namespace,
                    "get",
                    "pods",
                    "-l",
                    "job-name=driver",
                    "-o",
                    "json",
                ])?)?;
                if pods["items"]
                    .as_array()
                    .context("driver Pods")?
                    .iter()
                    .any(|pod| {
                        pod["status"]["containerStatuses"]
                            .as_array()
                            .is_some_and(|containers| {
                                containers
                                    .iter()
                                    .any(|container| container["state"]["running"].is_object())
                            })
                    })
                {
                    let logs = self.kubectl(&["-n", namespace, "logs", "job/driver"])?;
                    for line in logs.lines().filter(|line| line.starts_with('{')) {
                        let event: Value = serde_json::from_str(line)?;
                        if event["event"] == "failover_ready" {
                            original_operator = Some(self.crash_primary_app(namespace, &event)?);
                            break;
                        }
                    }
                }
            }
            if job["status"]["succeeded"].as_u64() == Some(1) {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "KinD driver deadline: {}",
                job["status"]
            );
            std::thread::sleep(Duration::from_secs(2));
        }
        let logs = self.kubectl(&["-n", namespace, "logs", "job/driver"])?;
        eprintln!("{logs}");
        let events: Vec<Value> = logs
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "scenario_complete"
                    && event["scenario"] == scenario.name()
                    && event["resources_absent"] == true
            })
            .context("successful driver omitted final lifecycle verification")?;
        if scenario == Scenario::PrimaryAppFailover {
            ensure!(
                original_operator.as_ref() == Some(&self.operator_uid(namespace)?),
                "failover lost or restarted the original operator"
            );
            ensure!(
                events
                    .iter()
                    .any(|event| event["event"] == "primary_app_failover"
                        && event["set_uid"] == completed["set_uid"]
                        && event["cohort"] == completed["cohort"]
                        && event["data_preserved"] == true
                        && event["writes_continued"] == true
                        && event["original_nodes_unchanged"] == true),
                "successful driver omitted actual application-crash failover evidence"
            );
        }
        ensure!(
            scenario == Scenario::DeleteDuringBootstrap
                || events
                    .iter()
                    .any(|event| event["event"] == "application_write_read"),
            "successful bootstrap omitted application write/read"
        );
        if scenario == Scenario::StaggeredShutdown {
            ensure!(
                completed["native_joined"] == true
                    && events
                        .iter()
                        .any(|event| event["event"] == "physical_cleanup"),
                "scripted qualification omitted native joining or physical cleanup"
            );
        } else {
            ensure!(
                completed["mode"] == "deployed-operator",
                "ordinary journey did not use the operator"
            );
            let logs = self.kubectl(&["-n", namespace, "logs", "deployment/kuberic-operator"])?;
            eprintln!("operator:\n{logs}");
            let operator_events: Vec<Value> = logs
                .lines()
                .filter(|line| line.starts_with('{'))
                .map(serde_json::from_str)
                .collect::<std::result::Result<_, _>>()?;
            verify_operator_cleanup(completed, &operator_events)?;
            if scenario == Scenario::PrimaryAppFailover {
                let promoted = events
                    .iter()
                    .find(|event| event["event"] == "primary_app_failover")
                    .context("missing promoted primary")?;
                ensure!(
                    operator_events
                        .iter()
                        .any(|event| event["event"] == "lifecycle_complete"
                            && event["set_uid"] == completed["set_uid"]
                            && event["primary_node_id"] == promoted["primary_node_id"]),
                    "native lifecycle completion did not retain the promoted primary"
                );
            }
        }
        ensure!(
            scenario != Scenario::StaggeredShutdown
                || events
                    .iter()
                    .any(|event| event["event"] == "staggered_node_shutdown"),
            "successful driver omitted independent staggered termination verification"
        );
        let remaining: Value = serde_json::from_str(&self.kubectl(&[
            "-n",
            namespace,
            "get",
            "pods,configmaps,services,kubericsets",
            "-l",
            "operator.kuberic.io/set-uid",
            "-o",
            "json",
        ])?)?;
        ensure!(
            remaining["items"]
                .as_array()
                .context("owned object list")?
                .is_empty(),
            "host still observes owned workload resources"
        );
        ensure!(
            self.kubectl(&["-n", namespace, "get", "kubericsets", "-o", "name"])?
                .is_empty(),
            "host still observes the finalizing KubericSet"
        );
        Ok(())
    }

    fn operator_uid(&self, namespace: &str) -> Result<String> {
        let pods: Value = serde_json::from_str(&self.kubectl(&[
            "-n",
            namespace,
            "get",
            "pods",
            "-l",
            "app.kubernetes.io/name=kuberic-operator",
            "-o",
            "json",
        ])?)?;
        let pods = pods["items"].as_array().context("operator Pods")?;
        ensure!(
            pods.len() == 1,
            "qualification requires one original operator Pod"
        );
        ensure!(
            pods[0]["status"]["containerStatuses"]
                .as_array()
                .is_some_and(|containers| containers.len() == 1
                    && containers[0]["restartCount"] == 0
                    && containers[0]["state"]["running"].is_object()),
            "original operator is not continuously running"
        );
        Ok(pods[0]["metadata"]["uid"]
            .as_str()
            .context("operator UID")?
            .into())
    }

    fn crash_primary_app(&self, namespace: &str, event: &Value) -> Result<String> {
        let cohort: kuberic_operator::FrozenCohort =
            serde_json::from_value(event["cohort"].clone())?;
        cohort.fm_config()?;
        let set: kuberic_operator::KubericSet = serde_json::from_str(&self.kubectl(&[
            "-n",
            namespace,
            "get",
            "kubericset",
            &cohort.allocation.resource_name,
            "-o",
            "json",
        ])?)?;
        ensure!(
            set.uid().as_deref() == Some(&cohort.allocation.resource_uid)
                && set.namespace().as_deref() == Some(namespace)
                && event["set_uid"] == cohort.allocation.resource_uid
                && set
                    .status
                    .as_ref()
                    .is_some_and(|status| status.cohort().as_ref() == Some(&cohort)
                        && status.phase == kuberic_operator::ControllerPhase::Serving
                        && status.primary_node_id.as_ref()
                            == Some(&cohort.allocation.nodes[1].node_id)),
            "fault request changed original Serving authority"
        );
        let name = &cohort.allocation.nodes[1].pod_name;
        let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_str(
            &self.kubectl(&["-n", namespace, "get", "pod", name, "-o", "json"])?,
        )?;
        ensure!(
            pod.uid().as_deref() == Some(&cohort.bindings[1].pod_uid)
                && pod.metadata.deletion_timestamp.is_none()
                && pod
                    .owner_references()
                    .iter()
                    .any(|owner| owner.controller == Some(true)
                        && owner.uid == cohort.allocation.resource_uid)
                && pod.labels().get(kuberic_operator::resources::POD_UID)
                    == Some(&cohort.bindings[1].pod_uid)
                && pod.status.as_ref().is_some_and(|status| status
                    .container_statuses
                    .as_ref()
                    .is_some_and(|containers| containers.len() == 1
                        && containers[0].restart_count == 0
                        && containers[0]
                            .state
                            .as_ref()
                            .is_some_and(|state| state.running.is_some()))),
            "fault target is not the running original primary Pod"
        );
        let operator = self.operator_uid(namespace)?;
        let logs = self.kubectl(&[
            "-n",
            namespace,
            "exec",
            name,
            "-c",
            "node",
            "--",
            "/opt/fube/kuberic-kind",
            "crash-primary-app",
            "--cohort",
            &serde_json::to_string(&cohort)?,
        ])?;
        let report: Value = serde_json::from_str(logs.trim())?;
        ensure!(
            report["event"] == "primary_app_crashed"
                && report["set_uid"] == cohort.allocation.resource_uid
                && report["pod_uid"] == cohort.bindings[1].pod_uid
                && report["application_pid"]
                    .as_u64()
                    .is_some_and(|pid| pid > 1),
            "fault injector omitted original application-crash evidence"
        );
        eprintln!("{logs}");
        Ok(operator)
    }
}

fn scenario_resources(
    namespace: &str,
    cluster: &str,
    image: &str,
    scenario: Scenario,
) -> Result<Value> {
    let arguments = match scenario {
        Scenario::Bootstrap => vec!["run"],
        Scenario::DeleteDuringBootstrap => vec!["run", "--early-delete"],
        Scenario::StaggeredShutdown => vec!["run", "--staggered-shutdown"],
        Scenario::PrimaryAppFailover => vec!["run", "--primary-app-failover"],
    };
    let scripted = scenario == Scenario::StaggeredShutdown;
    let rules = if scripted {
        json!([
            {"apiGroups": ["operator.kuberic.io"], "resources": ["kubericsets", "kubericsets/status", "kubericsets/finalizers"],
             "verbs": ["get", "list", "create", "update", "patch", "delete"]},
            {"apiGroups": [""], "resources": ["pods", "pods/finalizers", "configmaps", "services"],
             "verbs": ["get", "list", "create", "update", "patch", "delete"]}
        ])
    } else {
        json!([
            {"apiGroups": ["operator.kuberic.io"], "resources": ["kubericsets"],
             "verbs": ["get", "list", "create", "delete"]},
            {"apiGroups": [""], "resources": ["pods", "configmaps", "services"],
             "verbs": ["get", "list"]}
        ])
    };
    let mut resources = json!({"apiVersion": "v1", "kind": "List", "items": [
        {"apiVersion": "v1", "kind": "Namespace", "metadata": {
            "name": namespace, "labels": {"testing.kuberic.io/kind-owned": cluster}}},
        {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {
            "name": "driver", "namespace": namespace}},
        {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
         "metadata": {"name": "driver", "namespace": namespace}, "rules": rules},
        {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
         "metadata": {"name": "driver", "namespace": namespace},
         "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "driver"},
         "subjects": [{"kind": "ServiceAccount", "name": "driver", "namespace": namespace}]},
        {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "driver", "namespace": namespace},
         "spec": {"backoffLimit": 0, "activeDeadlineSeconds": 540, "template": {
            "spec": {"restartPolicy": "Never", "serviceAccountName": "driver",
                "containers": [{"name": "driver", "image": image, "imagePullPolicy": "Never",
                    "command": ["/sbin/tini", "-g", "--", "/opt/fube/kuberic-kind"],
                    "args": arguments, "env": [
                        {"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}},
                        {"name": "POD_IP", "valueFrom": {"fieldRef": {"fieldPath": "status.podIP"}}},
                        {"name": "KUBERIC_TEST_IMAGE", "value": image}
                    ]}]
            }
         }}}
    ]});
    if !scripted {
        let installation = kuberic_operator::deploy::installation(namespace, image)?;
        resources["items"]
            .as_array_mut()
            .context("scenario resources")?
            .extend(
                installation["items"]
                    .as_array()
                    .context("operator installation")?
                    .iter()
                    .cloned(),
            );
    }
    Ok(resources)
}

fn verify_operator_cleanup(completed: &Value, events: &[Value]) -> Result<()> {
    let errors: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "reconcile_error" && event["retry_observation"] != true)
        .collect();
    ensure!(
        errors.is_empty(),
        "healthy operator lifecycle reported unexpected errors: {errors:?}"
    );
    let cohort: kuberic_operator::FrozenCohort =
        serde_json::from_value(completed["cohort"].clone())?;
    let event = events
        .iter()
        .find(|event| {
            event["event"] == "lifecycle_complete"
                && event["set_uid"] == completed["set_uid"]
                && event["cohort"] == completed["cohort"]
                && event["native_joined"] == true
                && event["resources_absent"] == true
        })
        .context("operator omitted original native joining and cleanup evidence")?;
    let reports: BTreeMap<String, kuberic_operator::node::Report> =
        serde_json::from_value(event["reports"].clone())?;
    ensure!(
        reports.len() == cohort.bindings.len(),
        "operator omitted original cleanup reports"
    );
    for (node, binding) in cohort.allocation.nodes.iter().zip(&cohort.bindings) {
        let report = reports
            .get(&binding.pod_uid)
            .context("missing original Pod cleanup report")?;
        ensure!(
            report.resource_uid == cohort.allocation.resource_uid
                && report.deployment == cohort.allocation.deployment
                && report.pod_uid == binding.pod_uid
                && report.node_id == node.node_id,
            "operator cleanup evidence belongs to another original cohort"
        );
        report.receipt()?;
    }
    Ok(())
}

pub fn prepare_image(cluster: &Cluster) -> Result<String> {
    cluster.verify()?;
    let mut artifacts = build(
        "Cargo.toml",
        &[
            ("kuberic-kind-tests", "kuberic-kind"),
            ("kuberic-operator", "kuberic-operator"),
            ("kuberic-operator", "kuberic-node"),
        ],
    )?;
    artifacts.extend(build(
        "fube/Cargo.toml",
        &[
            ("fube-node-runner", "fube-node"),
            ("hosting-local-app", "hosting-local-app"),
        ],
    )?);
    let context = tempfile::Builder::new()
        .prefix("kind-image-")
        .tempdir_in(root().join("target"))?;
    for (binary, target) in [
        ("kuberic-kind", "kuberic-kind"),
        ("kuberic-operator", "kuberic-operator"),
        ("kuberic-node", "kuberic-node"),
        ("fube-node", "fube-node-real"),
    ] {
        fs::copy(
            artifacts.get(binary).context("missing image executable")?,
            context.path().join(target),
        )?;
    }
    let package = context.path().join("package/LocalService/Code");
    fs::create_dir_all(&package)?;
    fs::copy(
        artifacts
            .get("hosting-local-app")
            .context("missing sample application")?,
        package.join("hosting-local-app"),
    )?;
    for relative in [
        "ApplicationManifest.xml",
        "LocalService/ServiceManifest.xml",
    ] {
        fs::copy(
            root()
                .join("fube/crates/samples/hosting-local-app/pkg-stateful")
                .join(relative),
            context.path().join("package").join(relative),
        )?;
    }
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let base = format!("localhost/kuberic-operator:{tag}");
    checked(
        Command::new("docker")
            .args(["build", "--tag", &base, "--file"])
            .arg(root().join("kuberic-operator/deploy/Dockerfile"))
            .arg(context.path()),
    )?;
    let image = format!("localhost/kuberic-kind:{tag}");
    checked(
        Command::new("docker")
            .args([
                "build",
                "--tag",
                &image,
                "--build-arg",
                &format!("OPERATOR_IMAGE={base}"),
                "--file",
            ])
            .arg(root().join("kuberic-kind-tests/deploy/Dockerfile"))
            .arg(context.path()),
    )?;
    checked(Command::new("kind").args(["load", "docker-image", &image, "--name", &cluster.name]))?;
    Ok(image)
}

fn build(manifest: &str, targets: &[(&str, &str)]) -> Result<BTreeMap<String, PathBuf>> {
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root())
        .args(["build", "--locked", "--manifest-path"])
        .arg(root().join(manifest))
        .args([
            "--bins",
            "--message-format=json-render-diagnostics",
            "--target-dir",
        ])
        .arg(root().join("target/kind-fixtures"))
        .env("CARGO_PROFILE_DEV_DEBUG", "0")
        .env("CARGO_INCREMENTAL", "0")
        .stderr(Stdio::inherit());
    let mut packages = std::collections::BTreeSet::new();
    for (package, _) in targets {
        if packages.insert(*package) {
            command.args(["-p", package]);
        }
    }
    let output = checked(&mut command)?;
    let mut artifacts = BTreeMap::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line)?;
        if message["reason"] == "compiler-artifact" {
            if let (Some(name), Some(path)) = (
                message["target"]["name"].as_str(),
                message["executable"].as_str(),
            ) {
                artifacts.insert(name.to_owned(), PathBuf::from(path).canonicalize()?);
            }
        }
    }
    for (_, binary) in targets {
        ensure!(artifacts.contains_key(*binary), "Cargo omitted {binary}");
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_shared_or_ambiguous_cluster_targets() {
        let valid = Cluster {
            name: "kuberic-kind-test".into(),
            context: "kind-kuberic-kind-test".into(),
            kubeconfig: root().join("target/kind/test.kubeconfig"),
        };
        valid.validate().unwrap();
        for name in [
            "kind",
            "shared",
            "kuberic-test/other",
            "kuberic-test\n",
            "kuberic-test-",
        ] {
            let mut changed = valid.clone();
            changed.name = name.into();
            changed.context = format!("kind-{name}");
            assert!(changed.validate().is_err());
        }
        for path in ["relative", "/tmp/a:/tmp/b", "/tmp/../config"] {
            let mut changed = valid.clone();
            changed.kubeconfig = path.into();
            assert!(changed.validate().is_err());
        }
        let mut changed = valid.clone();
        changed.kubeconfig = Path::new(&std::env::var("HOME").unwrap()).join(".kube/config");
        assert!(changed.validate().is_err());
        changed = valid;
        changed.context = "kind-somebody-else".into();
        assert!(changed.validate().is_err());
    }
}
