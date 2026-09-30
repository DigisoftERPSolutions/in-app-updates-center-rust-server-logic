WITH scoped AS (
    SELECT * FROM app_pings
    WHERE ($1::text IS NULL OR company_url = $1)
      AND ($2::text IS NULL OR company_prefix = $2)
      AND ($3::text IS NULL OR device_id = $3)
), latest AS (
    SELECT DISTINCT ON (company_url, company_prefix, device_id) *
    FROM scoped
    ORDER BY company_url, company_prefix, device_id, pinged_at DESC, id DESC
), totals AS (
    SELECT company_url, company_prefix, device_id,
           COUNT(*) AS ping_count, MIN(pinged_at) AS first_ping,
           ARRAY_AGG(DISTINCT user_id ORDER BY user_id) AS users,
           ARRAY_AGG(DISTINCT ip_address ORDER BY ip_address) AS ip_addresses
    FROM scoped
    GROUP BY company_url, company_prefix, device_id
), gps AS (
    SELECT DISTINCT ON (company_url, company_prefix, device_id)
           company_url, company_prefix, device_id, lat, lon, location_accuracy,
           pinged_at AS location_pinged_at
    FROM scoped
    WHERE lat BETWEEN -90 AND 90 AND lon BETWEEN -180 AND 180
    ORDER BY company_url, company_prefix, device_id, pinged_at DESC, id DESC
)
SELECT l.device_id, l.company_url, l.company_prefix, l.company_name,
       l.user_id, l.model, l.brand, l.app_version, l.android_version, l.abi,
       t.ping_count, t.first_ping, t.users, t.ip_addresses,
       g.lat, g.lon, g.location_accuracy, g.location_pinged_at, l.pinged_at
FROM latest l
JOIN totals t USING (company_url, company_prefix, device_id)
LEFT JOIN gps g USING (company_url, company_prefix, device_id)
ORDER BY l.pinged_at DESC, l.company_url, l.company_prefix, l.device_id
