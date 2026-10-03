-- Phase 1 of MCP tool search: persist input_schema alongside tool catalog entries.
-- Previously fetched from backends but discarded before DB upsert.
ALTER TABLE mcp_connector_tools ADD COLUMN input_schema JSONB;
