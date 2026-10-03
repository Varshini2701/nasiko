-- Schema deltas that previously lived as edits to 0001_schema.sql and
-- 0005_llm_routing.sql.
--
-- Rewriting a migration that has already run is not a no-op: sqlx stores a
-- SHA-384 of each applied file and refuses to start when one changes
-- ("VersionMismatch"). Both of those files are applied in production, so the
-- edits are re-expressed here as forward-only statements instead. Every
-- statement below is written to be safe on a database that already has the
-- rewritten baseline *and* on one that only ever saw the original.
--
-- Deliberately NOT included: dropping the unused `artifacts` table, its
-- `artifact_status` enum, or the `vector` extension. Those were cosmetic
-- removals from the baseline; dropping a populated table in a migration is
-- irreversible, and `vector` is still a hard startup requirement
-- (REQUIRED_PG_EXTENSIONS in oss/server/src/state.rs), so removing it would
-- break boot. They stay as vestigial objects.

-- ── agents.protocol_version: default the A2A version new rows advertise ──────
-- The a2a-lf crate speaks 0.3.x; 0.2.9 was the default when the baseline was
-- written. Existing rows keep whatever they were given.
ALTER TABLE agents ALTER COLUMN protocol_version SET DEFAULT '0.3.0';

-- ── agents.status: admit 'crashed' ───────────────────────────────────────────
-- The enterprise crash-loop guardian writes 'crashed' when a Kubernetes
-- deployment enters CrashLoopBackOff. The column had no CHECK at all
-- in the original baseline; adding one without 'crashed' would make that UPDATE
-- fail with SQLSTATE 23514 and leave the row 'running' forever, so the crash
-- would never surface in the API or the UI. Mirrors the deployment_status enum.
-- Any row already holding a value outside the list is normalised first so the
-- constraint can be added without a table rewrite failure.
UPDATE agents SET status = 'failed'
 WHERE status NOT IN ('registered', 'deploying', 'running', 'stopped', 'failed', 'crashed');

ALTER TABLE agents DROP CONSTRAINT IF EXISTS agents_status_check;
ALTER TABLE agents ADD CONSTRAINT agents_status_check
    CHECK (status IN ('registered', 'deploying', 'running', 'stopped', 'failed', 'crashed'));

-- ── agent_deployments.updated_at: make it reliable ───────────────────────────
-- It was nullable with no default and nothing maintaining it, so "when did this
-- deployment last change" was unanswerable for any row nobody had explicitly
-- written. Backfill, then keep it current with the shared trigger.
UPDATE agent_deployments SET updated_at = COALESCE(updated_at, created_at, now())
 WHERE updated_at IS NULL;
ALTER TABLE agent_deployments ALTER COLUMN updated_at SET DEFAULT now();
ALTER TABLE agent_deployments ALTER COLUMN updated_at SET NOT NULL;

DROP TRIGGER IF EXISTS trg_agent_deployments_updated_at ON agent_deployments;
CREATE TRIGGER trg_agent_deployments_updated_at
    BEFORE UPDATE ON agent_deployments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ── token_usage: drop the metadata GIN index ─────────────────────────────────
-- Nothing queries token_usage by metadata containment; the index only cost
-- write throughput on the hottest insert path on the platform.
DROP INDEX IF EXISTS idx_token_usage_metadata;

-- ── settings: enforce the singleton ──────────────────────────────────────────
-- Every reader assumes exactly one row (id = 1). Nothing stopped a second one
-- from being inserted, after which which-row-wins was undefined.
DELETE FROM settings WHERE id <> 1;
ALTER TABLE settings DROP CONSTRAINT IF EXISTS settings_singleton;
ALTER TABLE settings ADD CONSTRAINT settings_singleton CHECK (id = 1);

-- ── calculate_token_cost: fall back to a model-only price match ──────────────
-- The router labels every OpenAI-compatible upstream 'openai' regardless of who
-- actually serves the model (e.g. DeepSeek behind OPENAI_API_BASE), while
-- pricing may be seeded under the upstream's own provider name. Without this
-- fallback, gateway-metered usage on such deployments never gets a cost at all.
-- Model names are near-unique across providers, so matching on model alone is a
-- safe second attempt -- and still returns NULL when nothing matches.
CREATE OR REPLACE FUNCTION calculate_token_cost(
    p_provider TEXT, p_model TEXT,
    p_input_tokens INTEGER, p_output_tokens INTEGER,
    p_cache_creation_tokens INTEGER, p_cache_read_tokens INTEGER,
    p_timestamp TIMESTAMPTZ
) RETURNS DECIMAL(10, 8) AS $$
DECLARE v_pricing RECORD; v_cost DECIMAL(10, 8);
BEGIN
    SELECT * INTO v_pricing FROM model_pricing
    WHERE provider = p_provider AND model = p_model
      AND effective_from <= p_timestamp
      AND (effective_until IS NULL OR effective_until > p_timestamp)
    ORDER BY effective_from DESC LIMIT 1;
    IF NOT FOUND THEN
        -- Provider labels are routing-surface labels, not upstream identities.
        SELECT * INTO v_pricing FROM model_pricing
        WHERE model = p_model
          AND effective_from <= p_timestamp
          AND (effective_until IS NULL OR effective_until > p_timestamp)
        ORDER BY effective_from DESC LIMIT 1;
    END IF;
    IF NOT FOUND THEN RETURN NULL; END IF;
    v_cost := (p_input_tokens::DECIMAL / 1000000.0) * v_pricing.input_price_per_1m
            + (p_output_tokens::DECIMAL / 1000000.0) * v_pricing.output_price_per_1m;
    IF v_pricing.cache_creation_price_per_1m IS NOT NULL THEN
        v_cost := v_cost
            + (COALESCE(p_cache_creation_tokens, 0)::DECIMAL / 1000000.0) * v_pricing.cache_creation_price_per_1m
            + (COALESCE(p_cache_read_tokens, 0)::DECIMAL / 1000000.0) * v_pricing.cache_read_price_per_1m;
    END IF;
    RETURN v_cost;
END;
$$ LANGUAGE plpgsql STABLE;

-- ── model_registry: remove the built-in seeds ────────────────────────────────
-- This table carries OPERATOR INTENT ONLY (written via PUT /api/model-registry)
-- and must start empty. The router now derives tiers from the live
-- provider_models catalog (0010); a seeded row overrides that derivation, and
-- the seeded names are actively wrong for any deployment whose provider
-- endpoint is a custom OpenAI-compatible host -- the router would rewrite the
-- request's model to a name the upstream rejects.
--
-- Scoped to the exact seeded tuples, and only where the row has never been
-- edited (set_updated_at bumps updated_at on every UPDATE). An operator who
-- deliberately set one of these values keeps it.
DELETE FROM model_registry
 WHERE (provider, tier, model) IN (
        ('anthropic', 1, 'claude-opus-4-8'),
        ('anthropic', 2, 'claude-sonnet-4-6'),
        ('anthropic', 3, 'claude-haiku-4-5'),
        ('openai',    1, 'gpt-5.5'),
        ('openai',    2, 'gpt-5.4'),
        ('openai',    3, 'gpt-4o-mini')
       )
   AND updated_at = created_at;
