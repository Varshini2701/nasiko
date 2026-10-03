-- MCP gateway credential rotation grace (see docs/MCP_GATEWAY_AGENT_AUTH.md).
--
-- Every deploy/restart re-mints MCP_GATEWAY_TOKEN, and the new hash used to
-- replace the old one the instant it was minted -- before the new workload was
-- known to be live. If the rollout then failed and the previous workload kept
-- serving, that workload still held the old plaintext and every /api/mcp call
-- it made returned 401 until someone redeployed. The Kubernetes restart path
-- had the same window whenever its best-effort Secret refresh failed.
--
-- Keep the superseded hash valid for a bounded window so a rollout (or a failed
-- one) cannot orphan a running agent. Bounded, not indefinite: rotation must
-- still mean something.
ALTER TABLE agent_gateway_tokens
    ADD COLUMN prev_token_hash TEXT,
    ADD COLUMN rotated_at TIMESTAMPTZ;

-- Authentication looks up by either hash; the grace window is applied in the
-- query, not the index.
CREATE INDEX idx_agent_gateway_tokens_prev_hash
    ON agent_gateway_tokens(prev_token_hash)
    WHERE prev_token_hash IS NOT NULL AND revoked_at IS NULL;
