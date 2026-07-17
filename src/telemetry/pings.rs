use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

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

pub fn telemetry_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(post_telemetry).get(get_telemetry))
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

    let result = sqlx::query(
        r#"
        INSERT INTO app_pings
            (company_url, company_prefix, company_name, user_id,
             app_version, abi, model, brand, android_version,
             device_id, ip_address, client_timestamp)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
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
                          device_id, ip_address, client_timestamp, pinged_at
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
                          device_id, ip_address, client_timestamp, pinged_at
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
