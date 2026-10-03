-- Per-agent opt-in for structural payload compression (see `nasiko-compress`).
--
-- A column on `agents` rather than a key inside the attached LLM config: `agents.llm_config_id`
-- is a foreign key to a *shared* `llm_configs` row, so a flag there would switch compression on
-- for every agent attached to that config. Compression changes what a model sees, so its blast
-- radius has to be exactly one agent.
--
-- Default false: an existing agent's requests are byte-identical until someone turns this on.
ALTER TABLE agents ADD COLUMN compress_enabled BOOLEAN NOT NULL DEFAULT false;
