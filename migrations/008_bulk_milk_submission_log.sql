-- Accountability trail for the "Bulk Milk Purchase" form in the old_pos
-- browser app (murima/old_pos). Every submission ATTEMPT (success, partial,
-- or failure) fires one row here, sent directly from the browser straight
-- to this Rust API — completely separate from, and in addition to, the
-- existing PHP insert (milkfarming/farmers/insert_bulk.php), which is left
-- untouched. This table exists purely so management can answer "who
-- submitted what, from which machine, when, and from where" even though the
-- PHP backend itself keeps no such record.
CREATE TABLE IF NOT EXISTS bulk_milk_submission_log (
    id                       BIGSERIAL        PRIMARY KEY,

    -- Correlates 1:1 with the batch_id sent to insert_bulk.php, so a row
    -- here can be matched back to the actual inserted milk records.
    batch_id                 TEXT             NOT NULL,

    company_url              TEXT             NOT NULL DEFAULT '',
    company_prefix           TEXT             NOT NULL DEFAULT '',
    site_name                TEXT             NOT NULL DEFAULT '',

    user_id                  TEXT             NOT NULL DEFAULT '',
    username                 TEXT             NOT NULL DEFAULT '',
    real_name                TEXT             NOT NULL DEFAULT '',
    email                    TEXT             NOT NULL DEFAULT '',

    -- What was submitted
    route_id                 TEXT             NOT NULL DEFAULT '',
    route_name                TEXT            NOT NULL DEFAULT '',
    store_code                TEXT            NOT NULL DEFAULT '',
    store_name                 TEXT           NOT NULL DEFAULT '',
    shift                      TEXT           NOT NULL DEFAULT '',
    pricing_mode                TEXT          NOT NULL DEFAULT '',
    reference_number             TEXT         NOT NULL DEFAULT '',
    invoice_date                  TEXT,
    due_date                       TEXT,
    confirmed_resubmit              BOOLEAN   NOT NULL DEFAULT false,
    record_count                     INTEGER  NOT NULL DEFAULT 0,
    total_quantity                    DOUBLE PRECISION NOT NULL DEFAULT 0,
    -- [{ "supplier_id": "...", "quantity": 12.5 }, ...] — the exact farmer
    -- lines in this attempt, so a disputed batch can be inspected line by
    -- line without needing the PHP database.
    records                            JSONB,

    -- Outcome of THIS attempt (mirrors SubmitBatchResult in the browser)
    status                              TEXT   NOT NULL, -- 'success' | 'partial' | 'failure'
    duplicate_count                      INTEGER,
    error_message                         TEXT,

    -- Accountability / device fingerprint
    ip_address                             TEXT NOT NULL DEFAULT '',
    user_agent                              TEXT NOT NULL DEFAULT '',
    browser_name                             TEXT,
    browser_version                           TEXT,
    os_name                                    TEXT,
    os_version                                  TEXT,
    platform                                     TEXT,
    screen_width                                  INTEGER,
    screen_height                                  INTEGER,
    viewport_width                                  INTEGER,
    viewport_height                                  INTEGER,
    device_pixel_ratio                                DOUBLE PRECISION,
    color_depth                                        INTEGER,
    timezone                                            TEXT,
    timezone_offset_minutes                              INTEGER,
    language                                              TEXT,
    hardware_concurrency                                   INTEGER,
    device_memory_gb                                        DOUBLE PRECISION,

    -- Best-effort last-known GPS fix — only populated if the browser had
    -- (and the user granted) geolocation, same "all or nothing" contract as
    -- app_pings.lat/lon/location_accuracy.
    lat                                                      DOUBLE PRECISION,
    lon                                                        DOUBLE PRECISION,
    location_accuracy                                           DOUBLE PRECISION,

    client_timestamp                                             TIMESTAMPTZ,
    submitted_at                                                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The admin log page's two query shapes: "everything for this company,
-- newest first" and "everything this user has ever submitted".
CREATE INDEX IF NOT EXISTS idx_bulk_milk_log_company_date
    ON bulk_milk_submission_log (company_prefix, submitted_at DESC);

CREATE INDEX IF NOT EXISTS idx_bulk_milk_log_user
    ON bulk_milk_submission_log (user_id, submitted_at DESC);

-- Looking up every attempt behind one specific batch (e.g. investigating a
-- disputed insert).
CREATE INDEX IF NOT EXISTS idx_bulk_milk_log_batch
    ON bulk_milk_submission_log (batch_id);
