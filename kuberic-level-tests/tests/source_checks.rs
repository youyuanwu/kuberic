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

impl Cargo {
    fn command(&self, directory: &Path, subcommand: &str) -> Command {
        let mut command = Command::new("cargo");
        command
            .arg(subcommand)
            .current_dir(directory)
            .env_remove("KUBERIC_WORKSPACE_TESTS")
            .env("RUSTUP_TOOLCHAIN", &self.toolchain)
            .env("CARGO_TARGET_DIR", &self.target);
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
    let toolchain = pinned_toolchain(root)?;
    let output = checked_output(
        Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
            .current_dir(root)
            .env("RUSTUP_TOOLCHAIN", &toolchain),
    )?;
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let scratch = tempfile::Builder::new()
        .prefix("kuberic-test-util-consumer-")
        .tempdir()?;
    let scratch_path = scratch.path().canonicalize()?;
    ensure!(
        !scratch_path.starts_with(root.canonicalize()?),
        "consumer must be outside the checkout to avoid inheriting Cargo configuration",
    );
    let cargo = Cargo {
        toolchain,
        target: metadata.target_directory.join("test-util-consumer"),
        vendor: scratch_path.join("vendor"),
    };
    checked_output(
        cargo
            .command(root, "vendor")
            .args(["--locked", "--versioned-dirs", "--quiet"])
            .arg(&cargo.vendor),
    )?;
    for name in ["kuberic-runtime", "kuberic-controller"] {
        checked_output(
            cargo
                .packaged_command(root, "package")?
                .args([
                    "-p",
                    name,
                    "--allow-dirty",
                    "--no-verify",
                    "--offline",
                    "--quiet",
                ])
                .env("CARGO_TARGET_DIR", &metadata.target_directory),
        )?;
        unpack_package(name, &cargo.vendor, &metadata.target_directory)?;
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
