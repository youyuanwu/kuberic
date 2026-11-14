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
use kube::CustomResourceExt;
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
        self.apply(&serde_json::to_value(kuberic_controller::KubericSet::crd())?)?;
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
        let resources = scenario_resources(&namespace, &self.name, image, scenario);
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
        loop {
            let job: Value = serde_json::from_str(
                &self.kubectl(&["-n", namespace, "get", "job", "driver", "-o", "json"])?,
            )?;
            ensure!(
                job["status"]["failed"].as_u64().unwrap_or(0) == 0,
                "in-cluster driver failed: {}",
                job["status"]
            );
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
        ensure!(
            events
                .iter()
                .any(|event| event["event"] == "scenario_complete"
                    && event["scenario"] == scenario.name()
                    && event["native_joined"] == true
                    && event["resources_absent"] == true),
            "successful driver omitted final lifecycle verification"
        );
        ensure!(
            scenario == Scenario::DeleteDuringBootstrap
                || events
                    .iter()
                    .any(|event| event["event"] == "application_write_read"),
            "successful bootstrap omitted application write/read"
        );
        ensure!(
            events
                .iter()
                .any(|event| event["event"] == "physical_cleanup"),
            "successful driver omitted physical cleanup evidence"
        );
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
}

fn scenario_resources(namespace: &str, cluster: &str, image: &str, scenario: Scenario) -> Value {
    let arguments = match scenario {
        Scenario::Bootstrap => vec!["run"],
        Scenario::DeleteDuringBootstrap => vec!["run", "--early-delete"],
        Scenario::StaggeredShutdown => vec!["run", "--staggered-shutdown"],
    };
    json!({"apiVersion": "v1", "kind": "List", "items": [
        {"apiVersion": "v1", "kind": "Namespace", "metadata": {
            "name": namespace, "labels": {"testing.kuberic.io/kind-owned": cluster}}},
        {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {
            "name": "driver", "namespace": namespace}},
        {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
         "metadata": {"name": "driver", "namespace": namespace}, "rules": [
            {"apiGroups": ["operator.kuberic.io"], "resources": ["kubericsets", "kubericsets/status", "kubericsets/finalizers"],
             "verbs": ["get", "list", "create", "update", "patch", "delete"]},
            {"apiGroups": [""], "resources": ["pods", "pods/finalizers", "configmaps", "services"],
             "verbs": ["get", "list", "create", "update", "patch", "delete"]}
         ]},
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
    ]})
}

pub fn prepare_image(cluster: &Cluster) -> Result<String> {
    cluster.verify()?;
    let mut artifacts = build(
        "Cargo.toml",
        &[
            ("kuberic-kind-tests", "kuberic-kind"),
            ("kuberic-kind-tests", "kuberic-kind-node"),
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
        ("kuberic-kind-node", "fube-node"),
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
    let image = format!("localhost/kuberic-kind:{}", uuid::Uuid::new_v4().simple());
    checked(
        Command::new("docker")
            .args(["build", "--tag", &image, "--file"])
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
