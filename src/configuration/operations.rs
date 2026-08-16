use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::authentication::guard::require_auth;

/// `GET /` (list releases) is the mobile app's live update-check call —
/// it's already deployed on real devices, calls with no auth at all, and
/// must keep working unauthenticated (release metadata isn't sensitive).
/// Creating/deleting releases is an admin action and stays behind
/// `require_auth`.
pub fn version_control(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", get(get_version_uploads))
        .merge(
            Router::new()
                .route("/", post(handle_version_post_control))
                .route(
                    "/{id}",
                    axum::routing::patch(update_release).delete(delete_release),
                )
                .route_layer(middleware::from_fn(require_auth)),
        )
        .with_state(db)
}

#[derive(Deserialize)]
pub struct ConfigPayload {
    pub url: String,
    pub force_update: bool,
    pub reason_for_update: String,
    pub update_display: String,
    pub memo: String,
    pub version_no: String,
    /// When set (together with company_prefix), this release only applies to
    /// that one company. Left out/null, the release is global — delivered to
    /// every site, matching the previous (pre-per-company) behavior.
    pub company_url: Option<String>,
    pub company_prefix: Option<String>,
    /// When set, this release only applies to devices reporting that exact
    /// ABI (e.g. "arm64-v8a", "armeabi-v7a") — lets the same version_no ship
    /// as two separate APKs, one per architecture. Left out/null, the
    /// release is universal and matches any device's ABI.
    pub abi: Option<String>,
}
// add new field called tag, which shows the most recent version
#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    /// When provided (with company_prefix), scopes results to releases
    /// targeted at that company PLUS any global (company_url IS NULL)
    /// releases. Omit both to get the unfiltered admin view of everything.
    pub company_url: Option<String>,
    pub company_prefix: Option<String>,
    /// The requesting device's own ABI. When provided, narrows results to
    /// releases that are either universal (abi IS NULL) or built for that
    /// exact ABI. Omit to skip ABI filtering (the admin dashboard listing
    /// wants to see every variant).
    pub abi: Option<String>,
}

fn default_page() -> i64 { 1 }
fn default_page_size() -> i64 { 20 }

#[derive(Serialize, sqlx::FromRow)]
pub struct ReleaseRow {
    pub id: Uuid,
    pub url: String,
    pub version_no: String,
    pub force_update: bool,
    pub reason_for_update: String,
    pub update_display: String,
    pub memo: String,
    pub company_url: Option<String>,
    pub company_prefix: Option<String>,
    pub abi: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn get_version_uploads(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let page_size = params.page_size.clamp(1, 100);
    let offset = (page - 1) * page_size;

    // No company given -> admin view, everything. Company given -> that
    // company's own releases plus global ones, most recent first.
    let has_company_filter = params.company_url.is_some();
    let company_url = params.company_url.unwrap_or_default();
    let company_prefix = params.company_prefix.unwrap_or_default();

    // Same idea for ABI: no abi given (admin dashboard) -> see every variant.
    // ABI given (a real device checking in) -> only universal releases or
    // ones built for that exact architecture.
    let has_abi_filter = params.abi.is_some();
    let abi = params.abi.unwrap_or_default();

    let releases = sqlx::query_as::<_, ReleaseRow>(r#"
        SELECT
            id, url, version_no, force_update, reason_for_update,
            update_display, memo, company_url, company_prefix, abi, created_at
        FROM releases
        WHERE (NOT $4
           OR company_url IS NULL
           OR (company_url = $1 AND company_prefix = $2))
          AND (NOT $6 OR abi IS NULL OR abi = $7)
        ORDER BY created_at DESC
        LIMIT $3 OFFSET $5
    "#)
    .bind(&company_url)
    .bind(&company_prefix)
    .bind(page_size)
    .bind(has_company_filter)
    .bind(offset)
    .bind(has_abi_filter)
    .bind(&abi)
    .fetch_all(db.as_ref())
    .await;

    let total: Option<(i64,)> = sqlx::query_as(r#"
        SELECT COUNT(*) FROM releases
        WHERE (NOT $3
           OR company_url IS NULL
           OR (company_url = $1 AND company_prefix = $2))
          AND (NOT $4 OR abi IS NULL OR abi = $5)
    "#)
    .bind(&company_url)
    .bind(&company_prefix)
    .bind(has_company_filter)
    .bind(has_abi_filter)
    .bind(&abi)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

    match releases {
        Ok(rows) => {
            let total_count = total.map(|t| t.0).unwrap_or(0);
            let total_pages = (total_count as f64 / page_size as f64).ceil() as i64;

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "data": rows,
                    "meta": {
                        "page": page,
                        "page_size": page_size,
                        "total": total_count,
                        "total_pages": total_pages
                    }
                })),
            ).into_response()
        }

        Err(e) => {
            tracing::error!("DB fetch failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            ).into_response()
        }
    }
}

pub async fn handle_version_post_control(
    State(db): State<Arc<PgPool>>,
    Json(payload): Json<ConfigPayload>,
) -> impl IntoResponse {
    let id = Uuid::new_v4();

    // Treat an empty-string company_url/abi the same as "not provided" so
    // the release is stored as global/universal rather than scoped to "".
    let company_url = payload.company_url.filter(|s| !s.trim().is_empty());
    let company_prefix = payload.company_prefix.filter(|s| !s.trim().is_empty());
    let abi = payload.abi.filter(|s| !s.trim().is_empty());

    let result = sqlx::query(r#"
        INSERT INTO releases (
            id,
            url,
            force_update,
            reason_for_update,
            update_display,
            memo,
            version_no,
            company_url,
            company_prefix,
            abi,
            created_at,
            updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW(), NOW())
    "#)
    .bind(&id)
    .bind(&payload.url)
    .bind(&payload.force_update)
    .bind(&payload.reason_for_update)
    .bind(&payload.update_display)
    .bind(&payload.memo)
    .bind(&payload.version_no)
    .bind(&company_url)
    .bind(&company_prefix)
    .bind(&abi)
    .execute(db.as_ref())
    .await;

    match result {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "message": "config created successfully",
                "id": id
            })),
        ).into_response(),

        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": if company_url.is_some() {
                    "This company already has a release with that version_no and ABI"
                } else {
                    "A global release with that version_no and ABI already exists"
                }
            })),
        ).into_response(),

        Err(e) => {
            tracing::error!("DB insert failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            ).into_response()
        }
    }
}

/// PATCH /configure/:id — edits an already-published release in place
/// (fix a typo'd reason, retarget a company, flip force_update, etc.)
/// without needing to delete-and-recreate, which would otherwise cost the
/// release its original `created_at` (breaking "Recent Releases" ordering)
/// and its id (breaking anything that already linked to it). Takes the same
/// shape as create — this is a full replace of the editable fields, not a
/// sparse patch.
pub async fn update_release(
    State(db): State<Arc<PgPool>>,
    Path(id): Path<Uuid>,
    Json(payload): Json<ConfigPayload>,
) -> impl IntoResponse {
    let company_url = payload.company_url.filter(|s| !s.trim().is_empty());
    let company_prefix = payload.company_prefix.filter(|s| !s.trim().is_empty());
    let abi = payload.abi.filter(|s| !s.trim().is_empty());

    let result = sqlx::query(r#"
        UPDATE releases SET
            url = $1,
            force_update = $2,
            reason_for_update = $3,
            update_display = $4,
            memo = $5,
            version_no = $6,
            company_url = $7,
            company_prefix = $8,
            abi = $9,
            updated_at = NOW()
        WHERE id = $10
    "#)
    .bind(&payload.url)
    .bind(&payload.force_update)
    .bind(&payload.reason_for_update)
    .bind(&payload.update_display)
    .bind(&payload.memo)
    .bind(&payload.version_no)
    .bind(&company_url)
    .bind(&company_prefix)
    .bind(&abi)
    .bind(id)
    .execute(db.as_ref())
    .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Release not found" })),
        ).into_response(),

        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": "Release updated", "id": id })),
        ).into_response(),

        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": if company_url.is_some() {
                    "This company already has a release with that version_no and ABI"
                } else {
                    "A global release with that version_no and ABI already exists"
                }
            })),
        ).into_response(),

        Err(e) => {
            tracing::error!("[CONFIGURE] update failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            ).into_response()
        }
    }
}

/// DELETE /configure/:id — removes the release ROW from Postgres only.
/// The APK file itself lives in Supabase storage and is deleted manually
/// (per how this project is operated) — this endpoint does not touch it.
pub async fn delete_release(
    State(db): State<Arc<PgPool>>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let result = sqlx::query("DELETE FROM releases WHERE id = $1")
        .bind(id)
        .execute(db.as_ref())
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Release not found" })),
        ).into_response(),
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": "Release deleted" })),
        ).into_response(),
        Err(e) => {
            tracing::error!("[CONFIGURE] delete failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            ).into_response()
        }
    }
}
