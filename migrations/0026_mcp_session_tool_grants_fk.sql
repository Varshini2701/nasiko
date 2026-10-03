-- Fixes two gaps in 0023_mcp_session_tool_grants.sql, found in review:
--
-- 1. `connector_id` had no FK, unlike every other `connector_id` column in this schema
--    (`mcp_connectors(id) ON DELETE CASCADE` is the established convention — see 0003_mcp.sql).
--    A deleted connector left its grants behind as orphaned rows referencing nothing.
--
-- 2. Nothing ever deleted an expired grant, so the table grows without bound — unlike
--    `hitl_requests`, which has `expire_stale`'s periodic sweep. `create_session_grant`'s own
--    non-idempotent-by-design behavior (a retried approval can create a second grant for the same
--    tuple, per its doc comment) makes this worse over time, not better, without a sweep.
--    A one-time cleanup of already-expired rows here, plus a periodic sweep wired into the
--    resume dispatcher's existing recovery tick (`oss/hitl/src/repo.rs::sweep_expired_session_grants`,
--    `oss/hitl/src/dispatcher.rs`), keeps this bounded going forward.

DELETE FROM mcp_session_tool_grants WHERE connector_id NOT IN (SELECT id FROM mcp_connectors);

ALTER TABLE mcp_session_tool_grants
  ADD CONSTRAINT mcp_session_tool_grants_connector_id_fkey
  FOREIGN KEY (connector_id) REFERENCES mcp_connectors(id) ON DELETE CASCADE;

DELETE FROM mcp_session_tool_grants WHERE expires_at < now();
