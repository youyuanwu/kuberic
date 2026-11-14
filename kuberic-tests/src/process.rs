use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use fube_local_host::process::OwnedApp;
use kuberic_operator::{ReconcileOutcome, Reconciler, StaleObservation};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};

pub mod controller;
pub mod fm;

pub struct Binaries {
    pub node: PathBuf,
    pub app: PathBuf,
    pub fm: PathBuf,
}

pub fn binaries() -> &'static Binaries {
    static BINARIES: OnceLock<Result<Binaries, String>> = OnceLock::new();
    BINARIES
        .get_or_init(|| build_binaries().map_err(|error| format!("{error:#}")))
        .as_ref()
        .unwrap_or_else(|error| panic!("preparing real-process test binaries: {error}"))
}

fn executable(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("locating executable {}", path.display()))?;
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "{} is not an executable file",
        path.display()
    );
    Ok(path)
}

fn override_binary(variable: &str) -> Result<Option<PathBuf>> {
    std::env::var_os(variable)
        .map(|path| executable(Path::new(&path)).with_context(|| format!("invalid {variable}")))
        .transpose()
}

fn build(
    root: &Path,
    manifest: &str,
    targets: &[(&str, &str)],
) -> Result<BTreeMap<String, PathBuf>> {
    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .args(["build", "--locked", "--manifest-path"])
        .arg(root.join(manifest))
        .args([
            "--bins",
            "--message-format=json-render-diagnostics",
            "--target-dir",
        ])
        .arg(root.join("target/process-fixtures"))
        .stderr(Stdio::inherit());
    for (package, _) in targets {
        command.args(["-p", package]);
    }
    let output = command
        .output()
        .with_context(|| format!("building fixtures from {manifest}"))?;
    ensure!(
        output.status.success(),
        "fixture build from {manifest} failed: {}",
        output.status
    );
    let mut artifacts = BTreeMap::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line).context("decoding Cargo build output")?;
        if message["reason"] == "compiler-artifact" {
            if let (Some(name), Some(path)) = (
                message["target"]["name"].as_str(),
                message["executable"].as_str(),
            ) {
                artifacts.insert(name.to_owned(), executable(Path::new(path))?);
            }
        }
    }
    for (_, binary) in targets {
        ensure!(
            artifacts.contains_key(*binary),
            "Cargo omitted fixture binary {binary}"
        );
    }
    Ok(artifacts)
}

fn build_binaries() -> Result<Binaries> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("missing workspace root")?;
    let node = override_binary("FUBE_NODE_EXE")?;
    let app = override_binary("FUBE_NODE_APP")?;
    let fm = override_binary("KUBERIC_FM_EXE")?;
    let mut targets = Vec::new();
    if node.is_none() {
        targets.push(("fube-node-runner", "fube-node"));
    }
    if app.is_none() {
        targets.push(("hosting-local-app", "hosting-local-app"));
    }
    let mut artifacts = build(root, "fube/Cargo.toml", &targets)?;
    if fm.is_none() {
        artifacts.extend(build(root, "Cargo.toml", &[("kuberic-fm", "kuberic-fm")])?);
    }
    let mut resolve = |path: Option<PathBuf>, name: &str| {
        path.or_else(|| artifacts.remove(name))
            .with_context(|| format!("missing fixture binary {name}"))
    };
    Ok(Binaries {
        node: resolve(node, "fube-node")?,
        app: resolve(app, "hosting-local-app")?,
        fm: resolve(fm, "kuberic-fm")?,
    })
}

pub fn stage_package(root: &Path, app: &Path) -> PathBuf {
    let package = root.join("package");
    fs::create_dir_all(package.join("LocalService/Code")).unwrap();
    fs::write(
        package.join("ApplicationManifest.xml"),
        include_bytes!(
            "../../fube/crates/samples/hosting-local-app/pkg-stateful/ApplicationManifest.xml"
        ),
    )
    .unwrap();
    fs::write(
        package.join("LocalService/ServiceManifest.xml"),
        include_bytes!("../../fube/crates/samples/hosting-local-app/pkg-stateful/LocalService/ServiceManifest.xml"),
    )
    .unwrap();
    fs::copy(app, package.join("LocalService/Code/hosting-local-app")).unwrap();
    package
}

pub fn reserve(ip: Ipv4Addr) -> SocketAddrV4 {
    let listener = std::net::TcpListener::bind((ip, 0)).unwrap();
    match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        _ => unreachable!(),
    }
}

pub fn start(executable: &Path, config: &Path) -> OwnedApp {
    let mut command = Command::new(executable);
    command.arg("--config").arg(config);
    let mut owner = None;
    OwnedApp::start(&mut owner, command).unwrap();
    owner.unwrap()
}

pub fn terminate(child: &OwnedApp) {
    assert!(Command::new("kill")
        .args(["-TERM", &child.pid().to_string()])
        .status()
        .unwrap()
        .success());
}

pub async fn finish(child: &mut OwnedApp) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.observed_exit().unwrap() {
            assert!(status.success(), "process failed: {:?}", child.logs());
            assert_eq!(child.cleanup().unwrap()["forced"], false);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "process exit deadline: {:?}",
            child.logs()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub async fn application(endpoint: &str, command: &str) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(endpoint).await.unwrap();
        stream
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        response
    })
    .await
    .unwrap()
}

pub async fn reconcile(controller: &Reconciler, name: &str) -> Result<ReconcileOutcome> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match controller.reconcile(name).await {
                Err(error) if error.is::<StaleObservation>() => {
                    eprintln!("Retrying reconciliation: {error}");
                    tokio::task::yield_now().await;
                }
                result => return result,
            }
        }
    })
    .await
    .context("reconciliation did not reach a stable observation")?
}
