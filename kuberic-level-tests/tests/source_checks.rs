use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Metadata {
    target_directory: PathBuf,
}

struct Cargo {
    toolchain: OsString,
    target: PathBuf,
    vendor: PathBuf,
}

struct ConsumerScratch {
    source: tempfile::TempDir,
    target: tempfile::TempDir,
}

fn external_source_parent(root: &Path, scratch_parent: &Path) -> Result<PathBuf> {
    let parent = scratch_parent.canonicalize()?;
    ensure!(
        !parent.starts_with(root.canonicalize()?),
        "consumer TMPDIR must be outside the checkout to avoid inheriting Cargo configuration",
    );
    Ok(parent)
}

impl ConsumerScratch {
    fn new(root: &Path, target: &Path, scratch_parent: &Path) -> Result<Self> {
        // Validate before allocating either owner. Never change inherited TMPDIR
        // or CARGO_TARGET_DIR; only build commands receive an owned --target-dir.
        let source_parent = external_source_parent(root, scratch_parent)?;
        let source = tempfile::Builder::new()
            .prefix("kuberic-test-util-consumer-")
            .tempdir_in(source_parent)?;
        let target = tempfile::Builder::new()
            .prefix("kuberic-test-util-build-")
            .tempdir_in(target)?;
        Ok(Self { source, target })
    }
}

fn check_consumer_placement(root: &Path) -> Result<()> {
    let inherited = (
        std::env::var_os("CARGO_TARGET_DIR"),
        std::env::var_os("TMPDIR"),
    );
    let scratch_parent = std::env::temp_dir();
    let external = external_source_parent(root, &scratch_parent)?;
    let owned = tempfile::Builder::new()
        .prefix("kuberic-consumer-placement-")
        .tempdir_in(&external)?;
    let checkout = owned.path().join("checkout");
    fs::create_dir(&checkout)?;
    // Representative default, documented checkout-local, and external cache
    // layouts. These probes allocate owners but never build in those targets.
    for target in [
        checkout.join("target"),
        checkout.join("target/m2-validation-owned/build"),
        owned.path().join("external-cache"),
    ] {
        fs::create_dir_all(&target)?;
        let fixture = ConsumerScratch::new(&checkout, &target, &scratch_parent)?;
        let source_path = fixture.source.path().canonicalize()?;
        let target_path = fixture.target.path().canonicalize()?;
        ensure!(!source_path.starts_with(root.canonicalize()?));
        ensure!(!source_path.starts_with(&checkout));
        ensure!(source_path.parent() == Some(external.as_path()));
        ensure!(target_path.parent() == Some(target.canonicalize()?.as_path()));
        ensure!(source_path != target_path);
        let cargo = Cargo {
            toolchain: "placement-probe-only".into(),
            target: target_path.clone(),
            vendor: source_path.join("vendor"),
        };
        let command = cargo.command(&source_path, "check");
        ensure!(
            command
                .get_envs()
                .all(|(name, _)| { name != "CARGO_TARGET_DIR" && name != "TMPDIR" })
        );
        let args = command.get_args().collect::<Vec<_>>();
        ensure!(
            args.windows(2)
                .any(|pair| { pair[0] == "--target-dir" && pair[1] == target_path.as_os_str() })
        );
        drop(fixture);
        ensure!(!source_path.exists() && !target_path.exists());
    }
    let before = fs::read_dir(&checkout)?.count();
    ensure!(ConsumerScratch::new(&checkout, &checkout, &checkout).is_err());
    ensure!(fs::read_dir(&checkout)?.count() == before);
    ensure!(
        inherited
            == (
                std::env::var_os("CARGO_TARGET_DIR"),
                std::env::var_os("TMPDIR")
            )
    );
    Ok(())
}

impl Cargo {
    fn command(&self, directory: &Path, subcommand: &str) -> Command {
        let mut command = Command::new("cargo");
        command
            .arg(subcommand)
            .current_dir(directory)
            .env_remove("KUBERIC_WORKSPACE_TESTS")
            .env("RUSTUP_TOOLCHAIN", &self.toolchain);
        if matches!(subcommand, "check" | "clippy" | "package" | "test") {
            command.arg("--target-dir").arg(&self.target);
        }
        if std::env::var_os("CARGO_BUILD_JOBS").is_none() {
            command.env("CARGO_BUILD_JOBS", "2");
        }
        command
    }

    fn packaged_command(&self, directory: &Path, subcommand: &str) -> Result<Command> {
        let mut command = self.command(directory, subcommand);
        // Clippy is an external Cargo subcommand and must receive these arguments itself.
        command
            .args([
                "--config",
                "source.crates-io.replace-with=\"test-util-packages\"",
            ])
            .arg("--config")
            .arg(format!(
                "source.test-util-packages.directory={}",
                serde_json::to_string(&self.vendor)?
            ));
        Ok(command)
    }
}

fn checked_output(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("could not execute {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed with {}:\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(output)
}

fn negative(command: &mut Command, expected: &[&str]) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("could not execute {command:?}"))?;
    ensure!(
        !output.status.success(),
        "negative compiler fixture unexpectedly passed: {command:?}",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for fragment in expected {
        ensure!(
            stderr.contains(fragment),
            "{command:?} failed for an unexpected reason; missing {fragment:?}:\n{}\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
    }
    Ok(())
}

fn pinned_toolchain(root: &Path) -> Result<OsString> {
    if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        return Ok(toolchain);
    }
    let output = checked_output(
        Command::new("rustup")
            .args(["show", "active-toolchain"])
            .current_dir(root),
    )?;
    let output = std::str::from_utf8(&output.stdout)?;
    let toolchain = output
        .split_whitespace()
        .next()
        .context("rustup reported no active toolchain for the checkout")?;
    Ok(toolchain.into())
}

fn sha256(path: &Path) -> Result<String> {
    let contents = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let digest = Bytes::copy_from_slice(&Sha256::digest(contents));
    Ok(format!("{digest:x}"))
}

fn checksums(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            checksums(root, &path, files)?;
        } else if kind.is_file() {
            if entry.file_name() != ".cargo-checksum.json" {
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF-8 package path")?;
                files.insert(relative.replace('\\', "/"), sha256(&path)?);
            }
        } else {
            bail!("unsupported package entry: {}", path.display());
        }
    }
    Ok(())
}

fn unpack_package(name: &str, vendor: &Path, target: &Path) -> Result<()> {
    let package = format!("{name}-{}", env!("CARGO_PKG_VERSION"));
    let archive = target.join("package").join(format!("{package}.crate"));
    let destination = vendor.join(&package);
    ensure!(
        !destination.exists(),
        "package destination already exists: {}",
        destination.display(),
    );
    // The archive was just produced by Cargo from this checkout.
    checked_output(
        Command::new("tar")
            .arg("--extract")
            .arg("--file")
            .arg(&archive)
            .arg("--directory")
            .arg(vendor),
    )?;
    let mut files = BTreeMap::new();
    checksums(&destination, &destination, &mut files)?;
    fs::write(
        destination.join(".cargo-checksum.json"),
        serde_json::to_vec(&serde_json::json!({
            "files": files,
            "package": sha256(&archive)?,
        }))?,
    )?;
    Ok(())
}

fn copy_sources(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_sources(&source, &destination)?;
        } else if kind.is_file() {
            fs::copy(&source, &destination)?;
        } else {
            bail!("unsupported fixture entry: {}", source.display());
        }
    }
    Ok(())
}

#[test]
#[ignore = "source: requires checkout, Cargo and package inputs"]
fn packaged_controller_test_util_boundary() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("level tests must be inside the workspace")?;
    check_consumer_placement(root)?;
    // Fail unsupported checkout-local TMPDIR before invoking Cargo or writing.
    let scratch_parent = external_source_parent(root, &std::env::temp_dir())?;
    let toolchain = pinned_toolchain(root)?;
    let output = checked_output(
        Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
            .current_dir(root)
            .env("RUSTUP_TOOLCHAIN", &toolchain),
    )?;
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let scratch = ConsumerScratch::new(root, &metadata.target_directory, &scratch_parent)?;
    let scratch_path = scratch.source.path().canonicalize()?;
    ensure!(
        !scratch_path.starts_with(root.canonicalize()?),
        "consumer must be outside the checkout to avoid inheriting Cargo configuration",
    );
    let cargo = Cargo {
        toolchain,
        target: scratch.target.path().canonicalize()?,
        vendor: scratch_path.join("vendor"),
    };
    // Separate invocations keep test/dev dependencies from enabling runtime hosting.
    for (package, target) in [
        ("kuberic-runtime", "--lib"),
        ("kuberic-controller", "--bins"),
    ] {
        checked_output(cargo.command(root, "check").args([
            "--locked",
            "--offline",
            "--no-default-features",
            "--package",
            package,
            target,
        ]))?;
    }
    checked_output(
        cargo
            .command(root, "vendor")
            .args(["--locked", "--versioned-dirs", "--quiet"])
            .arg(&cargo.vendor),
    )?;
    for name in ["kuberic-runtime", "kuberic-controller"] {
        checked_output(cargo.packaged_command(root, "package")?.args([
            "-p",
            name,
            "--allow-dirty",
            "--no-verify",
            "--offline",
            "--quiet",
        ]))?;
        unpack_package(name, &cargo.vendor, &cargo.target)?;
    }

    let fixture = root.join("scripts/fixtures/controller-test-util");
    let consumer = scratch_path.join("consumer");
    fs::create_dir(&consumer)?;
    fs::copy(fixture.join("Cargo.toml"), consumer.join("Cargo.toml"))?;
    copy_sources(&fixture.join("src"), &consumer.join("src"))?;

    checked_output(cargo.packaged_command(&consumer, "check")?.args([
        "--lib",
        "--no-default-features",
        "--offline",
        "--quiet",
    ]))?;
    negative(
        cargo.packaged_command(&consumer, "check")?.args([
            "--bin",
            "feature-off",
            "--no-default-features",
            "--offline",
            "--quiet",
        ]),
        &["test_util", "unresolved import"],
    )?;
    checked_output(cargo.packaged_command(&consumer, "clippy")?.args([
        "--lib",
        "--all-features",
        "--offline",
        "--quiet",
        "--",
        "-D",
        "warnings",
    ]))?;
    negative(
        cargo.packaged_command(&consumer, "check")?.args([
            "--bin",
            "private",
            "--offline",
            "--quiet",
        ]),
        &[
            "module `transport` is private",
            "module `hosting` is private",
        ],
    )?;
    negative(
        cargo.packaged_command(&consumer, "check")?.args([
            "--bin",
            "conformance",
            "--offline",
            "--quiet",
        ]),
        &[
            "unresolved import",
            "no `assert_public_v1_conformance_subject`",
        ],
    )?;
    negative(
        cargo
            .packaged_command(&consumer, "check")?
            .args(["--lib", "--offline", "--quiet"])
            .env("KUBERIC_WORKSPACE_TESTS", "1"),
        &[
            "requests repository conformance",
            "Unset KUBERIC_WORKSPACE_TESTS",
        ],
    )?;
    // Shared behavior bodies run in the archive; here only their packaged closure is compiled.
    checked_output(cargo.packaged_command(&consumer, "test")?.args([
        "--lib",
        "--no-run",
        "--offline",
        "--quiet",
    ]))?;
    Ok(())
}
