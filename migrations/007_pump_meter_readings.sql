-- `/meter-ocr/read` (see src/meter_ocr/operations.rs) is now the primary
-- capture-time read path, called on every meter photo rather than only as
-- an occasional manual fallback. To flag an obviously-wrong read (counter
-- went backwards, totalizer jumped by an implausible amount) it needs to
-- know the last CONFIRMED reading for the exact nozzle being photographed —
-- not the last model guess, which could itself have been wrong. This table
-- holds exactly that: one row per nozzle, upserted only by
-- `/meter-ocr/confirm` once an attendant has actually confirmed a reading
-- (never written by `/read` itself). pump_id/nozzle_id are TEXT because the
-- mobile client's ids for these are strings, not database-assigned ids.
CREATE TABLE IF NOT EXISTS pump_reading_state (
    company_prefix    TEXT             NOT NULL,
    company_url       TEXT             NOT NULL,
    site               TEXT             NOT NULL DEFAULT '',
    pump_id            TEXT             NOT NULL,
    nozzle_id          TEXT             NOT NULL,
    last_liters        DOUBLE PRECISION,
    last_sale_number   BIGINT,
    last_confirmed_at  TIMESTAMPTZ,
    updated_at         TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    PRIMARY KEY (company_prefix, company_url, site, pump_id, nozzle_id)
);

-- Append-only history of every attendant confirmation (shift open or close),
-- one row per confirm call, kept even when the attendant edited/overrode the
-- model's read or entered everything manually with no cloud read at all
-- (raw_model_response is NULL in that case). This is what lets us later go
-- back and tune the anomaly thresholds / prompt against real outcomes
-- instead of guessing — `pump_reading_state` only ever holds the latest
-- value, this table holds all of them.
CREATE TABLE IF NOT EXISTS pump_reading_audit (
    id                      BIGSERIAL        PRIMARY KEY,
    company_prefix         TEXT             NOT NULL,
    company_url            TEXT             NOT NULL,
    site                    TEXT             NOT NULL DEFAULT '',
    pump_id                 TEXT             NOT NULL,
    nozzle_id               TEXT             NOT NULL,
    shift_event             TEXT             NOT NULL, -- 'open' | 'close'
    raw_model_response      JSONB,
    confidence              TEXT,
    sale_token              TEXT,
    liters_token            TEXT,
    anomaly_flags           TEXT[]           NOT NULL DEFAULT '{}',
    needs_review            BOOLEAN          NOT NULL DEFAULT false,
    confirmed_sale_number   BIGINT,
    confirmed_liters        DOUBLE PRECISION,
    confirmed_reading       DOUBLE PRECISION,
    was_edited              BOOLEAN          NOT NULL DEFAULT false,
    image_ref               TEXT,
    attendant_id            TEXT,
    client_timestamp        TIMESTAMPTZ,
    created_at              TIMESTAMPTZ      NOT NULL DEFAULT NOW()
);

-- The only query shape this table needs so far: "show me the confirmation
-- history for this one nozzle, in order" — for a per-nozzle accuracy review.
CREATE INDEX IF NOT EXISTS idx_pump_reading_audit_nozzle
    ON pump_reading_audit (company_prefix, pump_id, nozzle_id, created_at);

-- Optional per-company overrides for the anomaly thresholds used in
-- src/meter_ocr/operations.rs (delta-too-large / sale-number-rollover
-- checks). Both nullable — NULL means "fall back to the global env-var
-- default", so most companies never need a row touched here at all; these
-- only exist for the rare station whose pumps/tanks genuinely move more
-- fuel per shift, or whose sale-number display wraps at a different ceiling,
-- than the global default assumes.
ALTER TABLE companies ADD COLUMN IF NOT EXISTS max_liters_delta_per_shift DOUBLE PRECISION;
ALTER TABLE companies ADD COLUMN IF NOT EXISTS sale_number_rollover_ceiling BIGINT;
