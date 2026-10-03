-- Dedicated visibility switch for agents that must never appear in the
-- agent list, routing candidates, or A2A discovery (e.g. Weave's
-- dashboard-generator). Previously modeled as a value in `agents.tags`, but
-- that column is also the free-text descriptive label a caller can overwrite
-- wholesale via PATCH /api/agents/{id} -- a routine metadata edit could
-- silently drop the hiding marker. A separate column the update endpoint
-- never exposes closes that gap.
ALTER TABLE agents ADD COLUMN is_internal BOOLEAN NOT NULL DEFAULT false;

UPDATE agents SET is_internal = true WHERE tags @> ARRAY['internal']::text[];
UPDATE agents SET tags = array_remove(tags, 'internal') WHERE is_internal;

CREATE INDEX idx_agents_is_internal ON agents (is_internal) WHERE is_internal;
