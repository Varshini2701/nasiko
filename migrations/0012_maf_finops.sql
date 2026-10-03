-- =============================================================================
-- MAF FinOps: persist real per-run cost so workflow-level attribution rows
-- (Agent/Workflow toggle on the TokenOps dashboard) come from a single fast
-- Postgres query, not a live Tempo aggregation.
--
-- `step_results` (JSONB) already carries the new per-step input_tokens/
-- output_tokens/model_used/cost_usd keys — no migration needed there, JSONB
-- is schemaless. This migration only adds the run-level summary column.
-- =============================================================================

ALTER TABLE maf_executions ADD COLUMN cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0;

CREATE INDEX idx_maf_executions_maf_started ON maf_executions (maf_id, started_at);
