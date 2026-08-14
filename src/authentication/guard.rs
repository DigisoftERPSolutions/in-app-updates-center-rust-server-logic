use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Mirrors the `Claims` struct signed in `authentication::login` — kept in
/// sync manually since the two modules don't share a type today.
#[derive(Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: Uuid,
    pub email: String,
    pub exp: usize,
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// Axum middleware guarding admin-only routes (sites/companies directory,
/// release management, fleet telemetry viewing). Requires a valid
/// `Authorization: Bearer <jwt>` header signed with the same `JWT_SECRET`
/// used by `authentication::login`. Device pings (`POST /telemetry`) are
/// intentionally NOT behind this guard — the mobile app never logs in.
pub async fn require_auth(mut req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match header.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) if !t.trim().is_empty() => t,
        _ => return unauthorized("missing or malformed Authorization header"),
    };

    let secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| {
        tracing::warn!("JWT_SECRET not set — using insecure fallback");
        "change_me".to_string()
    });

    match decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    ) {
        Ok(data) => {
            req.extensions_mut().insert(data.claims);
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!("[AUTH] rejected token: {e}");
            unauthorized("invalid or expired session — please log in again")
        }
    }
}
