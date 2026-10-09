# WorkflowContext

Work Title: PostgreSQL Stateless Metadata
Work ID: postgresql-stateless-metadata
Base Branch: main
Target Branch: feature/postgresql-stateless-metadata
Execution Mode: current-checkout
Repository Identity: github.com/youyuanwu/kuberic@3d2f129bf328d7869c998718fa98e4dac441693b
Execution Binding: none
Workflow Mode: full
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: enabled
Final Review Mode: multi-model
Final Review Interactive: smart
Final Review Models: gpt-5.4, gemini-3.8-flash, claude-opus-4.8
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Final Review Perspectives: auto
Final Review Perspective Cap: 2
Implementation Model: none
Plan Generation Mode: single-model
Plan Generation Models: gpt-5.4, gemini-3.8-flash, claude-opus-4.8
Planning Docs Review: enabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: gpt-5.4, gemini-3.8-flash, claude-opus-4.8
Planning Review Specialists: all
Planning Review Interaction Mode: parallel
Planning Review Specialist Models: none
Planning Review Perspectives: auto
Planning Review Perspective Cap: 2
Custom Workflow Instructions: Fresh deployments only. Do not implement metadata migration, rolling-upgrade compatibility, mixed-version behavior, legacy fallback, or backward compatibility. Treat docs/features/postgres/stateless-metadata.md as the existing design input and begin with code research.
Initial Prompt: Implement PostgreSQL restart-stateless metadata so the PostgreSQL custom replicator persists no application-owned durable state. PostgreSQL facts must be reconstructed from PGDATA, while durable workflow and safety evidence move to generic Kuberic runtime/controller authority. Preserve the Replicator, PrimaryReplicator, and StateProvider interfaces.
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: docs/features/postgres/stateless-metadata.md
