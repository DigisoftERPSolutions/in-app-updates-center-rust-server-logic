-- Optional last-known GPS coordinates on a ping. NULL unless the device had
-- a GPS fix available at ping time (best-effort, piggybacked on the
-- existing telemetry ping cadence — this is NOT a dedicated high-frequency
-- location tracker; see the mobile client for how/when it's populated).
ALTER TABLE app_pings ADD COLUMN IF NOT EXISTS lat DOUBLE PRECISION;
ALTER TABLE app_pings ADD COLUMN IF NOT EXISTS lon DOUBLE PRECISION;
ALTER TABLE app_pings ADD COLUMN IF NOT EXISTS location_accuracy DOUBLE PRECISION;

-- The device map only ever needs the MOST RECENT ping per device_id, and
-- that lookup should stay fast as app_pings grows into the millions of
-- rows. Partial index — only pings that actually carry a fix are worth
-- indexing for this purpose.
CREATE INDEX IF NOT EXISTS idx_app_pings_device_location
    ON app_pings (device_id, pinged_at DESC)
    WHERE lat IS NOT NULL AND lon IS NOT NULL;
