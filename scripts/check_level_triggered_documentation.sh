#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

grep -Fqx 'apiVersion: operator.kuberic.io/v1alpha1' examples/kvstore2/deploy/sample.yaml
grep -Fqx 'kind: KubericSet' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  replicas: 3' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  image: localhost/kvstore2:level-triggered-v1' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  failoverDelaySeconds: 10' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  # switchover:' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  #   requestId: move-to-replica-2-001' examples/kvstore2/deploy/sample.yaml
grep -Fqx '  #   targetReplicaId: 2' examples/kvstore2/deploy/sample.yaml

python3 - "$@" <<'PY'
import difflib
import json
import pathlib
import re
import subprocess
import sys
import urllib.parse

root = pathlib.Path.cwd()
checked_crd = (root / "kuberic-controller/deploy/crd.json").read_bytes()
generated_crd = subprocess.run(
    ["cargo", "run", "-p", "kuberic-controller", "--bin", "crdgen", "--quiet"],
    check=True, stdout=subprocess.PIPE,
).stdout
if checked_crd != generated_crd:
    print("The checked-in level-triggered CRD differs from the generated API.", file=sys.stderr)
    sys.stderr.writelines(difflib.unified_diff(
        checked_crd.decode().splitlines(keepends=True),
        generated_crd.decode().splitlines(keepends=True),
        fromfile="checked-in CRD", tofile="generated CRD",
    ))
    raise SystemExit(1)
schema = json.loads(checked_crd)
properties = schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]
spec = properties["spec"]
assert set(spec["properties"]) == {"replicas", "image", "failoverDelaySeconds", "switchover"}
assert spec["properties"]["replicas"]["type"] == "integer"
assert spec["properties"]["replicas"]["minimum"] == 1
request = spec["properties"]["switchover"]
assert "switchover" not in spec["required"], "Switchover must remain opt-in"
assert set(request["required"]) == {"requestId", "targetReplicaId"}
assert set(request["properties"]) == {"requestId", "targetReplicaId"}
assert request["properties"]["requestId"]["type"] == "string"
assert request["properties"]["requestId"]["minLength"] == 1
assert request["properties"]["targetReplicaId"]["type"] == "integer"
assert request["properties"]["targetReplicaId"]["minimum"] == 1
status = properties["status"]["properties"]
receipt = status["lastSwitchover"]["properties"]
assert set(receipt) == {
    "requestId", "requestedTargetReplicaId", "acceptedTarget", "resultingPrimary", "outcome"
}
assert set(receipt["outcome"]["enum"]) == {
    "requestedTargetCompleted", "oldPrimaryRestored", "oldPrimaryCompensated", "rejected", "unsafe"
}
transition = status["transition"]["properties"]
assert "plannedSwitchover" in transition["kind"]["enum"]
assert "secondaryScaleDown" in transition["kind"]["enum"]
removal = transition["secondaryScaleDown"]["properties"]
assert set(removal) == {
    "operationId", "resourceUid", "specGeneration", "desiredReplicas",
    "previousConfiguration", "currentConfiguration", "previousPolicy", "currentPolicy",
    "primary", "target", "cleanup"
}
assert removal["desiredReplicas"]["minimum"] == 1
assert removal["specGeneration"]["minimum"] == 1
assert set(transition["secondaryRemovalEvidence"]["properties"]) == {
    "preparation", "previousReadQuorum", "reducedWriteQuorum"
}
assert set(status["secondaryScaleDownCleanup"]["properties"]) == {
    "evidence", "currentOnlyWriteQuorum", "retirement"
}
assert set(status["lastSecondaryRemoval"]["properties"]) == {
    "evidence", "currentOnlyWriteQuorum"
}, "Completed removal proof must not retain retirement/deletion authority"
for field in ("secondaryScaleDownCleanup", "lastSecondaryRemoval",
              "pendingReplacementCleanup", "lastReplacement"):
    assert field not in properties["status"]["required"], f"{field} must remain optional"
for field in ("pendingReplacementCleanup", "lastReplacement"):
    assert set(status[field]["properties"]) == {"resourceUid", "resources", "target"}
for resource in ("endpoint", "pod", "pvc"):
    identity = removal["cleanup"]["properties"][resource]["properties"]
    assert set(identity) == {"present", "absent"}
    assert set(identity["present"]["required"]) == {"name", "uid"}
    assert set(identity["absent"]["required"]) == {"name"}
intent = transition["switchover"]["properties"]
assert set(intent) == {
    "preparationGeneration", "requestId", "source", "target", "requestedConfiguration", "resolution", "handoff"
}
assert set(intent["resolution"]["enum"]) == {
    "requestedTarget", "restoringOldPrimary", "compensatingOldPrimary", "unsafe"
}
assert set(intent["handoff"]["properties"]) == {
    "preparationGeneration", "preparationOperationId", "requestId", "source", "target",
    "startingConfigurationId", "startingEpoch", "handoffLsn"
}

sample = (root / "examples/kvstore2/deploy/sample.yaml").read_text()
active_sample = "\n".join(line for line in sample.splitlines() if not line.lstrip().startswith("#"))
assert "switchover:" not in active_sample, "Installation must not submit a switchover"
request_example = "\n".join(
    line.replace("  # ", "  ", 1) if line.startswith(
        ("  # switchover:", "  #   requestId:", "  #   targetReplicaId:")
    ) else line
    for line in sample.splitlines()
    if not line.lstrip().startswith("#") or line.startswith(
        ("  # switchover:", "  #   requestId:", "  #   targetReplicaId:")
    )
)
guide = root / "docs/features/kuberic/level-triggered-operator.md"
guide_text = guide.read_text()
examples = re.findall(r"```yaml\n(.*?)\n```", guide_text, re.DOTALL)
assert active_sample in examples, "Guide bootstrap example differs from sample"
assert request_example in examples, "Guide request example differs from opt-in sample"
assert active_sample.replace("  replicas: 3", "  replicas: 2") in examples, \
    "Guide scale-down example must only lower the sample replica count"
patches = [json.loads(value) for value in re.findall(r"-p '(\{.*\})'", guide_text)]
for replicas in (2, 1):
    assert {"spec": {"replicas": replicas}} in patches, \
        f"Missing spec-only scale-down patch for {replicas} replicas"
assert "Permanent PVC deletion" in sample and "Permanent PVC deletion" in guide_text

protocol = (root / "kuberic-protocol/src/lib.rs").read_text()
store = (root / "kuberic-agent/src/state.rs").read_text()
assert re.search(r"pub const PROTOCOL_VERSION: u32 = 6;", protocol)
assert re.search(r"pub const SCHEMA_VERSION: u32 = 2;", store)
assert "Protocol version 6" in guide_text and "schema 2" in guide_text
assert "Protocol version 6" in (root / "kuberic-wire/README.md").read_text()
assert "Protocol 6" in sample and "schema 2" in sample

recipes = (root / "justfile").read_text()
workflow = (root / ".github/workflows/level-triggered-CI.yml").read_text()
live_tests = (root / "kuberic-level-tests/src/level_triggered_k8s.rs").read_text()
matrix = re.search(r"expanded\+=\(([^)]+)\)", recipes).group(1).split()
for selector, test in (("scale-down", "scale_down"),
                       ("scale-down-adversarial", "scale_down_adversarial")):
    assert selector in matrix
    assert f'{selector}) test_name="level_triggered_k8s::{test}"' in recipes
    assert re.search(rf"fn {test}\(", live_tests)
    for document in (guide, root / "examples/kvstore2/README.md",
                     root / "docs/features/kuberic/testing.md"):
        assert f"just level-triggered-kind-test {selector}" in document.read_text()
assert "just level-triggered-kind-test scale-down" in workflow
assert "just level-triggered-kind-test all" in workflow

documents = [
    root / "README.md",
    root / "docs/Dev.md",
    root / "docs/features/kuberic/testing.md",
    root / "docs/features/kuberic/level-triggered-operator.md",
    root / "docs/proposal/level-triggered-operator-design.md",
    root / "docs/proposal/v1-retirement-plan.md",
    root / "examples/kvstore2/README.md",
    root / "kuberic-agent/README.md",
    root / "kuberic-controller/README.md",
    root / "kuberic-protocol/README.md",
    root / "kuberic-runtime/README.md",
    root / "kuberic-wire/README.md",
]
documents.extend(root / path for path in sys.argv[1:])
link = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
failures = []

def heading_anchors(document):
    anchors = set()
    counts = {}
    content = re.sub(r"```.*?```", "", document.read_text(), flags=re.DOTALL)
    for heading in re.findall(r"^#{1,6}\s+(.+?)\s*#*\s*$", content, re.MULTILINE):
        slug = re.sub(r"[^\w\- ]", "", heading.lower()).replace(" ", "-")
        count = counts.get(slug, 0)
        counts[slug] = count + 1
        anchors.add(f"{slug}-{count}" if count else slug)
    return anchors

assert {"secondary-scale-down", "authority-and-cleanup", "availability-and-restart",
        "tests-and-diagnostics"} <= heading_anchors(guide)
for document in (root / "README.md", root / "examples/kvstore2/README.md",
                 root / "docs/proposal/v1-retirement-plan.md"):
    assert "level-triggered-operator.md#secondary-scale-down" in document.read_text()

for document in documents:
    if not document.is_file():
        failures.append(f"{document.relative_to(root)} does not exist")
        continue
    for line_number, line in enumerate(document.read_text().splitlines(), 1):
        for raw_target in link.findall(line):
            target = raw_target.strip()
            if target.startswith("<") and target.endswith(">"):
                target = target[1:-1]
            target = target.split(maxsplit=1)[0]
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            path = urllib.parse.unquote(target.split("#", 1)[0].split("?", 1)[0])
            resolved = (document.parent / path).resolve() if path else document
            if not resolved.exists():
                failures.append(
                    f"{document.relative_to(root)}:{line_number}: missing {target}"
                )
            elif "#" in target and resolved.suffix == ".md":
                fragment = urllib.parse.unquote(target.split("#", 1)[1])
                if fragment and fragment not in heading_anchors(resolved):
                    failures.append(
                        f"{document.relative_to(root)}:{line_number}: missing heading {target}"
                    )

if failures:
    print("Level-triggered documentation link failures:", file=sys.stderr)
    for failure in failures:
        print(f"- {failure}", file=sys.stderr)
    raise SystemExit(1)
PY

scripts/check_runtime_public_api.sh

echo "Level-triggered links, API examples, scale-down contracts, protocol 6, generated CRD, and runtime API boundaries are current."
