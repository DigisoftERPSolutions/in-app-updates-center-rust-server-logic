-- Scope releases to a specific company (per-company updates), keyed by the
-- same (company_url, company_prefix) pair the telemetry ping already uses.
-- A release with company_url/company_prefix left NULL is a "global" release
-- that applies to every company (today's behavior, preserved).
ALTER TABLE releases ADD COLUMN IF NOT EXISTS company_url TEXT;
ALTER TABLE releases ADD COLUMN IF NOT EXISTS company_prefix TEXT;

-- The old constraint made version_no unique across ALL companies, which
-- breaks the moment two different companies are each pushed to their own
-- "3.2.31" independently. Replace it with scoped uniqueness instead.
ALTER TABLE releases DROP CONSTRAINT IF EXISTS unique_version_no;

CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_company_version
    ON releases (company_url, company_prefix, version_no)
    WHERE company_url IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_global_version
    ON releases (version_no)
    WHERE company_url IS NULL;

CREATE INDEX IF NOT EXISTS idx_releases_company_lookup
    ON releases (company_url, company_prefix, created_at DESC);
