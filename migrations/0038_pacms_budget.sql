-- Per-user PACMS conversation-history token-budget preference. The tier only
-- names which budget a user picked; the actual token counts per tier are
-- operator-configurable (PACMS_BUDGET_LOW/MEDIUM/HIGH), not stored here.
CREATE TYPE pacms_budget_level AS ENUM ('low', 'medium', 'high');

ALTER TABLE users
    ADD COLUMN pacms_budget_level pacms_budget_level NOT NULL DEFAULT 'medium';
