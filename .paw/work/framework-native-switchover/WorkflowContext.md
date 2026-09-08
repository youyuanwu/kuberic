# WorkflowContext

Work Title: Framework-Native Switchover Graduation
Work ID: framework-native-switchover
Base Branch: main
Target Branch: feature/framework-native-switchover
Execution Mode: current-checkout
Repository Identity: github.com/youyuanwu/kuberic@3d2f129bf328d7869c998718fa98e4dac441693b
Execution Binding: none
Workflow Mode: full
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: single-model
Final Review Interactive: false
Final Review Models: gpt-5.6-sol
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Final Review Perspectives: none
Final Review Perspective Cap: 2
Implementation Model: gpt-5.6-sol
Plan Generation Mode: single-model
Plan Generation Models: gpt-5.6-sol
Planning Docs Review: enabled
Planning Review Mode: single-model
Planning Review Interactive: false
Planning Review Models: gpt-5.6-sol
Planning Review Specialists: all
Planning Review Interaction Mode: parallel
Planning Review Specialist Models: none
Planning Review Perspectives: none
Planning Review Perspective Cap: 2
Custom Workflow Instructions: Run autonomously without pausing except for a genuinely material scope or safety decision and the mandatory final pre-PR milestone. Use local strategy with no intermediate branches or PRs. Create exactly one final PR to main after approval. Disable multi-model and Society-of-Thought execution everywhere. Every commit must include the required Copilot co-author trailer. Final PR title must start with [Framework-Native Switchover].
Initial Prompt: Migrate switchover from the explicit-default and feature-gated durable-pilot split to the default and only production framework-native durable workflow. Reuse the bounded in-process durable runner and ConfigMap checkpoint provider from framework-native remove-replica; evaluate an agent-owned coarse switchover intent; define an independent versioned execution contract and operation-specific bounds; fail closed on incompatible state; preserve safety and recovery guarantees; keep reconciliation as scheduler; update APIs, deployments, examples, tests, CI, measurements, roadmap, status, protocol, operator docs, and generated artifacts. Do not migrate add-replica, failover, or initial creation; do not address SQLite issue #42; avoid unrelated redesign; preserve remove-replica and other operation behavior.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: Repository youyuanwu/kuberic; main synchronized with origin/main at merged PR #61. Prior artifacts at .paw/work/framework-native-remove-replica are read-only context and must not be modified. All KinD testing must create a unique workflow-specific cluster and isolated kubeconfig, verify the exact target context before mutation, never inspect/reuse/touch unrelated or CAPI-related clusters or containers, explicitly target the isolated cluster from kubectl/just/tests, and clean up only resources created for this workflow.
