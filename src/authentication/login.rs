use std::sync::Arc;

use axum::{
    extract::State,
    response::IntoResponse,
    routing::post,
    Json,
    Router,
};
use bcrypt::verify;
use chrono::{Utc, Duration as ChronoDuration};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct LoginPayload {
    email: String,
    password: String,
}

// JWT claims struct — this is what gets encoded into the token
#[derive(Serialize, Deserialize)]
struct Claims {
    sub: Uuid,       // subject: the user's UUID
    email: String,
    exp: usize,      // expiry timestamp (required by JWT spec)
}

pub fn login_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(handle_login))
        .with_state(db)
}

async fn handle_login(
    State(db): State<Arc<PgPool>>,
    Json(payload): Json<LoginPayload>,
) -> impl IntoResponse {

    if let Err(msg) = validate(&payload) {
        return Json(serde_json::json!({ "error": msg }));
    }

    let user = sqlx::query(
        "SELECT id, email, password_hash FROM users WHERE email = $1"
    )
    .bind(&payload.email)
    .fetch_optional(db.as_ref())
    .await;

    match user {
        Ok(Some(user)) => {
            let password_hash: String = user.get("password_hash");
            let user_id: Uuid = user.get("id");
            let user_email: String = user.get("email");

            let is_valid: bool = match verify(&payload.password, &password_hash) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("bcrypt verify error: {e}");
                    return Json(serde_json::json!({
                        "error": "internal authentication error"
                    }));
                }
            };

            if !is_valid {
                return Json(serde_json::json!({
                    "error": "invalid credentials"
                }));
            }

            // Build claims — token expires in 24 hours
            let expiry = Utc::now()
                .checked_add_signed(ChronoDuration::hours(24))
                .expect("valid timestamp")
                .timestamp() as usize;

            let claims = Claims {
                sub: user_id,
                email: user_email,
                exp: expiry,
            };

            // Sign the token — load secret from env, never hardcode it
            let secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| {
                tracing::warn!("JWT_SECRET not set — using insecure fallback");
                "change_me".to_string()
            });

            match encode(
                &Header::default(),        // alg: HS256
                &claims,
                &EncodingKey::from_secret(secret.as_bytes()),
            ) {
                Ok(token) => Json(serde_json::json!({
                    "message": "login successful",
                    "token": token
                })),
                Err(e) => {
                    tracing::error!("JWT encode error: {e}");
                    Json(serde_json::json!({
                        "error": "failed to generate token"
                    }))
                }
            }
        }

        Ok(None) => Json(serde_json::json!({
            "error": "user not found"
        })),

        Err(e) => {
            tracing::error!("DB error: {e}");
            Json(serde_json::json!({
                "error": "database error"
            }))
        }
    }
}


fn validate(payload: &LoginPayload) -> Result<(), String> {
    let email = &payload.email;
    let password = &payload.password;

    // Step 1: empty check
    if email.trim().is_empty() || password.trim().is_empty() {
        return Err("both email and password are required".to_string());
    }

    // Step 2: email validation
    if !email.contains('@') || !email.contains('.') {
        return Err("invalid email address".to_string());
    }

    // Step 3: password rules
    if password.len() < 8 {
        return Err("password must be at least 8 characters long".to_string());
    }

    if !password.chars().any(|c| !c.is_alphanumeric()) {
        return Err("password must contain a special character".to_string());
    }

    Ok(())
}