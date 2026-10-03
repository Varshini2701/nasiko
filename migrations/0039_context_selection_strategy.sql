-- Per-user conversation-history context-selection strategy preference:
-- pacms (budget-aware, coverage-diversified; default), topk (pure
-- cosine-similarity ranking over query/answer pairs, no token budget), or
-- lastk (plain recency, no embeddings at all).
CREATE TYPE context_selection_strategy AS ENUM ('pacms', 'topk', 'lastk');

ALTER TABLE users
    ADD COLUMN context_selection_strategy context_selection_strategy NOT NULL DEFAULT 'pacms';
