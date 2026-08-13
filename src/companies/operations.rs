use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Company / site directory. Lives in our own Postgres DB — seeded from the
/// `0_companies` production export, but not a mirror of it: writes here
/// (add/update/delete a site) only affect this directory, which exists to
/// let the admin dashboard scope releases per company. It never touches the
/// live digerp.com MySQL database.
pub fn companies_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", get(list_companies).post(create_company))
        .route("/{id}", delete(delete_company).patch(update_company))
        .with_state(db)
}

#[derive(Deserialize)]
pub struct CompanyPayload {
    pub site_name: String,
    pub site_description: Option<String>,
    pub branch_: String,
    #[serde(default = "default_prefix")]
    pub company_prefix: String,
    pub company_url: String,
    #[serde(default)]
    pub site_key: String,
}

fn default_prefix() -> String {
    "0_".to_string()
}

#[derive(Deserialize)]
pub struct CompanyUpdatePayload {
    pub site_name: Option<String>,
    pub site_description: Option<String>,
    pub branch_: Option<String>,
    pub company_prefix: Option<String>,
    pub company_url: Option<String>,
    pub site_key: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct CompanyRow {
    pub id: i64,
    pub site_name: String,
    pub site_description: Option<String>,
    pub branch_: String,
    pub company_prefix: String,
    pub company_url: String,
    pub site_key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    pub search: Option<String>,
}

fn default_page() -> i64 { 1 }
fn default_page_size() -> i64 { 50 }

/// GET /companies — list every site, paginated, optionally filtered by
/// site_name/branch_/company_url (case-insensitive substring match).
pub async fn list_companies(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let page_size = params.page_size.clamp(1, 200);
    let offset = (page - 1) * page_size;
    let search = params.search.unwrap_or_default();
    let like = format!("%{}%", search.trim());

    let rows = sqlx::query_as::<_, CompanyRow>(
        r#"
        SELECT id, site_name, site_description, branch_, company_prefix,
               company_url, site_key, created_at, updated_at
        FROM companies
        WHERE $1 = '' OR site_name ILIKE $1 OR branch_ ILIKE $1 OR company_url ILIKE $1
        ORDER BY site_name ASC, branch_ ASC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(&like)
    .bind(page_size)
    .bind(offset)
    .fetch_all(db.as_ref())
    .await;

    let total: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM companies WHERE $1 = '' OR site_name ILIKE $1 OR branch_ ILIKE $1 OR company_url ILIKE $1",
    )
    .bind(&like)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

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
            tracing::error!("[COMPANIES] list failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

/// POST /companies — add a new site.
pub async fn create_company(
    State(db): State<Arc<PgPool>>,
    Json(payload): Json<CompanyPayload>,
) -> impl IntoResponse {
    if payload.site_name.trim().is_empty()
        || payload.branch_.trim().is_empty()
        || payload.company_url.trim().is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "site_name, branch_ and company_url are required" })),
        )
            .into_response();
    }

    let result = sqlx::query_as::<_, CompanyRow>(
        r#"
        INSERT INTO companies (site_name, site_description, branch_, company_prefix, company_url, site_key, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, NOW())
        RETURNING id, site_name, site_description, branch_, company_prefix, company_url, site_key, created_at, updated_at
        "#,
    )
    .bind(&payload.site_name)
    .bind(&payload.site_description)
    .bind(&payload.branch_)
    .bind(&payload.company_prefix)
    .bind(&payload.company_url)
    .bind(&payload.site_key)
    .fetch_one(db.as_ref())
    .await;

    match result {
        Ok(row) => (StatusCode::CREATED, Json(serde_json::json!(row))).into_response(),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "A site with this company_url and company_prefix already exists" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("[COMPANIES] create failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

/// PATCH /companies/:id — edit a site. Only the fields present in the body are changed.
pub async fn update_company(
    State(db): State<Arc<PgPool>>,
    Path(id): Path<i64>,
    Json(payload): Json<CompanyUpdatePayload>,
) -> impl IntoResponse {
    let result = sqlx::query_as::<_, CompanyRow>(
        r#"
        UPDATE companies SET
            site_name = COALESCE($2, site_name),
            site_description = COALESCE($3, site_description),
            branch_ = COALESCE($4, branch_),
            company_prefix = COALESCE($5, company_prefix),
            company_url = COALESCE($6, company_url),
            site_key = COALESCE($7, site_key),
            updated_at = NOW()
        WHERE id = $1
        RETURNING id, site_name, site_description, branch_, company_prefix, company_url, site_key, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(&payload.site_name)
    .bind(&payload.site_description)
    .bind(&payload.branch_)
    .bind(&payload.company_prefix)
    .bind(&payload.company_url)
    .bind(&payload.site_key)
    .fetch_optional(db.as_ref())
    .await;

    match result {
        Ok(Some(row)) => (StatusCode::OK, Json(serde_json::json!(row))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Site not found" })),
        )
            .into_response(),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "A site with this company_url and company_prefix already exists" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("[COMPANIES] update failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

/// DELETE /companies/:id — remove a site from this directory.
/// Does not touch any releases already scoped to it (they simply become
/// "orphaned" scoped releases — still delivered by company_url/company_prefix
/// match, just no longer visible as a company in the picker).
pub async fn delete_company(
    State(db): State<Arc<PgPool>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let result = sqlx::query("DELETE FROM companies WHERE id = $1")
        .bind(id)
        .execute(db.as_ref())
        .await;

    match result {
        Ok(r) if r.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Site not found" })),
        )
            .into_response(),
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": "Site deleted" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("[COMPANIES] delete failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}
