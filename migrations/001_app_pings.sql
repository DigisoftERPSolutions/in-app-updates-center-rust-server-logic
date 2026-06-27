CREATE TABLE IF NOT EXISTS app_pings (
    id               BIGSERIAL   PRIMARY KEY,
    company_url      TEXT        NOT NULL DEFAULT '',
    company_prefix   TEXT        NOT NULL,
    company_name     TEXT        NOT NULL DEFAULT '',
    user_id          TEXT        NOT NULL,
    app_version      TEXT        NOT NULL,
    abi              TEXT        NOT NULL,
    model            TEXT        NOT NULL,
    brand            TEXT        NOT NULL,
    android_version  TEXT        NOT NULL,
    device_id        TEXT        NOT NULL,
    ip_address       TEXT        NOT NULL,
    client_timestamp TIMESTAMPTZ,
    pinged_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_app_pings_company_month
    ON app_pings (company_prefix, pinged_at);
