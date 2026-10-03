-- AuthRequired's idempotent-creation guard for MCP connectors, mirroring
-- uq_hitl_pending_per_tool_call for tool_approval: at most one open
-- re-authentication request per (agent, connector, conversation). Scoped
-- narrower than tool_approval's index (no tool_name) because AuthRequired is
-- a connector-level condition, not a per-tool one — any tool call against the
-- same unusable connector in the same conversation must surface (and later
-- resume against) the same row, not spawn a duplicate.
CREATE UNIQUE INDEX uq_hitl_pending_per_connector_auth
  ON hitl_requests (agent_id, connector_id, context_id)
  WHERE status = 'pending' AND kind = 'auth_required' AND origin = 'mcp_tool';

-- Postgres treats NULLs as distinct in unique indexes, so the index above
-- enforces nothing if connector_id or context_id is NULL on a pending
-- auth_required/mcp_tool row. This CHECK is the same backstop pattern as
-- chk_hitl_tool_approval_identity in 0007_hitl.sql, scoped to this one
-- kind+origin combination only — it says nothing about auth_required rows
-- from other origins (direct_chat/agent_proxy/orchestrator/maf), whose
-- identity requirements are a separate, not-yet-decided question.
ALTER TABLE hitl_requests
  ADD CONSTRAINT chk_hitl_mcp_auth_required_identity CHECK (
    kind <> 'auth_required'
    OR origin <> 'mcp_tool'
    OR (connector_id IS NOT NULL AND context_id IS NOT NULL)
  );
