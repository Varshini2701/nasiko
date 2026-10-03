-- Per-message cached-prompt counts, so the chat surface can report the whole prompt.
--
-- `input_tokens` holds the *fresh* prompt only — cached tokens are billed at a different rate
-- and are tracked separately in `token_usage`. Without the same split here, the chat chip adds
-- `input_tokens + output_tokens` and calls it "tokens", so the number collapses as caching
-- improves: the same six prompts replayed two minutes apart reported 2,873 tokens on the first
-- run and 957 on the second, for prompts that were 5,305 and 5,309 tokens respectively.
--
-- Nullable with no default, matching the other usage columns: NULL means "nothing recorded"
-- (a bring-your-own-key reply, or a row written before this migration), which is a different
-- thing from a genuine zero.
ALTER TABLE chat_messages
    ADD COLUMN cache_read_tokens INTEGER,
    ADD COLUMN cache_creation_tokens INTEGER;
