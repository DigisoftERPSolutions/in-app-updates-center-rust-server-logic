-- Every call to the meter-OCR cloud fallback (POST /meter-ocr/fallback), win
-- or lose. This is the endpoint's only real safety net: since it's a public,
-- unauthenticated, per-call-billed device endpoint (same trust model as
-- /telemetry and GET /configure), the daily-cap check in operations.rs reads
-- this table before making the Anthropic call, and it's what gives anyone
-- looking at cost a per-company breakdown instead of one opaque API bill.
CREATE TABLE IF NOT EXISTS meter_ocr_fallback_calls (
    id             BIGSERIAL   PRIMARY KEY,
    company_prefix TEXT        NOT NULL,
    company_url    TEXT        NOT NULL DEFAULT '',
    site           TEXT        NOT NULL DEFAULT '',
    device_id      TEXT        NOT NULL DEFAULT '',
    expected_mode  TEXT        NOT NULL,
    ok             BOOLEAN     NOT NULL,
    error          TEXT,
    top_token      TEXT,
    bottom_token   TEXT,
    confidence     TEXT,
    model          TEXT        NOT NULL,
    input_tokens   INTEGER,
    output_tokens  INTEGER,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The daily-cap check (COUNT(*) for a company_prefix since midnight UTC) and
-- the per-company cost dashboard are the only two query shapes against this
-- table, and both filter on (company_prefix, created_at).
CREATE INDEX IF NOT EXISTS idx_meter_ocr_fallback_company_day
    ON meter_ocr_fallback_calls (company_prefix, created_at);
