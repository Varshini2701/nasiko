-- =============================================================================
-- Add tool_call_count to trace_usage.
--
-- Tool-call spans carry gen_ai.operation.name = "call_tool" (GenAI semconv)
-- or openinference.span.kind = "TOOL" (OpenInference). They carry no
-- input/output tokens so the existing materializer skipped them entirely;
-- this column gives them a home.
--
-- Default 0 keeps existing rows valid without a backfill. The next
-- materializer pass will recompute correct counts within its high-water
-- overlap window.
-- =============================================================================

ALTER TABLE trace_usage ADD COLUMN tool_call_count BIGINT NOT NULL DEFAULT 0;

-- Partial index: only a subset of traces contain tool calls; skip the rest.
CREATE INDEX idx_trace_usage_tool_calls ON trace_usage (started_at) WHERE tool_call_count > 0;
