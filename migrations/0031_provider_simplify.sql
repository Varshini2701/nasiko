-- Make default_model optional: models are discovered via catalog sync, not
-- required at registration time. Existing rows keep their value.
ALTER TABLE custom_providers ALTER COLUMN default_model DROP NOT NULL;

-- Portkey pricing slug (optional): the admin can specify which Portkey provider
-- slug to use for automatic pricing sync. When NULL the hostname mapping or
-- the label is used as fallback.
ALTER TABLE custom_providers ADD COLUMN portkey_slug TEXT;
