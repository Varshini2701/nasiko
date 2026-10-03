-- =============================================================================
-- Scope the HITL pending-pause uniqueness/idempotency key by owner + agent, not by task_id
-- alone (security review).
--
-- `hitl_requests.task_id` is populated from the string the CALLED AGENT returns in its paused
-- A2A response (`paused_task_id`), not a Nasiko-minted id — see `oss/server/src/router/
-- a2a_dispatch.rs::paused_task_id`. The original `uq_hitl_pending_per_task` (0007_hitl.sql)
-- trusted that agent-controlled value as a database-wide (every user, every agent) uniqueness
-- key: on a unique-violation, `create()`'s `find_existing_pending` fallback
-- (oss/hitl/src/store.rs) returned whatever pending row already held that task_id, with no
-- check that its owner_user_id/agent_id matched the new request. A non-random or
-- maliciously-templated task_id from any agent could therefore collide two unrelated users'
-- pauses onto the same row.
--
-- Scoping the index — and the matching `find_existing_pending` lookup — by (owner_user_id,
-- agent_id) as well as task_id makes the uniqueness guarantee, and the idempotent-create
-- fallback, actually mean "this user's pending pause with this agent on this task", never "any
-- pending pause anywhere that happens to share this task_id string".
-- =============================================================================

DROP INDEX uq_hitl_pending_per_task;

CREATE UNIQUE INDEX uq_hitl_pending_per_task
  ON hitl_requests (owner_user_id, agent_id, task_id)
  WHERE status = 'pending' AND task_id IS NOT NULL;
