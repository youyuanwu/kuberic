#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
temporary="$repo_root/target/level-guards-$$"
mkdir -p "$temporary"
trap 'rm -rf -- "$temporary"' EXIT

new_git_repo() {
    local directory=$1
    mkdir -p "$directory/scripts" "$directory/kuberic-core"
    cp "$repo_root/scripts/check_level_triggered_scope.sh" "$directory/scripts/"
    (
        cd "$directory"
        git init -q -b main
        git config user.email test@example.invalid
        git config user.name "Guard Test"
        printf 'baseline\n' > kuberic-core/file.rs
        git add kuberic-core/file.rs
        git commit -q -m baseline
    )
}

expect_scope_failure() {
    local directory=$1
    local base=$2
    if (cd "$directory" && scripts/check_level_triggered_scope.sh "$base"); then
        echo "Scope guard accepted a protected v1 change in $directory." >&2
        exit 1
    fi
}

committed="$temporary/committed"
new_git_repo "$committed"
(
    cd "$committed"
    base=$(git rev-parse HEAD)
    git switch -q -c feature
    mkdir new-stack
    git mv kuberic-core/file.rs new-stack/file.rs
    git commit -q -m rename
    printf '%s\n' "$base" > "$temporary/committed-base"
)
expect_scope_failure "$committed" "$(cat "$temporary/committed-base")"

staged="$temporary/staged"
new_git_repo "$staged"
(
    cd "$staged"
    mkdir new-stack
    git mv kuberic-core/file.rs "new-stack/quoted file.rs"
)
expect_scope_failure "$staged" HEAD

unstaged="$temporary/unstaged"
new_git_repo "$unstaged"
printf 'changed\n' >> "$unstaged/kuberic-core/file.rs"
expect_scope_failure "$unstaged" HEAD

untracked="$temporary/untracked"
new_git_repo "$untracked"
printf 'new\n' > "$untracked/kuberic-core/untracked file.rs"
expect_scope_failure "$untracked" HEAD

# SQLite is the same package, now migrated in place rather than protected v1.
sqlite_scope="$temporary/sqlite-scope"
new_git_repo "$sqlite_scope"
mkdir -p "$sqlite_scope/examples/sqlite/src"
printf 'pub fn sqlite() {}\n' > "$sqlite_scope/examples/sqlite/src/lib.rs"
(cd "$sqlite_scope" && scripts/check_level_triggered_scope.sh HEAD)
(cd "$sqlite_scope" && git add examples/sqlite/src/lib.rs && scripts/check_level_triggered_scope.sh HEAD)

new_cargo_repo() {
    local directory=$1
    mkdir -p "$directory/scripts" "$directory/kuberic-protocol/src" \
        "$directory/kuberic-core/src"
    cp "$repo_root/scripts/check_level_triggered_dependencies.sh" "$directory/scripts/"
    cat > "$directory/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["kuberic-protocol"]
EOF
    cat > "$directory/kuberic-protocol/Cargo.toml" <<'EOF'
[package]
name = "kuberic-protocol"
version = "0.1.0"
edition = "2024"
EOF
    printf 'pub fn protected() {}\n' > "$directory/kuberic-core/src/types.rs"
}

expect_dependency_failure() {
    local directory=$1
    if (cd "$directory" && scripts/check_level_triggered_dependencies.sh); then
        echo "Dependency guard accepted a protected v1 import in $directory." >&2
        exit 1
    fi
}

direct="$temporary/direct"
new_cargo_repo "$direct"
cat >> "$direct/Cargo.toml" <<'EOF'
exclude = ["kuberic-core"]
EOF
cat >> "$direct/kuberic-protocol/Cargo.toml" <<'EOF'
[dependencies]
kuberic-core = { path = "../kuberic-core" }
EOF
cat > "$direct/kuberic-core/Cargo.toml" <<'EOF'
[package]
name = "kuberic-core"
version = "0.1.0"
edition = "2024"
EOF
printf 'pub fn protocol() {}\n' > "$direct/kuberic-protocol/src/lib.rs"
expect_dependency_failure "$direct"

path_attribute="$temporary/path-attribute"
new_cargo_repo "$path_attribute"
cat > "$path_attribute/kuberic-protocol/src/lib.rs" <<'EOF'
#[path = "../../kuberic-core/src/types.rs"]
mod protected;
EOF
expect_dependency_failure "$path_attribute"

multiline_include="$temporary/multiline-include"
new_cargo_repo "$multiline_include"
cat > "$multiline_include/kuberic-protocol/src/lib.rs" <<'EOF'
const PROTECTED: &str = include_str!(
    "../../kuberic-core/src/types.rs"
);
EOF
expect_dependency_failure "$multiline_include"

nested_include="$temporary/nested-include"
new_cargo_repo "$nested_include"
cat > "$nested_include/kuberic-protocol/src/lib.rs" <<'EOF'
const PROTECTED: &str = include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../kuberic-core/src/types.rs"
));
EOF
expect_dependency_failure "$nested_include"

cfg_attr_path="$temporary/cfg-attr-path"
new_cargo_repo "$cfg_attr_path"
cat > "$cfg_attr_path/kuberic-protocol/src/lib.rs" <<'EOF'
#[cfg_attr(all(), path = "../../kuberic-core/src/types.rs")]
mod protected;
EOF
expect_dependency_failure "$cfg_attr_path"

symlink_import="$temporary/symlink-import"
new_cargo_repo "$symlink_import"
printf 'pub fn protocol() {}\n' > "$symlink_import/kuberic-protocol/src/lib.rs"
ln -s ../../kuberic-core/src/types.rs \
    "$symlink_import/kuberic-protocol/src/protected.rs"
expect_dependency_failure "$symlink_import"

new_sqlite_repo() {
    local directory=$1
    new_cargo_repo "$directory"
    mkdir -p "$directory/examples"
    mv "$directory/kuberic-protocol" "$directory/examples/sqlite"
    sed -i 's/"kuberic-protocol"/"examples\/sqlite"/' "$directory/Cargo.toml"
    sed -i 's/name = "kuberic-protocol"/name = "sqlite-replicated"/' \
        "$directory/examples/sqlite/Cargo.toml"
    printf 'pub fn sqlite() {}\n' > "$directory/examples/sqlite/src/lib.rs"
}

sqlite_dependency="$temporary/sqlite-dependency"
new_sqlite_repo "$sqlite_dependency"
(cd "$sqlite_dependency" && scripts/check_level_triggered_dependencies.sh)
cat >> "$sqlite_dependency/Cargo.toml" <<'EOF'
exclude = ["kuberic-core"]
EOF
cat > "$sqlite_dependency/kuberic-core/Cargo.toml" <<'EOF'
[package]
name = "kuberic-core"
version = "0.1.0"
edition = "2024"
EOF
printf 'pub fn classic() {}\n' > "$sqlite_dependency/kuberic-core/src/lib.rs"
cat >> "$sqlite_dependency/examples/sqlite/Cargo.toml" <<'EOF'
[dependencies]
kuberic-core = { path = "../../kuberic-core" }
EOF
expect_dependency_failure "$sqlite_dependency"

# Reject both classic runtime and operator dependencies on the migrated package.
mv "$sqlite_dependency/kuberic-core" "$sqlite_dependency/kuberic-operator"
sed -i 's/kuberic-core/kuberic-operator/g' \
    "$sqlite_dependency/Cargo.toml" \
    "$sqlite_dependency/examples/sqlite/Cargo.toml" \
    "$sqlite_dependency/kuberic-operator/Cargo.toml"
expect_dependency_failure "$sqlite_dependency"

sqlite_source="$temporary/sqlite-source"
new_sqlite_repo "$sqlite_source"
cat > "$sqlite_source/examples/sqlite/src/lib.rs" <<'EOF'
include!("../../../kuberic-core/src/types.rs");
EOF
expect_dependency_failure "$sqlite_source"

sqlite_symlink="$temporary/sqlite-symlink"
new_sqlite_repo "$sqlite_symlink"
ln -s ../../../kuberic-core/src/types.rs "$sqlite_symlink/examples/sqlite/src/classic.rs"
expect_dependency_failure "$sqlite_symlink"

postgres_scope="$temporary/postgres-scope"
new_git_repo "$postgres_scope"
mkdir -p "$postgres_scope/examples/postgres/src"
printf 'pub fn postgres() {}\n' > "$postgres_scope/examples/postgres/src/lib.rs"
(cd "$postgres_scope" && scripts/check_level_triggered_scope.sh HEAD)
(cd "$postgres_scope" && git add examples/postgres/src/lib.rs && scripts/check_level_triggered_scope.sh HEAD)
(cd "$postgres_scope" && git commit -q -m postgres-v2 && scripts/check_level_triggered_scope.sh HEAD~1)
printf '// v2 edit\n' >> "$postgres_scope/examples/postgres/src/lib.rs"
(cd "$postgres_scope" && scripts/check_level_triggered_scope.sh HEAD)
mkdir -p "$postgres_scope/examples/postgres/deploy"
printf 'kind: Pod\n' > "$postgres_scope/examples/postgres/deploy/sample.yaml"
expect_scope_failure "$postgres_scope" HEAD
rm "$postgres_scope/examples/postgres/deploy/sample.yaml"
rmdir "$postgres_scope/examples/postgres/deploy"
printf 'FROM postgres:16\n' > "$postgres_scope/examples/postgres/Dockerfile"
expect_scope_failure "$postgres_scope" HEAD

new_postgres_repo() {
    local directory=$1
    new_sqlite_repo "$directory"
    mv "$directory/examples/sqlite" "$directory/examples/postgres"
    sed -i 's/examples\/sqlite/examples\/postgres/' "$directory/Cargo.toml"
    sed -i 's/sqlite-replicated/postgres-replicated/' "$directory/examples/postgres/Cargo.toml"
}

for dependency in kuberic-core kuberic-operator; do
    postgres_dependency="$temporary/postgres-$dependency"
    new_postgres_repo "$postgres_dependency"
    (cd "$postgres_dependency" && scripts/check_level_triggered_dependencies.sh)
    mkdir -p "$postgres_dependency/$dependency/src"
    printf '\nexclude = ["%s"]\n' "$dependency" >> "$postgres_dependency/Cargo.toml"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\n' "$dependency" \
        > "$postgres_dependency/$dependency/Cargo.toml"
    printf 'pub fn classic() {}\n' > "$postgres_dependency/$dependency/src/lib.rs"
    printf '\n[dev-dependencies]\n%s = { path = "../../%s" }\n' "$dependency" "$dependency" \
        >> "$postgres_dependency/examples/postgres/Cargo.toml"
    expect_dependency_failure "$postgres_dependency"
done

postgres_source="$temporary/postgres-source"
new_postgres_repo "$postgres_source"
printf 'use kuberic_core::types::Role;\n' > "$postgres_source/examples/postgres/src/lib.rs"
expect_dependency_failure "$postgres_source"
printf 'include!("../../../kuberic-core/src/types.rs");\n' > "$postgres_source/examples/postgres/src/lib.rs"
expect_dependency_failure "$postgres_source"
printf 'pub fn postgres() {}\n' > "$postgres_source/examples/postgres/src/lib.rs"
ln -s ../../../kuberic-core/src/types.rs "$postgres_source/examples/postgres/src/classic.rs"
expect_dependency_failure "$postgres_source"

for dependency in kube k8s-openapi kuberic-level-tests testcontainers bollard; do
    postgres_cluster="$temporary/postgres-cluster-$dependency"
    new_postgres_repo "$postgres_cluster"
    mkdir -p "$postgres_cluster/$dependency/src"
    printf '\nexclude = ["%s"]\n' "$dependency" >> "$postgres_cluster/Cargo.toml"
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\n' "$dependency" \
        > "$postgres_cluster/$dependency/Cargo.toml"
    printf 'pub fn cluster() {}\n' > "$postgres_cluster/$dependency/src/lib.rs"
    printf '\n[dev-dependencies]\ncluster = { package = "%s", path = "../../%s" }\n' "$dependency" "$dependency" \
        >> "$postgres_cluster/examples/postgres/Cargo.toml"
    expect_dependency_failure "$postgres_cluster"
done

postgres_live_source="$temporary/postgres-live-source"
new_postgres_repo "$postgres_live_source"
mkdir -p "$postgres_live_source/kuberic-level-tests/src"
printf 'pub fn cluster() {}\n' > "$postgres_live_source/kuberic-level-tests/src/lib.rs"
cat > "$postgres_live_source/examples/postgres/src/lib.rs" <<'EOF'
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../kuberic-level-tests/src/lib.rs"
));
EOF
expect_dependency_failure "$postgres_live_source"
printf 'fn main() { std::process::Command::new("docker"); }\n' \
    > "$postgres_live_source/examples/postgres/src/lib.rs"
expect_dependency_failure "$postgres_live_source"
printf 'pub fn postgres() {}\n' > "$postgres_live_source/examples/postgres/src/lib.rs"
ln -s ../../../kuberic-level-tests/src/lib.rs "$postgres_live_source/examples/postgres/src/live.rs"
expect_dependency_failure "$postgres_live_source"

python3 - "$repo_root" "$temporary/postgres-docs" <<'PY'
import pathlib
import re
import shutil
import subprocess
import sys

root, fixture = map(pathlib.Path, sys.argv[1:])
paths = (
    "scripts/check_level_triggered_documentation.sh",
    ".github/workflows/level-triggered-CI.yml",
    "README.md", "docs/Dev.md", "docs/features/postgres/design.md",
    "docs/features/kuberic/testing.md", "docs/features/kuberic/level-triggered-operator.md",
    "docs/features/sqlserver/design.md", "docs/features/kuberic/design-gaps.md",
    "docs/proposal/v1-retirement-plan.md", "examples/kvstore2/deploy/sample.yaml",
    "kuberic-protocol/src/lib.rs", "kuberic-agent/src/state.rs", "justfile",
)
for relative in paths:
    target = fixture / relative
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(root / relative, target)
shutil.copytree(root / "kuberic-level-tests", fixture / "kuberic-level-tests")

def check():
    return subprocess.run(
        ["bash", "scripts/check_level_triggered_documentation.sh", "--postgres-contract-only"],
        cwd=fixture, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )

baseline = check()
assert baseline.returncode == 0, baseline.stdout
workflow = ".github/workflows/level-triggered-CI.yml"
design = "docs/features/postgres/design.md"
unit_command = "cargo test -p postgres-replicated --all-features -- --test-threads=1"
unit_step = "- name: Test PostgreSQL unit and host-local subprocess suites"
unit_error = "Test PostgreSQL unit and host-local subprocess suites: run must be a shell string"
deployment_error = "PostgreSQL fresh deployment must declare exactly protocol 9 / agent schema 5"
live_recipe = "level-triggered-images: verify-kind-context"
default_cargo_error = "Implicit/default workspace Cargo execution must not enter live recipe level-triggered-images"
cases = (
    ("missing unit command", workflow, unit_command, "true", unit_error),
    ("disabled unit step", workflow, unit_step, unit_step + "\n        if: ${{ false }}",
     "Test PostgreSQL unit and host-local subprocess suites must be unconditional"),
    ("comment-only unit command", workflow, "run: " + unit_command,
     "run: true # " + unit_command, unit_error),
    ("quoted comment-only command", workflow, "run: " + unit_command,
     'run: "true # ' + unit_command + '"',
     "PostgreSQL serial unit selection must execute the required command"),
    ("missing lint", workflow, "          -p postgres-replicated\n", "",
     "PostgreSQL targeted lint is required"),
    ("missing server", workflow, "postgresql-16 postgresql-client-16", "postgresql-client-16",
     "PostgreSQL 16 installation and version check must execute"),
    ("wrong runner", workflow, "runs-on: ubuntu-24.04", "runs-on: ubuntu-latest",
     "Use the Ubuntu PostgreSQL 16 package baseline"),
    ("missing version check", workflow, "/usr/lib/postgresql/16/bin/postgres --version", "true",
     "PostgreSQL 16 installation and version check must execute"),
    ("live smoke command", workflow, "run: just level-triggered-kind-test bootstrap",
     "run: " + unit_command, "PostgreSQL must not enter KinD/live job bootstrap-kind"),
    ("live full command", workflow, "run: just level-triggered-kind-test all",
     "run: " + unit_command, "PostgreSQL must not enter KinD/live job full-kind"),
    ("live images command", "justfile", "level-triggered-images: verify-kind-context",
     "level-triggered-images: verify-kind-context\n    " + unit_command,
     "PostgreSQL must not enter live recipe level-triggered-images"),
    ("transitive live dependency", "justfile", "verify-kind-ownership:",
     "verify-kind-ownership: unit-only\n\nunit-only:\n    " + unit_command,
     "PostgreSQL must not enter live recipe unit-only"),
    ("nested live invocation", "justfile", "level-triggered-images: verify-kind-context",
     "unit-only:\n    " + unit_command +
     "\n\nlevel-triggered-images: verify-kind-context\n    just unit-only",
     "PostgreSQL must not enter live recipe unit-only"),
    ("multiple live invocations", "justfile", "level-triggered-images: verify-kind-context",
     "unit-only:\n    " + unit_command +
     "\n\nlevel-triggered-images: verify-kind-context\n    just verify-kind-context unit-only",
     "PostgreSQL must not enter live recipe unit-only"),
    ("echo-suppressed live invocation", "justfile", live_recipe,
     "unit-only:\n    " + unit_command + "\n\n" + live_recipe + "\n    @just unit-only",
     "PostgreSQL must not enter live recipe unit-only"),
    ("adjacent live invocation", "justfile", live_recipe,
     "unit-only:\n    " + unit_command + "\n\n" + live_recipe + "\n    true;just unit-only",
     "PostgreSQL must not enter live recipe unit-only"),
    ("plain live cargo test", "justfile", live_recipe,
     live_recipe + "\n    cargo test", default_cargo_error),
    ("plain live job cargo test", workflow, "run: just level-triggered-kind-test bootstrap",
     "run: cargo test", "Implicit/default workspace Cargo execution must not enter KinD/live job bootstrap-kind"),
    ("live selector", "justfile", 'bootstrap) test_name="level_triggered_k8s::',
     'postgres) test_name="level_triggered_k8s::',
     "PostgreSQL must not enter live recipe level-triggered-kind-test"),
    ("old deployment versions", design, "protocol 9 / agent schema 5",
     "protocol 8 / agent schema 4", deployment_error),
    ("new deployment versions", design, "protocol 9 / agent schema 5",
     "protocol 10 / agent schema 6", deployment_error),
    ("missing supervision boundary", design, "supervisor loss", "supervision",
     "PostgreSQL design missing contract: supervisor loss"),
    ("missing headroom", design, "free-space headroom", "free-space",
     "PostgreSQL design missing contract: headroom"),
    ("classic claim", design, "## Architecture and Authority", "## PG-level write fencing not implemented",
     "PostgreSQL design retains classic/planned claim"),
    ("retirement classification", "docs/proposal/v1-retirement-plan.md", "Workstream 4 complete",
     "Workstream 4 pending", "AssertionError"),
    ("protocol constant", "kuberic-protocol/src/lib.rs", "PROTOCOL_VERSION: u32 = 9",
     "PROTOCOL_VERSION: u32 = 8", "AssertionError"),
    ("schema constant", "kuberic-agent/src/state.rs", "SCHEMA_VERSION: u32 = 5",
     "SCHEMA_VERSION: u32 = 4", "AssertionError"),
)
cases += tuple(
    (f"live invocation {invocation}", "justfile", live_recipe,
     "unit-only:\n    " + unit_command + "\n\n" + live_recipe + "\n    " + invocation,
     "PostgreSQL must not enter live recipe unit-only")
    for invocation in (
        "@just\tunit-only", "true && just unit-only", "true&&just unit-only",
        "false || just unit-only", "false||just unit-only", "@true;just unit-only",
        "just verify-kind-context;just unit-only", "/usr/bin/just unit-only",
        "just \\\n        unit-only",
    )
)
cases += tuple(
    (f"unresolved live invocation {invocation}", "justfile", live_recipe,
     live_recipe + "\n    " + invocation, "Unresolved live recipe call")
    for invocation in ("@just \"$recipe\"", "true;just", "just --unknown unit-only")
)
cases += tuple(
    (f"default live Cargo selection {command}", "justfile", live_recipe,
     live_recipe + "\n    " + command, default_cargo_error)
    for command in (
        "cargo build", "cargo check", "cargo clippy", "cargo t", "cargo b", "cargo c",
        "@cargo test", "true;cargo test", "cargo +stable test --all-features",
        "cargo test -- -p kuberic-level-tests",
        "cargo test;cargo test -p kuberic-level-tests",
        "cargo test&&cargo test -p kuberic-level-tests",
        "cargo test||cargo test -p kuberic-level-tests",
    )
)
cases += (
    ("workspace live Cargo selection", "justfile", live_recipe,
     live_recipe + "\n    cargo test --workspace -p kuberic-level-tests",
     "Workspace-wide Cargo execution must not enter live recipe level-triggered-images"),
    ("all live Cargo selection", "justfile", live_recipe,
     live_recipe + "\n    cargo clippy --all -p kuberic-protocol",
     "Workspace-wide Cargo execution must not enter live recipe level-triggered-images"),
    ("wildcard live Cargo selection", "justfile", live_recipe,
     live_recipe + '\n    cargo test -p "*"',
     "Unresolved live Cargo package selection"),
    ("dynamic live Cargo selection", "justfile", live_recipe,
     live_recipe + '\n    cargo test --package="$package"',
     "Unresolved live Cargo package selection"),
)
for name, relative, before, after, reason in cases:
    target = fixture / relative
    original = target.read_text()
    assert before in original, (relative, before)
    try:
        target.write_text(re.sub(re.escape(before), lambda _: after, original, flags=re.I))
        result = check()
        assert result.returncode != 0 and reason in result.stdout, \
            f"Documentation guard accepted {name} or failed for the wrong reason\n{result.stdout}"
        print(f"Rejected {name}: {reason}")
    finally:
        target.write_text(original)

# Comments cannot supply executable coverage, nor should comments that merely
# mention PostgreSQL create a false live execution finding.
positive_checks = 1
for relative, addition in (
    (workflow, "\n# PostgreSQL remains host-local only\n"),
    ("justfile", "\n# PostgreSQL remains host-local only\n"),
    ("justfile", "\nunit-only:\n    " + unit_command + "\n"),
):
    target = fixture / relative
    original = target.read_text()
    try:
        target.write_text(original + addition)
        result = check()
        assert result.returncode == 0, result.stdout
        positive_checks += 1
    finally:
        target.write_text(original)

# Keep targeted host-local PostgreSQL CI intact while checking safe live commands.
target = fixture / workflow
original = target.read_text()
try:
    target.write_text(original.replace("run: " + unit_command,
                                      'run: "' + unit_command + ' # host-local only"'))
    result = check()
    assert result.returncode == 0, result.stdout
    positive_checks += 1
    print("Accepted executable targeted host-local PostgreSQL CI with an inline comment")
finally:
    target.write_text(original)

for command in (
    "cargo test -p kuberic-level-tests",
    "@cargo build -p kuberic-controller -p kvstore2",
    "true;cargo check --package kuberic-protocol",
    "true&&cargo clippy --package=kuberic-protocol -- -D warnings",
    "false||cargo test -pkuberic-level-tests -- --ignored --exact",
    "cargo +stable test -p=kuberic-protocol",
    "cargo test -p kuberic-protocol;cargo check -p kuberic-wire",
    "cargo metadata --no-deps",
    "@just verify-kind-context",
    "@ just\tverify-kind-context",
    "true;just verify-kind-context",
    "true&&just verify-kind-context||just verify-kind-ownership",
    "just level-triggered-kind-test bootstrap replacement",
):
    target = fixture / "justfile"
    original = target.read_text()
    try:
        target.write_text(original.replace(live_recipe, live_recipe + "\n    " + command))
        result = check()
        assert result.returncode == 0, f"Rejected safe live command {command}\n{result.stdout}"
        positive_checks += 1
        print(f"Accepted safe live command with targeted PostgreSQL CI: {command}")
    finally:
        target.write_text(original)

live = fixture / "kuberic-level-tests/src/postgres.rs"
live.write_text("use postgres_replicated::testing;\n")
result = check()
assert result.returncode != 0 and "PostgreSQL must not enter live tests" in result.stdout, result.stdout
print(f"PostgreSQL classification, CI and documentation regression tests passed "
      f"({len(cases) + 1} negative mutations, {positive_checks} positive checks).")
PY

echo "Level-triggered guard regression tests passed."
