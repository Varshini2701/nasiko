-- Add 'skipped' to `hitl_requests.resume_status`'s allowed values (found in review).
--
-- `oss/hitl/src/repo.rs::skip_resume_for_mirrored_row` marks a mirrored `mcp_tool` row's resume
-- as terminal without ever attempting delivery — the mirror's own `direct_chat`/`agent_proxy` row
-- is the real, task_id-bearing resume, and firing this row's own context-free nudge on top of it
-- would race it (see that function's own doc comment). It previously reused 'completed' for this,
-- which incorrectly implies a delivery attempt was made and succeeded. 'skipped' names the actual
-- state: resume deliberately never attempted.
--
-- Rewriting 0007_hitl.sql's inline CHECK is not an option (sqlx hashes applied migration files),
-- so this re-expresses it forward-only, following 0011_baseline_deltas.sql's
-- agents_status_check precedent. Any row already at 'completed' from the old behavior is left
-- as-is — this migration only widens what's allowed going forward.

ALTER TABLE hitl_requests DROP CONSTRAINT IF EXISTS hitl_requests_resume_status_check;
ALTER TABLE hitl_requests
  ADD CONSTRAINT hitl_requests_resume_status_check CHECK (resume_status IN
    ('not_started', 'completed', 'failed', 'delivery_outcome_unknown', 'skipped'));
