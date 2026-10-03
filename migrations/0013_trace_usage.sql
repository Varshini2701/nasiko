-- =============================================================================
-- trace_usage: per-agent-per-trace FinOps summary, materialized from Tempo
-- spans by the background trace-usage worker (trace_materializer.rs).
--
-- One row per (trace, agent) pair — a multi-agent trace where Agent A and
-- Agent B both make LLM calls produces two rows, one per agent, each with
-- its own token/cost totals. This avoids mis-attributing Agent B's spend
-- to Agent A.
--
-- NOT a replacement for `token_usage` (which tracks platform-paid orchestrator
-- spend via server-side metering). This table captures agent-side LLM spend
-- visible only through OTel spans.
-- =============================================================================

CREATE TABLE trace_usage (
    trace_id              TEXT NOT NULL,
    agent_name            TEXT NOT NULL,
    session_id            TEXT,
    agent_id              UUID REFERENCES agents(id) ON DELETE SET NULL,
    user_id               UUID REFERENCES users(id) ON DELETE SET NULL,
    model                 TEXT,
    provider              TEXT,
    input_tokens          BIGINT NOT NULL DEFAULT 0,
    output_tokens         BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens     BIGINT NOT NULL DEFAULT 0,
    cache_creation_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd              DOUBLE PRECISION NOT NULL DEFAULT 0,
    prompt_cost_usd       DOUBLE PRECISION NOT NULL DEFAULT 0,
    completion_cost_usd   DOUBLE PRECISION NOT NULL DEFAULT 0,
    latency_ms            BIGINT,
    started_at            TIMESTAMPTZ NOT NULL,
    materialized_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (trace_id, agent_name),
    CONSTRAINT trace_usage_tokens_nonneg CHECK (input_tokens >= 0 AND output_tokens >= 0)
);

-- FinOps dashboard: per-agent aggregation in a time window
CREATE INDEX idx_trace_usage_agent_time ON trace_usage (agent_name, started_at);
-- Spend timeseries/calendar: time-bucketed aggregation across all agents
CREATE INDEX idx_trace_usage_started ON trace_usage (started_at);
-- Model filter
CREATE INDEX idx_trace_usage_model ON trace_usage (model, started_at) WHERE model IS NOT NULL;
-- Provider filter
CREATE INDEX idx_trace_usage_provider ON trace_usage (provider, started_at) WHERE provider IS NOT NULL;
-- Session drill-down
CREATE INDEX idx_trace_usage_session ON trace_usage (session_id) WHERE session_id IS NOT NULL;
-- Agent UUID lookup (for joining with agents table)
CREATE INDEX idx_trace_usage_agent_id ON trace_usage (agent_id, started_at) WHERE agent_id IS NOT NULL;
-- User/org-unit filter (EE: resolves org_unit → user_ids, then filters here)
CREATE INDEX idx_trace_usage_user ON trace_usage (user_id, started_at) WHERE user_id IS NOT NULL;

-- =============================================================================
-- trace_usage_cursor: materializer high-water mark.
-- Single-row singleton (same pattern as `settings`). The worker reads this at
-- startup and advances it after each successful batch.
-- =============================================================================

CREATE TABLE trace_usage_cursor (
    id         INTEGER PRIMARY KEY DEFAULT 1,
    high_water TIMESTAMPTZ NOT NULL DEFAULT '2020-01-01T00:00:00Z',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT trace_usage_cursor_singleton CHECK (id = 1)
);
INSERT INTO trace_usage_cursor (id) VALUES (1);
