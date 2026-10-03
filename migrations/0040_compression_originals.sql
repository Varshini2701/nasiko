-- Recovery store for structurally compressed payloads (PRD §9 IP-5).
--
-- Compression elides content. Without a way back, every compressor must stay conservative,
-- because a wrong drop is unrecoverable and costs a whole retry. With this table a wrong drop
-- costs one `recover_compressed` tool call — which is what licenses raising the level past
-- `conservative`.
--
-- `handle` is minted by the llm-router seam and interpolated into the elision marker the model
-- sees, e.g. `[… 412 lines elided · recover: nasiko://c/9f3a… ]`.
CREATE TABLE compression_originals (
    handle       UUID PRIMARY KEY,
    -- Scope for retrieval. `flow_id` is the W3C traceparent trace-id (the same value
    -- `routing::boundary::parse_flow_id` returns), so an agent can recover only what was
    -- compressed inside the flow it is currently participating in.
    flow_id      TEXT NOT NULL,
    owner_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Which agent's request the payload was compressed out of. Audit only: a flow may hand a
    -- compressed result to a downstream agent, which must still be able to recover it.
    agent_id     UUID REFERENCES agents(id) ON DELETE SET NULL,
    content      TEXT NOT NULL,
    content_type TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The retrieval path: handle plus the two scope columns, so the authorization predicate is
-- served by the primary key alone and the scope check is a filter on the same row.
CREATE INDEX idx_compression_originals_scope
    ON compression_originals (flow_id, owner_id);

-- TTL sweep. Originals are only useful while their flow is alive; beyond that they are dead
-- weight holding full-size copies of every compressed payload.
CREATE INDEX idx_compression_originals_created
    ON compression_originals (created_at);
