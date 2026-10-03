-- Scope hitl_requests' two MCP-tool idempotency indexes by owner, not just agent (security
-- review) — the same class of bug 0025_hitl_task_id_scope.sql already closed for
-- uq_hitl_pending_per_task, which uq_hitl_pending_per_tool_call and
-- uq_hitl_pending_per_connector_auth never got.
--
-- Both indexes back an `ON CONFLICT (...) DO UPDATE ... RETURNING *` in
-- oss/hitl/src/repo.rs (create_pending_tool_approval_with_ttl /
-- create_pending_auth_required_with_ttl). `context_id` for these rows falls back to the raw,
-- agent-controlled trace id whenever no `session_traces` mapping exists
-- (session::resolve_context_id in oss/mcp-gateway) — so a non-random or forged context_id from
-- any agent could collide two different users' pending rows onto the same
-- (agent, connector[, tool], context) tuple. Without owner_user_id in the conflict target, the
-- second user's INSERT would silently update and return the FIRST user's row: one user's
-- approval would then resolve the other user's pending tool call.
--
-- Widening an index (adding a column) can only make it less restrictive, so this is safe to run
-- against existing data with no prior normalization step, unlike 0011_baseline_deltas.sql's own
-- precedent for narrowing one.

DROP INDEX uq_hitl_pending_per_tool_call;
CREATE UNIQUE INDEX uq_hitl_pending_per_tool_call
  ON hitl_requests (owner_user_id, agent_id, connector_id, tool_name, context_id)
  WHERE status = 'pending' AND kind = 'tool_approval';

DROP INDEX uq_hitl_pending_per_connector_auth;
CREATE UNIQUE INDEX uq_hitl_pending_per_connector_auth
  ON hitl_requests (owner_user_id, agent_id, connector_id, context_id)
  WHERE status = 'pending' AND kind = 'auth_required' AND origin = 'mcp_tool';
