-- Add the wire dialect a custom provider speaks. 'openai' is the plain OpenAI shape
-- (`{base}/chat/completions`, bearer auth, `GET {base}/models`). 'azure-openai' is
-- Azure's classic data plane: the model is a *deployment* name carried in the path,
-- the credential is an `api-key` header, and every call needs an `?api-version=`.
-- See ProviderDialect in oss/llm-router.
ALTER TABLE custom_providers
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'openai'
        CHECK (kind IN ('openai', 'azure-openai')),
    -- Required by (and only meaningful for) kind = 'azure-openai'.
    ADD COLUMN api_version TEXT;

-- Azure routes every call through `?api-version=`, and no default stays correct as the
-- API evolves, so the admin must pick one explicitly. Existing rows are all 'openai', so
-- the constraint holds for them with api_version NULL.
ALTER TABLE custom_providers
    ADD CONSTRAINT azure_requires_api_version
        CHECK (kind <> 'azure-openai' OR api_version IS NOT NULL);
