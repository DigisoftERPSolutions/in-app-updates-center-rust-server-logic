-- Supports exact company/device lookups and deterministic latest-ping ordering.
-- No data backfill is needed: inventory totals use the existing full history.
CREATE INDEX IF NOT EXISTS idx_app_pings_company_device_history
    ON app_pings (company_url, company_prefix, device_id, pinged_at DESC, id DESC);
