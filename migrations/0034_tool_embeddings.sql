-- Phase 2 of MCP tool search: embedding storage for semantic search.
-- Embeddings are loaded into memory at startup; the DB is just persistence.
-- Using BYTEA (not pgvector VECTOR) to avoid requiring the pgvector extension.
CREATE TABLE mcp_tool_embeddings (
    connector_id UUID NOT NULL,
    tool_name    TEXT NOT NULL,
    embedding    BYTEA NOT NULL,
    model        TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (connector_id, tool_name),
    FOREIGN KEY (connector_id, tool_name)
        REFERENCES mcp_connector_tools(connector_id, tool_name) ON DELETE CASCADE
);
