-- M7: "allow for this session" grants for MCP ToolApproval — the second of
-- the three finalized dialog actions (allow once / allow for this session /
-- deny; "always" is out of scope). A grant here lets a retried tools/call
-- proceed without re-asking for the rest of the conversation (context_id),
-- up to expires_at, without consuming/one-time-using a hitl_requests row the
-- way `once` scope does via consumed_at.
CREATE TABLE mcp_session_tool_grants (
  id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

  -- Matching identity — same tuple as tool_approval's own
  -- uq_hitl_pending_per_tool_call, minus kind/origin (this table is MCP-only
  -- by construction, so those columns would be redundant).
  agent_id      UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  connector_id  UUID NOT NULL,
  tool_name     TEXT NOT NULL,
  context_id    TEXT NOT NULL,

  granted_by       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- The tool_approval row whose "allow for this session" decision produced
  -- this grant — audit trail only; ON DELETE SET NULL so a hard-deleted
  -- hitl_requests row (agent cascade-delete) doesn't take an otherwise-still
  -- valid grant down with it.
  hitl_request_id  UUID REFERENCES hitl_requests(id) ON DELETE SET NULL,

  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at    TIMESTAMPTZ NOT NULL
);

-- Lookup path: "is there any unexpired grant for this exact tuple" — the
-- expiry comparison itself (`expires_at > now()`) happens in the query, not
-- the index, since `now()` isn't immutable and can't back a partial index.
CREATE INDEX idx_mcp_session_tool_grants_lookup
  ON mcp_session_tool_grants (agent_id, connector_id, tool_name, context_id);
