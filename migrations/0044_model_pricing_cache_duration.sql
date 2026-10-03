-- Preserve the upstream one-hour write rate separately from the default cache-write rate.
ALTER TABLE model_pricing
    ADD COLUMN cache_creation_1h_price_per_1m DECIMAL(10,4)
    CHECK (cache_creation_1h_price_per_1m >= 0);
