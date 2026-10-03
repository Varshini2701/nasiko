-- Two fixes to the pricing seed, both found by auditing every seeded row in
-- `model_pricing` against the upstream price books on 2026-09-29.
--
-- Part 1 corrects seven rates that disagree with upstream. Part 2 is the reason
-- they were able to drift for so long, and matters more.
--
-- Migration 0006 gave its rows human-readable notes ('o3', 'GPT-4o standard',
-- 'DeepSeek R1', ...). `sync_managed_note` in the pricing sync recognizes only
-- rows it wrote itself, so it read every one of these as an operator-set rate —
-- a negotiated price no public book carries — and deliberately skipped them,
-- logging "preserving operator price row". The result: 27 rows frozen at their
-- authoring-day values forever, which is why `openai` had 174 correctly synced
-- rows sitting beside 13 stale ones. The 174 had no seed row to "protect".
--
-- Rows are matched on the exact (provider, model, notes) triples that 0006 and
-- 0020 wrote, never on a pattern: a genuine operator rate must not be swept up
-- and handed to the sync.

-- ---------------------------------------------------------------------------
-- Part 1: correct the seven rates that disagree with upstream.
--
-- Closed and reopened, not updated in place, so costs already quoted against
-- the old rate stay as quoted and only new calls see the correction.
--
--   openai/o3                  10.00/40.00 -> 2.00/8.00      (OpenAI's ~80% cut)
--   openai/o1-mini              3.00/12.00 -> 1.10/4.40
--   deepseek/deepseek-reasoner  0.55/2.19  -> 0.14/0.28      (unified V4 pricing)
--   deepseek/deepseek-v4-pro    0.55/2.19  -> 0.435/0.87
--   deepseek/deepseek-chat      cache reads 0.014 -> 0.0028, writes free
--   gemini|google/gemini-2.5-flash  0.15/0.60 -> 0.30/2.50
--
-- Cache writes of 0 are a real price, not a missing one: OpenAI and DeepSeek do
-- not charge for them. Gemini's book lists a cache read but no write, so the
-- write stays NULL — a listed read price does not establish that writes are free.
-- deepseek-v4-pro's upstream cache read is 0.003625, stored as 0.0036 because the
-- column is numeric(10,4); that is what the sync itself would write.
UPDATE model_pricing m
   SET effective_until = now()
  FROM (VALUES
      ('openai',   'o3',                 'o3'),
      ('openai',   'o1-mini',            'o1 mini'),
      ('deepseek', 'deepseek-chat',      'DeepSeek Chat'),
      ('deepseek', 'deepseek-reasoner',  'DeepSeek R1'),
      ('deepseek', 'deepseek-v4-pro',    'boot seed (static list)'),
      ('gemini',   'gemini-2.5-flash',   'boot seed (static list)'),
      ('google',   'gemini-2.5-flash',   'Gemini 2.5 Flash')
  ) AS s (provider, model, notes)
 WHERE m.provider = s.provider
   AND m.model = s.model
   AND m.notes = s.notes
   AND m.effective_until IS NULL;

INSERT INTO model_pricing
    (provider, model, input_price_per_1m, output_price_per_1m,
     cache_creation_price_per_1m, cache_read_price_per_1m, notes)
SELECT v.provider, v.model, v.input, v.output, v.cache_write, v.cache_read, v.notes
  FROM (
      VALUES
          ('openai', 'o3',
           2.0000::NUMERIC(10,4), 8.0000::NUMERIC(10,4),
           0.0000::NUMERIC(10,4), 0.5000::NUMERIC(10,4),
           'seed: OpenAI o3 list price, verified 2026-09-29'),
          ('openai', 'o1-mini',
           1.1000::NUMERIC(10,4), 4.4000::NUMERIC(10,4),
           0.0000::NUMERIC(10,4), 0.5500::NUMERIC(10,4),
           'seed: OpenAI o1-mini list price, verified 2026-09-29'),
          ('deepseek', 'deepseek-chat',
           0.1400::NUMERIC(10,4), 0.2800::NUMERIC(10,4),
           0.0000::NUMERIC(10,4), 0.0028::NUMERIC(10,4),
           'seed: DeepSeek Chat list price, verified 2026-09-29'),
          ('deepseek', 'deepseek-reasoner',
           0.1400::NUMERIC(10,4), 0.2800::NUMERIC(10,4),
           0.0000::NUMERIC(10,4), 0.0028::NUMERIC(10,4),
           'seed: DeepSeek Reasoner list price, verified 2026-09-29'),
          ('deepseek', 'deepseek-v4-pro',
           0.4350::NUMERIC(10,4), 0.8700::NUMERIC(10,4),
           0.0000::NUMERIC(10,4), 0.0036::NUMERIC(10,4),
           'seed: DeepSeek V4 Pro list price, verified 2026-09-29'),
          ('gemini', 'gemini-2.5-flash',
           0.3000::NUMERIC(10,4), 2.5000::NUMERIC(10,4),
           NULL::NUMERIC(10,4), 0.0300::NUMERIC(10,4),
           'seed: Gemini 2.5 Flash list price, verified 2026-09-29'),
          ('google', 'gemini-2.5-flash',
           0.3000::NUMERIC(10,4), 2.5000::NUMERIC(10,4),
           NULL::NUMERIC(10,4), 0.0300::NUMERIC(10,4),
           'seed: Gemini 2.5 Flash list price, verified 2026-09-29')
  ) AS v (provider, model, input, output, cache_write, cache_read, notes)
 WHERE NOT EXISTS (
     SELECT 1 FROM model_pricing m
      WHERE m.provider = v.provider
        AND m.model = v.model
        AND m.effective_until IS NULL
 );

-- ---------------------------------------------------------------------------
-- Part 2: hand the remaining seeded rows back to the pricing sync.
--
-- Prefixing the note with `seed:` is all that is needed — `sync_managed_note`
-- recognizes that prefix, so from here these rows are refreshed from the
-- upstream book like any other instead of being mistaken for operator rates.
-- The original description is kept after the prefix.
--
-- This does not correct anything by itself. It is what stops the next stale
-- rate from needing a migration at all.
UPDATE model_pricing m
   SET notes = 'seed: ' || m.notes
  FROM (VALUES
      ('openai',         'gpt-4o',                     'GPT-4o standard'),
      ('openai',         'gpt-4o-mini',                'GPT-4o mini'),
      ('openai',         'gpt-4.1',                    'GPT-4.1'),
      ('openai',         'gpt-4.1-mini',               'GPT-4.1 mini'),
      ('openai',         'gpt-4.1-nano',               'GPT-4.1 nano'),
      ('openai',         'gpt-4-turbo',                'GPT-4 Turbo'),
      ('openai',         'gpt-3.5-turbo',              'GPT-3.5 Turbo'),
      ('openai',         'o1-preview',                 'o1 preview'),
      ('openai',         'o3-mini',                    'o3 mini'),
      ('openai',         'text-embedding-3-small',     'OpenAI text-embedding-3-small'),
      ('openai',         'text-embedding-3-large',     'OpenAI text-embedding-3-large'),
      ('anthropic',      'claude-opus-4',              'Claude Opus 4'),
      ('anthropic',      'claude-sonnet-4',            'Claude Sonnet 4'),
      ('anthropic',      'claude-haiku-4',             'Claude Haiku 4'),
      ('anthropic',      'claude-3-5-sonnet',          'Claude 3.5 Sonnet'),
      ('anthropic',      'claude-3-5-haiku',           'Claude 3.5 Haiku'),
      ('anthropic',      'claude-3-5-sonnet-20241022', 'Claude 3.5 Sonnet'),
      ('anthropic',      'claude-3-5-haiku-20241022',  'Claude 3.5 Haiku'),
      ('google',         'gemini-2.5-pro',             'Gemini 2.5 Pro'),
      ('gemini',         'gemini-1.5-pro',             'Gemini 1.5 Pro'),
      ('gemini',         'gemini-1.5-flash',           'Gemini 1.5 Flash'),
      ('gemini',         'gemini-2.0-flash',           'Gemini 2.0 Flash'),
      ('groq',           'llama-3.3-70b-versatile',    'Llama 3.3 70B on Groq'),
      ('groq',           'llama-3.1-8b-instant',       'Llama 3.1 8B on Groq'),
      ('deepseek',       'deepseek-v4-flash',          'DeepSeek V4 Flash'),
      ('amazon-bedrock', 'openai.gpt-5.6-sol',
       'GPT-5.6 Sol via Amazon Bedrock Mantle, standard context')
  ) AS s (provider, model, notes)
 WHERE m.provider = s.provider
   AND m.model = s.model
   AND m.notes = s.notes
   AND m.effective_until IS NULL;
