//! Wire-format constants for MAF (Multi-Agent Flow) execution status — shared between
//! `oss/orchestrator` (writes `maf_executions.status` and each step's own `StepResult.status`)
//! and `oss/cli` (polls `GET /maf/workflow/result/{id}` and reads the same value back), which
//! can't depend on `oss/orchestrator` directly (the CLI stays a lightweight, sync-HTTP, no-tokio
//! binary — see `oss/docs/CLI_DESIGN.md`). `oss/types` is a dependency of both already.

/// The execution (or one step) is paused awaiting a human's answer via the HITL resume flow —
/// not a terminal state; `success`/`failed` are the only two of those. Written by
/// `oss/orchestrator/src/maf/worker.rs` and `executor.rs`, read by `oss/cli/src/commands/maf.rs`'s
/// poll loop (found in review: both sides hardcoded the literal independently).
pub const AWAITING_HUMAN: &str = "awaiting_human";
