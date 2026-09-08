# WorkflowContext

Work Title: Framework-Native Remove Replica
Work ID: framework-native-remove-replica
Base Branch: main
Target Branch: feature/framework-native-remove-replica
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
Custom Workflow Instructions: none
Initial Prompt: Rewrite remove-replica as the default and only framework-native durable operator workflow, introduce a reusable shared durable runner, compact workflow state and activity contracts, remove the explicit/durable pilot architecture after safety traceability, update measurements and documentation, and prepare stable extension points for a future add-replica migration without porting add-replica.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: Single-model execution only. Use gpt-5.6-sol for plan generation and every review. Disable multi-model and Society-of-Thought. Use local planning and implementation commits directly on the target branch, no intermediate PRs or phase branches, and create exactly one final PR to main. Use bracketed title prefix [Framework-Native Remove Replica]. Include Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com> on every commit. Do not pause except for material scope or safety decisions and the mandatory final pre-PR milestone. Source-code complexity comparisons, executable-line counts, decision-point counts, ratios, classifications, and lexical measurement gates are explicitly excluded; report only operational and persistence evidence.
