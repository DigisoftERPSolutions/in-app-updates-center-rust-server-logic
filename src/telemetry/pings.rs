use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::authentication::guard::require_auth;

#[derive(Debug, Deserialize)]
pub struct TelemetryPayload {
    pub company_url: Option<String>,
    pub company_prefix: String,
    pub company_name: Option<String>,
    pub user_id: String,
    pub app_version: String,
    pub abi: String,
    pub model: String,
    pub brand: String,
    pub android_version: String,
    pub device_id: String,
    pub timestamp: Option<String>,
    /// Best-effort last-known GPS fix, piggybacked on this same ping — the
    /// mobile app only sends these when it already has a recent fix handy
    /// (see `submit_device_location`'s existing GPS plumbing), it never
    /// requests location just for this. All three travel together or not
    /// at all.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location_accuracy: Option<f64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct PingRow {
    pub id: i64,
    pub company_url: String,
    pub company_prefix: String,
    pub company_name: String,
    pub user_id: String,
    pub app_version: String,
    pub abi: String,
    pub model: String,
    pub brand: String,
    pub android_version: String,
    pub device_id: String,
    pub ip_address: String,
    pub client_timestamp: Option<DateTime<Utc>>,
    pub pinged_at: DateTime<Utc>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location_accuracy: Option<f64>,
}

/// One row per device — its MOST RECENT ping, for the live device map.
/// Distinct from `PingRow` (which is one row per ping / a raw history feed)
/// because the map only cares about "where is this device right now",
/// never its full ping history.
#[derive(Serialize, sqlx::FromRow)]
pub struct DeviceLocationRow {
    pub device_id: String,
    pub company_url: String,
    pub company_prefix: String,
    pub company_name: String,
    pub user_id: String,
    pub model: String,
    pub brand: String,
    pub app_version: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location_accuracy: Option<f64>,
    pub pinged_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct PingQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    pub company_prefix: Option<String>,
}

fn default_page() -> i64 { 1 }
fn default_page_size() -> i64 { 20 }

#[derive(Serialize, sqlx::FromRow)]
pub struct SummaryTotals {
    pub total_pings: i64,
    pub distinct_devices: i64,
    pub distinct_companies: i64,
    pub active_24h: i64,
    pub active_1h: i64,
    /// Devices that have pinged at least once but haven't been heard from in
    /// over 7 days — gone quiet (offline, uninstalled, dead battery, staff
    /// left) rather than "never existed". A fleet health signal the raw
    /// active/total counts don't surface on their own.
    pub stale_7d: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct CompanyBreakdown {
    pub company_url: String,
    pub company_name: String,
    pub company_prefix: String,
    pub latest_version: String,
    pub last_seen: DateTime<Utc>,
    pub device_count: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct VersionBreakdown {
    pub app_version: String,
    pub device_count: i64,
}

/// Device pings (`POST /`) need no login — the mobile app never
/// authenticates as a user. Everything that lets an admin browse the fleet
/// (`GET /`, `GET /summary`) sits behind `require_auth`.
pub fn telemetry_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(post_telemetry))
        .merge(
            Router::new()
                .route("/", get(get_telemetry))
                .route("/summary", get(get_telemetry_summary))
                .route("/devices", get(get_device_locations))
                .route_layer(middleware::from_fn(require_auth)),
        )
        .with_state(db)
}

fn extract_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if let Some(val) = headers.get("x-forwarded-for") {
        if let Ok(s) = val.to_str() {
            if let Some(ip) = s.split(',').next() {
                return ip.trim().to_string();
            }
        }
    }
    peer.ip().to_string()
}

async fn post_telemetry(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(db): State<Arc<PgPool>>,
    Json(payload): Json<TelemetryPayload>,
) -> impl IntoResponse {
    let ip = extract_ip(&headers, peer);

    let client_ts: Option<DateTime<Utc>> = payload
        .timestamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // lat/lon/accuracy travel together or not at all — a fix with only two
    // of the three fields is nonsensical, so treat a partial set as "no fix"
    // rather than storing a half-populated location.
    let (lat, lon, location_accuracy) = match (payload.lat, payload.lon) {
        (Some(lat), Some(lon)) => (Some(lat), Some(lon), payload.location_accuracy),
        _ => (None, None, None),
    };

    let result = sqlx::query(
        r#"
        INSERT INTO app_pings
            (company_url, company_prefix, company_name, user_id,
             app_version, abi, model, brand, android_version,
             device_id, ip_address, client_timestamp, lat, lon, location_accuracy)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
        "#,
    )
    .bind(payload.company_url.unwrap_or_default())
    .bind(&payload.company_prefix)
    .bind(payload.company_name.unwrap_or_default())
    .bind(&payload.user_id)
    .bind(&payload.app_version)
    .bind(&payload.abi)
    .bind(&payload.model)
    .bind(&payload.brand)
    .bind(&payload.android_version)
    .bind(&payload.device_id)
    .bind(&ip)
    .bind(client_ts)
    .bind(lat)
    .bind(lon)
    .bind(location_accuracy)
    .execute(db.as_ref())
    .await;

    match result {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(e) => {
            tracing::error!("[TELEMETRY] DB error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_telemetry(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<PingQuery>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let page_size = params.page_size.clamp(1, 100);
    let offset = (page - 1) * page_size;

    let (rows, total) = match &params.company_prefix {
        Some(prefix) => {
            let rows = sqlx::query_as::<_, PingRow>(
                r#"SELECT id, company_url, company_prefix, company_name, user_id,
                          app_version, abi, model, brand, android_version,
                          device_id, ip_address, client_timestamp, pinged_at,
                          lat, lon, location_accuracy
                   FROM app_pings
                   WHERE company_prefix = $1
                   ORDER BY pinged_at DESC
                   LIMIT $2 OFFSET $3"#,
            )
            .bind(prefix)
            .bind(page_size)
            .bind(offset)
            .fetch_all(db.as_ref())
            .await;

            let total: Option<(i64,)> =
                sqlx::query_as("SELECT COUNT(*) FROM app_pings WHERE company_prefix = $1")
                    .bind(prefix)
                    .fetch_optional(db.as_ref())
                    .await
                    .unwrap_or(None);

            (rows, total)
        }
        None => {
            let rows = sqlx::query_as::<_, PingRow>(
                r#"SELECT id, company_url, company_prefix, company_name, user_id,
                          app_version, abi, model, brand, android_version,
                          device_id, ip_address, client_timestamp, pinged_at,
                          lat, lon, location_accuracy
                   FROM app_pings
                   ORDER BY pinged_at DESC
                   LIMIT $1 OFFSET $2"#,
            )
            .bind(page_size)
            .bind(offset)
            .fetch_all(db.as_ref())
            .await;

            let total: Option<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM app_pings")
                .fetch_optional(db.as_ref())
                .await
                .unwrap_or(None);

            (rows, total)
        }
    };

    match rows {
        Ok(data) => {
            let total_count = total.map(|t| t.0).unwrap_or(0);
            let total_pages = (total_count as f64 / page_size as f64).ceil() as i64;
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "data": data,
                    "meta": {
                        "page": page,
                        "page_size": page_size,
                        "total": total_count,
                        "total_pages": total_pages
                    }
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("[TELEMETRY] DB fetch error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

/// GET /telemetry/summary — aggregate KPIs for the management dashboard.
/// Deliberately pre-aggregated in SQL rather than shipping all ping rows to
/// the client: the fleet is already in the thousands of pings and only grows.
pub async fn get_telemetry_summary(State(db): State<Arc<PgPool>>) -> impl IntoResponse {
    let totals = sqlx::query_as::<_, SummaryTotals>(
        r#"
        SELECT
            COUNT(*)                                                                    AS total_pings,
            COUNT(DISTINCT device_id)                                                   AS distinct_devices,
            COUNT(DISTINCT company_url)                                                 AS distinct_companies,
            COUNT(DISTINCT device_id) FILTER (WHERE pinged_at > NOW() - INTERVAL '24 hours') AS active_24h,
            COUNT(DISTINCT device_id) FILTER (WHERE pinged_at > NOW() - INTERVAL '1 hour')    AS active_1h,
            (
                SELECT COUNT(*) FROM (
                    SELECT device_id FROM app_pings
                    GROUP BY device_id
                    HAVING MAX(pinged_at) < NOW() - INTERVAL '7 days'
                ) stale
            ) AS stale_7d
        FROM app_pings
        "#,
    )
    .fetch_one(db.as_ref())
    .await;

    let by_company = sqlx::query_as::<_, CompanyBreakdown>(
        r#"
        WITH latest AS (
            SELECT DISTINCT ON (company_url)
                company_url, company_name, company_prefix,
                app_version AS latest_version, pinged_at AS last_seen
            FROM app_pings
            ORDER BY company_url, pinged_at DESC
        ),
        counts AS (
            SELECT company_url, COUNT(DISTINCT device_id) AS device_count
            FROM app_pings
            GROUP BY company_url
        )
        SELECT l.company_url, l.company_name, l.company_prefix,
               l.latest_version, l.last_seen, c.device_count
        FROM latest l
        JOIN counts c ON c.company_url = l.company_url
        ORDER BY l.last_seen DESC
        "#,
    )
    .fetch_all(db.as_ref())
    .await;

    let by_version = sqlx::query_as::<_, VersionBreakdown>(
        r#"
        SELECT app_version, COUNT(DISTINCT device_id) AS device_count
        FROM app_pings
        GROUP BY app_version
        ORDER BY app_version DESC
        "#,
    )
    .fetch_all(db.as_ref())
    .await;

    match (totals, by_company, by_version) {
        (Ok(totals), Ok(by_company), Ok(by_version)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "totals": totals,
                "by_company": by_company,
                "by_version": by_version,
            })),
        )
            .into_response(),
        (t, c, v) => {
            if let Some(e) = t.as_ref().err().or(c.as_ref().err()).or(v.as_ref().err()) {
                tracing::error!("[TELEMETRY] summary query failed: {e}");
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct DeviceLocationQuery {
    pub company_prefix: Option<String>,
}

/// GET /telemetry/devices — the live device map's data source: one row per
/// device, its MOST RECENT ping only (`DISTINCT ON`), optionally scoped to a
/// company. Devices that have never sent a GPS fix are still returned (with
/// lat/lon null) so the map/list can show "N devices, M with a known
/// location" rather than silently dropping the rest.
pub async fn get_device_locations(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<DeviceLocationQuery>,
) -> impl IntoResponse {
    let has_company_filter = params.company_prefix.is_some();
    let company_prefix = params.company_prefix.unwrap_or_default();

    let rows = sqlx::query_as::<_, DeviceLocationRow>(
        r#"
        SELECT DISTINCT ON (device_id)
            device_id, company_url, company_prefix, company_name, user_id,
            model, brand, app_version, lat, lon, location_accuracy, pinged_at
        FROM app_pings
        WHERE NOT $1 OR company_prefix = $2
        ORDER BY device_id, pinged_at DESC
        "#,
    )
    .bind(has_company_filter)
    .bind(&company_prefix)
    .fetch_all(db.as_ref())
    .await;

    match rows {
        Ok(data) => (StatusCode::OK, Json(serde_json::json!({ "data": data }))).into_response(),
        Err(e) => {
            tracing::error!("[TELEMETRY] device location fetch failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}
