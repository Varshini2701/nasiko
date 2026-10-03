-- Retire the SQL cost engine. Cost is computed in Rust by `nasiko-pricing`,
-- which every surface now prices through.
--
-- `calculate_token_cost` looked `model_pricing` up by exact `(provider, model)`
-- and returned NULL on a miss. NULL reads as $0 in every `SUM(cost_usd)`, so on
-- the reference deployment 389 of 419 rows — 92.8% — recorded no spend:
-- $0.0152 booked against ~$5.24 of real usage. Three independent causes, none
-- visible from the row itself:
--
--   * provider labels differ between writer and book (`aws-bedrock` vs
--     `amazon-bedrock`) — 341 rows;
--   * the model was never seeded (`zai-org/GLM-5.3`) — 26 rows;
--   * MCP tool calls write a tool name into `model`, which can never price.
--
-- The function also disagreed with the Rust engine wherever both ran: on a
-- model with no cache rates it skipped cache cost entirely while Rust charged
-- it at the input rate, and it resolved prices at the row timestamp while
-- `DbPricing` resolved at now().
--
-- The trigger is dropped, not the column: `token_usage.cost_usd` is still the
-- cost, it is just written by the caller now. Rows already priced by the trigger
-- keep their value; the NULL ones are backfilled separately so the repair is
-- auditable on its own.
--
-- Reversible: re-creating the trigger restores the previous behaviour, since
-- `calculate_token_cost` only ever fired when `cost_usd` was NULL and the Rust
-- path now always supplies a value.

DROP TRIGGER IF EXISTS trigger_calculate_usage_cost ON token_usage;
DROP FUNCTION IF EXISTS calculate_usage_cost_trigger();
DROP FUNCTION IF EXISTS calculate_token_cost(TEXT, TEXT, INTEGER, INTEGER, INTEGER, INTEGER, TIMESTAMPTZ);
