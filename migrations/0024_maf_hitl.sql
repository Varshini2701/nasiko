-- =============================================================================
-- MAF HITL support
--
-- Two columns needed to resume a paused MAF execution without re-deriving state that today only
-- lives transiently (a Redis message payload, or a local variable inside `run_maf`):
--
--   maf_json           the exact workflow-definition snapshot this execution runs against. The
--                       Redis job already carries this at run time, but by the time a human
--                       answers a paused step (minutes to days later), that in-flight message is
--                       long gone. Resume must reuse the SAME snapshot the run started with —
--                       never re-fetch the mutable `mafs.maf_json`, which may have changed since
--                       (§2.3 #6) — so it is now durably persisted alongside the execution row.
--
--   output_generation  the planner's synthesis guideline (LLM call 1's output), produced once per
--                       run and, until now, held only in a local variable inside `run_maf` — lost
--                       entirely on resume. Persisting it lets a resumed execution's final answer
--                       follow the same guidance the original run planned, instead of silently
--                       falling back to a generic default or re-planning (which could disagree
--                       with the per-step prompts/extraction goals already fixed and persisted).
--
-- `maf_executions.status` has no CHECK constraint (see 0004_maf.sql), so no migration is needed to
-- add the new `awaiting_human` status value the executor/worker now write.
-- =============================================================================

ALTER TABLE maf_executions
    ADD COLUMN maf_json          JSONB,
    ADD COLUMN output_generation TEXT;
