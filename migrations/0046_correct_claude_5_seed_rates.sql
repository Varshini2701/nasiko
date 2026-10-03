-- Correct the Claude 5 seed rates written by migration 0020, which carried the
-- Claude 4 Opus/Sonnet rates forward as placeholders ("verify") and were never
-- verified. Opus 5 was being billed at 3x its true rate, Sonnet 5 at 1.5x.
--
-- Verified 2026-09-29 against two independent sources that agree exactly —
-- Anthropic's published list prices and the Portkey price book that the pricing
-- sync itself reads (configs.portkey.ai/pricing/anthropic.json):
--
--                    input   output   cache write   cache read   1h cache write
--   claude-opus-5     5.00    25.00          6.25         0.50            10.00
--   claude-sonnet-5   2.00    10.00          2.50         0.20             4.00
--
-- The 1h cache-write rate was NULL on the old rows, so Anthropic 1h writes fell
-- back to the `input * 2` inference in nasiko-pricing's context pricing. That
-- inference is right for every Anthropic model checked, but it marks the call
-- `estimated`; setting the column makes it a looked-up rate instead.
--
-- Closed and reopened rather than updated in place. `model_pricing` carries real
-- price history and `DbPriceBook` resolves a call against the row that was
-- effective when the call happened, so every cost already quoted stays exactly as
-- quoted — only calls from here on pick up the corrected rate.

-- Only the two placeholder rows are closed. A row the pricing sync has since
-- written is already correct and is left untouched, along with any operator rate.
UPDATE model_pricing
   SET effective_until = now()
 WHERE provider = 'anthropic'
   AND effective_until IS NULL
   AND notes IN (
       'Claude Opus 5 - rate carried forward from Opus 4, verify',
       'Claude Sonnet 5 - rate carried forward from Sonnet 4, verify'
   );

-- Gap-filling, like the boot seed: if an active row survived the close above, it
-- came from the sync or an operator and must keep winning. Inserting regardless
-- would shadow it, since a lookup takes the newest effective row.
INSERT INTO model_pricing
    (provider, model, input_price_per_1m, output_price_per_1m,
     cache_creation_price_per_1m, cache_read_price_per_1m,
     cache_creation_1h_price_per_1m, notes)
SELECT v.provider, v.model, v.input, v.output, v.cache_write, v.cache_read,
       v.cache_write_1h, v.notes
  FROM (
      VALUES
          ('anthropic', 'claude-opus-5',
           5.00::DECIMAL(10,4), 25.00::DECIMAL(10,4), 6.25::DECIMAL(10,4),
           0.50::DECIMAL(10,4), 10.00::DECIMAL(10,4),
           'seed: Claude Opus 5 list price, verified 2026-09-29'),
          ('anthropic', 'claude-sonnet-5',
           2.00::DECIMAL(10,4), 10.00::DECIMAL(10,4), 2.50::DECIMAL(10,4),
           0.20::DECIMAL(10,4), 4.00::DECIMAL(10,4),
           'seed: Claude Sonnet 5 list price, verified 2026-09-29')
  ) AS v (provider, model, input, output, cache_write, cache_read,
          cache_write_1h, notes)
 WHERE NOT EXISTS (
     SELECT 1 FROM model_pricing m
      WHERE m.provider = v.provider
        AND m.model = v.model
        AND m.effective_until IS NULL
 );
