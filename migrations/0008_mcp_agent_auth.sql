-- MCP gateway agent auth: deploy-time agent credential + flow-bound user identity
-- (see docs/MCP_GATEWAY_AGENT_AUTH.md). Replaces the per-request delegation JWT.

-- agent_gateway_tokens — one live gateway credential per agent, minted at deploy
-- time and injected into the container env as MCP_GATEWAY_TOKEN. Only the SHA-256
-- hex hash is stored (mirrors oci_pull_credentials); the plaintext exists solely
-- in the container env. Every deploy/restart rotates the token (upsert), and
-- destroy tombstones it via revoked_at.
CREATE TABLE agent_gateway_tokens (
    agent_id UUID PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ
);
-- Bearer-token authentication looks up by hash alone.
CREATE INDEX idx_agent_gateway_tokens_hash ON agent_gateway_tokens(token_hash)
    WHERE revoked_at IS NULL;

-- flow_participants — durable record of which agents a flow was dispatched to.
-- Written synchronously at every dispatch site alongside the existing flows
-- insert (agent proxy, a2a dispatch, MAF executor) and per cascade leg (CpCallGuard).
-- The MCP gateway and the LLM router authorize a call only when the
-- authenticated agent is a recorded participant of the flow named by the
-- request's traceparent — missing records fail closed (denial, not escalation).
CREATE TABLE flow_participants (
    flow_id TEXT NOT NULL REFERENCES flows(flow_id) ON DELETE CASCADE,
    agent_id UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, agent_id)
);
CREATE INDEX idx_flow_participants_agent ON flow_participants(agent_id);
