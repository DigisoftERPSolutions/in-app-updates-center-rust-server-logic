-- Per-ABI release variants — a release can now target a specific CPU
-- architecture (separate arm64-v8a / armeabi-v7a APKs under the same
-- version_no) instead of always being a single universal APK.
-- NULL abi = universal, matches any requesting device (today's behavior,
-- preserved for every existing row).
ALTER TABLE releases ADD COLUMN IF NOT EXISTS abi TEXT;

-- Migration 003's company-scoped uniqueness only allowed ONE row per
-- (company, version_no), which blocks uploading both an arm64-v8a and an
-- armeabi-v7a build under the same version number. Replace with uniqueness
-- per (company, version_no, abi) instead. COALESCE(abi, '') normalizes NULL
-- so "universal" rows are still deduped against each other — Postgres
-- otherwise treats every NULL as distinct in a unique index, which would
-- silently allow duplicate universal releases for the same version.
DROP INDEX IF EXISTS idx_releases_company_version;
DROP INDEX IF EXISTS idx_releases_global_version;

CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_company_version_abi
    ON releases (company_url, company_prefix, version_no, COALESCE(abi, ''))
    WHERE company_url IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_releases_global_version_abi
    ON releases (version_no, COALESCE(abi, ''))
    WHERE company_url IS NULL;
