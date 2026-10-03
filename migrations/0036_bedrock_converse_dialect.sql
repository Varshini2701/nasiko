-- Add 'bedrock-converse' to the wire dialect CHECK constraint on custom_providers.kind.
-- Bedrock Converse is the native AWS Bedrock Runtime API, which supports all models
-- (including INFERENCE_PROFILE-only ones like Claude, GPT-6, Grok) via a different
-- wire format than the OpenAI-compatible Mantle endpoint.
ALTER TABLE custom_providers
    DROP CONSTRAINT custom_providers_kind_check;

ALTER TABLE custom_providers
    ADD CONSTRAINT custom_providers_kind_check
        CHECK (kind IN ('openai', 'azure-openai', 'bedrock-converse'));
