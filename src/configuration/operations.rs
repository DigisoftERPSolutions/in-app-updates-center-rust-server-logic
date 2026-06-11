use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

pub fn version_control(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(handle_version_post_control).get(get_version_uploads))
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
}

#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
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
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn get_version_uploads(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<PaginationParams>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let page_size = params.page_size.clamp(1, 100);
    let offset = (page - 1) * page_size;

    let releases = sqlx::query_as::<_, ReleaseRow>(r#"
        SELECT
            id,
            url,
            version_no,
            force_update,
            reason_for_update,
            update_display,
            memo,
            created_at
        FROM releases
        ORDER BY created_at DESC
        LIMIT $1 OFFSET $2
    "#)
    .bind(page_size)
    .bind(offset)
    .fetch_all(db.as_ref())
    .await;

    let total: Option<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM releases")
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

    let result = sqlx::query(r#"
        INSERT INTO releases (
            id,
            url,
            force_update,
            reason_for_update,
            update_display,
            memo,
            version_no,
            created_at,
            updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
    "#)
    .bind(&id)
    .bind(&payload.url)
    .bind(&payload.force_update)
    .bind(&payload.reason_for_update)
    .bind(&payload.update_display)
    .bind(&payload.memo)
    .bind(&payload.version_no)
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

        Err(e) => {
            tracing::error!("DB insert failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            ).into_response()
        }
    }
}