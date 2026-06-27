use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
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

pub fn telemetry_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(post_telemetry))
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
