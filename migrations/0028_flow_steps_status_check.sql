-- `flow_steps.status` had no CHECK at all since 0001_schema.sql: a bare `TEXT NOT NULL DEFAULT
-- 'pending'`, so the vocabulary was undocumented and any typo would insert silently (found in
-- review). The values every writer actually uses:
--   'pending'         — the column default; no INSERT currently omits `status`, but it's the
--                       schema's own default and disallowing it would be self-contradictory.
--   'running'         — a step is in flight (flows.rs::add_step, a2a_dispatch.rs's ToolCall).
--   'completed'/'failed' — ToolResult's own outcome.
--   'awaiting_human'  — the step's call paused on a HITL request (a2a_dispatch.rs's
--                       AwaitingHuman handling).
--   'resumed'         — the pause above was answered and the orchestrator continued; distinct
--                       from 'completed'/'failed' because no ToolResult ever arrives for an
--                       `awaiting_human` row, so those would misreport whether the *original*
--                       call itself succeeded.
--
-- Any row already outside this list is normalized first (0011_baseline_deltas.sql's own
-- precedent for adding a CHECK to a previously-unconstrained column) so the ADD CONSTRAINT below
-- can never fail on unexpected existing data.
UPDATE flow_steps SET status = 'failed'
 WHERE status NOT IN ('pending', 'running', 'completed', 'failed', 'awaiting_human', 'resumed');

ALTER TABLE flow_steps DROP CONSTRAINT IF EXISTS flow_steps_status_check;
ALTER TABLE flow_steps
  ADD CONSTRAINT flow_steps_status_check CHECK (status IN
    ('pending', 'running', 'completed', 'failed', 'awaiting_human', 'resumed'));
