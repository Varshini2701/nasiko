-- Pricing confidence is unknown until a materializer records it explicitly.
ALTER TABLE trace_usage ADD COLUMN cost_estimated BOOLEAN;

-- Materialization is a separate lifecycle from OTLP delivery, so it gets its own
-- columns rather than another `otlp_state` value the outbox's claim queries would
-- have to exclude. Delivery, not the trace timestamp, drives the retry: a receipt
-- reported after a laptop was offline carries span timestamps older than the
-- materializer's rolling Tempo cursor, so the window scan alone never sees it.
ALTER TABLE coding_agent_telemetry_events
    ADD COLUMN materialized_at TIMESTAMPTZ,
    ADD COLUMN materialize_next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN materialize_last_error TEXT;

CREATE INDEX idx_coding_agent_telemetry_materialize_ready
    ON coding_agent_telemetry_events (materialize_next_attempt_at, received_at)
    WHERE otlp_trace_delivered_at IS NOT NULL AND materialized_at IS NULL;
