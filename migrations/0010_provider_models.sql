-- provider_models — the live model catalog per provider, synced from each provider's
-- GET /models endpoint by the LLM router's catalog-sync loop (see
-- oss/llm-router/src/routing/catalog.rs). This replaces hardcoded model lists: the
-- smart router derives tier→model mappings from what the configured endpoint actually
-- serves (ranked by price as the strength signal), so a custom OpenAI-compatible
-- upstream (DeepSeek, vLLM, Ollama, …) is routed among *its own* models automatically.
--
-- Rows are catalog-owned: the sync upserts what the provider lists and deletes rows
-- the provider no longer lists. Operator tier overrides belong in model_registry
-- (PUT /api/model-registry), which always wins over the derived mapping.
CREATE TABLE provider_models (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, model)
);
CREATE INDEX idx_provider_models_provider ON provider_models(provider);
