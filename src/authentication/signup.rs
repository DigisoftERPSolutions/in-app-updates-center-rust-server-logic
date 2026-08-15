use axum::{
    extract::{State, Path},
    http::StatusCode,
    response::IntoResponse,
    routing::{ post, delete},
    Json, Router,
};
use bcrypt::{hash, DEFAULT_COST};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

// ── Shared app state ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<PgPool>,
}

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignUpPayload {
    #[serde(rename = "fullName")]
    pub full_name: String,
    pub email:    String,
    pub username: String,
    pub password: String,
    #[serde(rename = "confirmPassword")]
    pub confirm_password: String,
}

#[derive(Deserialize)]
pub struct UpdateUserPayload {
    pub username: Option<String>,
    pub email:    Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UserResponse {
    pub id: uuid::Uuid,
    pub full_name: String,
    pub username: String,
    pub email: String,
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn signup_route(db: Arc<PgPool>) -> Router {
    let state = AppState { db };

    Router::new()
        .route(
            "/",
            post(handle_sign_up).get(list_all_users),
        )
        .route(
            "/{id}",
            delete(delete_user).patch(update_user),
        )
        .with_state(state)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /signup
/// Body: { "username": "...", "email": "...", "password": "..." }
pub async fn handle_sign_up(
    State(state): State<AppState>,
    Json(payload): Json<SignUpPayload>,
) -> impl IntoResponse {



    // 1. compare then hash the password with bcrypt

    if payload.password != payload.confirm_password {
    tracing::error!("Passwords do not match");

    return (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "Passwords do not match"
        })),
    );
}


    let hashed_password = match hash(&payload.password, DEFAULT_COST) {
        Ok(h)  => h,
        Err(e) => {
            tracing::error!("Password hashing failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to hash password" })),
            );
        }
    };

    // 2. Generate a UUID for the new user
    let user_id = Uuid::new_v4();



    // 3. Insert into DB — pure SQL, no ORM
  let result = sqlx::query(
    r#"
    INSERT INTO users (
        id,
        full_name,
        email,
        username,
        password_hash,
        created_at
    )
    VALUES ($1, $2, $3, $4, $5, NOW())
    "#,
)
.bind(&user_id)
.bind(&payload.full_name)
.bind(&payload.email)
.bind(&payload.username)
.bind(&hashed_password)
.execute(state.db.as_ref())
.await;

    match result {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "message": "User created successfully",
                "id": user_id
            })),
        ),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            tracing::warn!("Duplicate email attempt: {}", payload.email);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "Email already registered" })),
            )
        }
        Err(e) => {
            tracing::error!("DB insert failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
        }
    }
}

/// GET /signup  — list every user (no passwords returned)
pub async fn list_all_users(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let result = sqlx::query_as::<_, UserResponse>(
        r#"SELECT id, full_name, email, username
FROM users
ORDER BY created_at DESC"#,
    )
    .fetch_all(state.db.as_ref())
    .await;

    match result {
        Ok(users) => (StatusCode::OK, Json(serde_json::json!(users))),
        Err(e) => {
            tracing::error!("Failed to list users: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Could not fetch users" })),
            )
        }
    }
}

/// DELETE /signup/:id
pub async fn delete_user(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    // `id` column is `uuid`, not `text` — binding a bare String here made
    // Postgres reject every call with a type-mismatch error (surfaced to the
    // caller as a generic "Database error"). Path<Uuid> both validates the
    // input and binds it as the right type.
    let result = sqlx::query(r#"DELETE FROM users WHERE id = $1"#)
        .bind(id)
        .execute(state.db.as_ref())
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "User not found" })),
        ),
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": "User deleted" })),
        ),
        Err(e) => {
            tracing::error!("Delete failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
        }
    }
}

/// PATCH /signup/:id
/// Body: { "username": "new_name" }  and/or  { "email": "new@email.com" }
pub async fn update_user(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateUserPayload>,
) -> impl IntoResponse {
    // Build the SET clause dynamically — only update fields that were provided.
    // Placeholders are numbered ($1, $2, ...) because sqlx's Postgres driver
    // uses positional params, not the `?` (MySQL-style) placeholder the
    // previous version used — that would fail on every call.
    let mut set_parts: Vec<String> = Vec::new();
    let mut next_param = 1;
    if payload.username.is_some() {
        set_parts.push(format!("username = ${next_param}"));
        next_param += 1;
    }
    if payload.email.is_some() {
        set_parts.push(format!("email = ${next_param}"));
        next_param += 1;
    }

    if set_parts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No fields to update" })),
        );
    }

    let sql = format!(
        "UPDATE users SET {} WHERE id = ${next_param}",
        set_parts.join(", ")
    );

    // Bind values in the same order as set_parts. `id` is bound as a real
    // Uuid (not text) — the `id` column is `uuid`, and binding a String
    // against it made Postgres reject every call with a type mismatch.
    let mut query = sqlx::query(&sql);
    if let Some(ref username) = payload.username { query = query.bind(username); }
    if let Some(ref email)    = payload.email    { query = query.bind(email);    }
    query = query.bind(id);

    match query.execute(state.db.as_ref()).await {
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "User not found" })),
        ),
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": "User updated" })),
        ),
        Err(e) => {
            tracing::error!("Update failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
        }
    }
}