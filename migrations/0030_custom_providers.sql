-- =============================================================================
-- custom_providers — admin-registered, OpenAI-compatible LLM endpoints.
-- =============================================================================
-- An admin adds an endpoint by Base URL + API key + default model. The platform
-- fetches its model list (GET {base_url}/models) into the existing
-- provider_models table under this provider's `label`, keeps it fresh via the
-- background catalog-sync loop, and surfaces those models in the LLM config
-- screen — no server redeploy, no .env edit.
--
-- The `label` is used as-is everywhere `provider` is a free-form TEXT column:
-- llm_configs.provider, provider_models.provider, model_registry.provider and
-- token_usage.provider (none of which carry a foreign key), so custom names drop
-- straight in. Its *shape* is checked here; the clash with built-in provider
-- names (openai/anthropic/gemini) is checked in the handler — SQL cannot own
-- that list.
CREATE TABLE custom_providers (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- 2–40 chars, lowercase alphanumeric + internal hyphens. Minimum length is 2
    -- (a 1-char body between two anchors + the two anchor chars would be 3, so
    -- the {0,38} interior keeps the minimum at 2 to match the documented range).
    label        TEXT NOT NULL CHECK (label ~ '^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$'),
    display_name TEXT NOT NULL,
    base_url     TEXT NOT NULL,
    -- AES-256-GCM via SecretsCrypto::for_platform_settings() — a shared platform
    -- credential, not a per-user secret.
    encrypted_api_key TEXT NOT NULL,

    -- Last-resort model for this provider, used instead of the global
    -- DEFAULT_MODEL whenever a call resolves to this provider and no other model
    -- was chosen. Picked by the admin at registration from the fetched model list.
    default_model TEXT NOT NULL,

    -- Whether the background catalog-sync loop re-fetches this provider's models.
    catalog_sync_enabled BOOLEAN NOT NULL DEFAULT true,
    -- Health of the background refresh, shown in the UI.
    last_sync_at     TIMESTAMPTZ,
    last_sync_status TEXT,       -- 'ok' | 'failed' | 'unsupported'
    last_sync_error  TEXT,

    created_by   UUID NOT NULL REFERENCES users(id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at   TIMESTAMPTZ
);

-- Partial only: a soft-deleted label must be reusable (an inline UNIQUE would
-- forbid that forever).
CREATE UNIQUE INDEX uq_custom_providers_label
    ON custom_providers (label) WHERE deleted_at IS NULL;

CREATE TRIGGER trg_custom_providers_updated_at BEFORE UPDATE ON custom_providers
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
