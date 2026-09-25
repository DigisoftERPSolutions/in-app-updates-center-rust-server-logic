use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{types::Json as SqlxJson, PgPool, QueryBuilder};

use crate::authentication::guard::require_auth;

/// Accountability trail for the "Bulk Milk Purchase" browser form
/// (murima/old_pos). The browser fires a request here — in addition to,
/// never instead of, its existing call to the untouched PHP backend
/// (`insert_bulk.php`) — on every submission attempt, success or not. This
/// module is the whole reason that trail exists anywhere: the PHP side
/// keeps no record of who submitted from what machine, so this is it.

#[derive(Debug, Deserialize)]
pub struct BulkMilkRecordItem {
    pub supplier_id: String,
    pub quantity: f64,
}

#[derive(Debug, Deserialize)]
pub struct BulkMilkLogPayload {
    pub batch_id: String,

    pub company_url: Option<String>,
    pub company_prefix: Option<String>,
    pub site_name: Option<String>,

    pub user_id: String,
    pub username: Option<String>,
    pub real_name: Option<String>,
    pub email: Option<String>,

    pub route_id: Option<String>,
    pub route_name: Option<String>,
    pub store_code: Option<String>,
    pub store_name: Option<String>,
    pub shift: Option<String>,
    pub pricing_mode: Option<String>,
    pub reference_number: Option<String>,
    pub invoice_date: Option<String>,
    pub due_date: Option<String>,
    #[serde(default)]
    pub confirmed_resubmit: bool,
    pub records: Option<Vec<BulkMilkRecordItem>>,

    /// Mirrors the browser's `SubmitBatchResult` — "success" | "partial" | "failure".
    pub status: String,
    pub duplicate_count: Option<i32>,
    pub error_message: Option<String>,

    pub user_agent: Option<String>,
    pub browser_name: Option<String>,
    pub browser_version: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub platform: Option<String>,
    pub screen_width: Option<i32>,
    pub screen_height: Option<i32>,
    pub viewport_width: Option<i32>,
    pub viewport_height: Option<i32>,
    pub device_pixel_ratio: Option<f64>,
    pub color_depth: Option<i32>,
    pub timezone: Option<String>,
    pub timezone_offset_minutes: Option<i32>,
    pub language: Option<String>,
    pub hardware_concurrency: Option<i32>,
    pub device_memory_gb: Option<f64>,

    /// Best-effort last-known GPS fix — only sent when the browser has (and
    /// the user granted) geolocation. All three travel together or not at
    /// all, same contract as the mobile app's telemetry ping.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location_accuracy: Option<f64>,

    pub client_timestamp: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct BulkMilkLogRow {
    pub id: i64,
    pub batch_id: String,
    pub company_url: String,
    pub company_prefix: String,
    pub site_name: String,
    pub user_id: String,
    pub username: String,
    pub real_name: String,
    pub email: String,
    pub route_id: String,
    pub route_name: String,
    pub store_code: String,
    pub store_name: String,
    pub shift: String,
    pub pricing_mode: String,
    pub reference_number: String,
    pub invoice_date: Option<String>,
    pub due_date: Option<String>,
    pub confirmed_resubmit: bool,
    pub record_count: i32,
    pub total_quantity: f64,
    pub records: Option<SqlxJson<Vec<BulkMilkRecordRowItem>>>,
    pub status: String,
    pub duplicate_count: Option<i32>,
    pub error_message: Option<String>,
    pub ip_address: String,
    pub user_agent: String,
    pub browser_name: Option<String>,
    pub browser_version: Option<String>,
    pub os_name: Option<String>,
    pub os_version: Option<String>,
    pub platform: Option<String>,
    pub screen_width: Option<i32>,
    pub screen_height: Option<i32>,
    pub viewport_width: Option<i32>,
    pub viewport_height: Option<i32>,
    pub device_pixel_ratio: Option<f64>,
    pub color_depth: Option<i32>,
    pub timezone: Option<String>,
    pub timezone_offset_minutes: Option<i32>,
    pub language: Option<String>,
    pub hardware_concurrency: Option<i32>,
    pub device_memory_gb: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location_accuracy: Option<f64>,
    pub client_timestamp: Option<DateTime<Utc>>,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BulkMilkRecordRowItem {
    pub supplier_id: String,
    pub quantity: f64,
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

/// The browser calls `POST /` with no login — old_pos is a per-company
/// login against the PHP backend, not this Rust API, so there's no JWT to
/// send. Same public/unauthenticated trust model as `POST /telemetry`.
/// Everything that lets an admin browse/export the log stays behind
/// `require_auth`.
pub fn bulk_milk_log_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/", post(post_bulk_milk_log))
        .merge(
            Router::new()
                .route("/", get(get_bulk_milk_log))
                .route("/summary", get(get_bulk_milk_log_summary))
                .route("/export", get(export_bulk_milk_log))
                .route_layer(middleware::from_fn(require_auth)),
        )
        .with_state(db)
}

async fn post_bulk_milk_log(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(db): State<Arc<PgPool>>,
    Json(payload): Json<BulkMilkLogPayload>,
) -> impl IntoResponse {
    let ip = extract_ip(&headers, peer);

    let client_ts: Option<DateTime<Utc>> = payload
        .client_timestamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let (lat, lon, location_accuracy) = match (payload.lat, payload.lon) {
        (Some(lat), Some(lon)) => (Some(lat), Some(lon), payload.location_accuracy),
        _ => (None, None, None),
    };

    let record_count = payload.records.as_ref().map(|r| r.len() as i32).unwrap_or(0);
    let total_quantity: f64 = payload
        .records
        .as_ref()
        .map(|r| r.iter().map(|x| x.quantity).sum())
        .unwrap_or(0.0);
    let records_json = payload.records.as_ref().map(|r| {
        SqlxJson(
            r.iter()
                .map(|x| BulkMilkRecordRowItem {
                    supplier_id: x.supplier_id.clone(),
                    quantity: x.quantity,
                })
                .collect::<Vec<_>>(),
        )
    });

    let result = sqlx::query(
        r#"
        INSERT INTO bulk_milk_submission_log
            (batch_id, company_url, company_prefix, site_name,
             user_id, username, real_name, email,
             route_id, route_name, store_code, store_name, shift, pricing_mode,
             reference_number, invoice_date, due_date, confirmed_resubmit,
             record_count, total_quantity, records,
             status, duplicate_count, error_message,
             ip_address, user_agent, browser_name, browser_version,
             os_name, os_version, platform,
             screen_width, screen_height, viewport_width, viewport_height,
             device_pixel_ratio, color_depth, timezone, timezone_offset_minutes,
             language, hardware_concurrency, device_memory_gb,
             lat, lon, location_accuracy, client_timestamp)
        VALUES
            ($1,$2,$3,$4,
             $5,$6,$7,$8,
             $9,$10,$11,$12,$13,$14,
             $15,$16,$17,$18,
             $19,$20,$21,
             $22,$23,$24,
             $25,$26,$27,$28,
             $29,$30,$31,
             $32,$33,$34,$35,
             $36,$37,$38,$39,
             $40,$41,$42,
             $43,$44,$45,$46)
        "#,
    )
    .bind(&payload.batch_id)
    .bind(payload.company_url.unwrap_or_default())
    .bind(payload.company_prefix.unwrap_or_default())
    .bind(payload.site_name.unwrap_or_default())
    .bind(&payload.user_id)
    .bind(payload.username.unwrap_or_default())
    .bind(payload.real_name.unwrap_or_default())
    .bind(payload.email.unwrap_or_default())
    .bind(payload.route_id.unwrap_or_default())
    .bind(payload.route_name.unwrap_or_default())
    .bind(payload.store_code.unwrap_or_default())
    .bind(payload.store_name.unwrap_or_default())
    .bind(payload.shift.unwrap_or_default())
    .bind(payload.pricing_mode.unwrap_or_default())
    .bind(payload.reference_number.unwrap_or_default())
    .bind(payload.invoice_date)
    .bind(payload.due_date)
    .bind(payload.confirmed_resubmit)
    .bind(record_count)
    .bind(total_quantity)
    .bind(records_json)
    .bind(&payload.status)
    .bind(payload.duplicate_count)
    .bind(payload.error_message)
    .bind(&ip)
    .bind(payload.user_agent.unwrap_or_default())
    .bind(payload.browser_name)
    .bind(payload.browser_version)
    .bind(payload.os_name)
    .bind(payload.os_version)
    .bind(payload.platform)
    .bind(payload.screen_width)
    .bind(payload.screen_height)
    .bind(payload.viewport_width)
    .bind(payload.viewport_height)
    .bind(payload.device_pixel_ratio)
    .bind(payload.color_depth)
    .bind(payload.timezone)
    .bind(payload.timezone_offset_minutes)
    .bind(payload.language)
    .bind(payload.hardware_concurrency)
    .bind(payload.device_memory_gb)
    .bind(lat)
    .bind(lon)
    .bind(location_accuracy)
    .bind(client_ts)
    .execute(db.as_ref())
    .await;

    match result {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(e) => {
            tracing::error!("[BULK_MILK_LOG] DB error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BulkMilkLogQuery {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    pub company_prefix: Option<String>,
    pub company_url: Option<String>,
    pub user_id: Option<String>,
    pub status: Option<String>,
    pub batch_id: Option<String>,
    /// Free-text search across username/real name/email/route/store/reference.
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default = "default_sort")]
    pub sort: String,
}

fn default_page() -> i64 {
    1
}
fn default_page_size() -> i64 {
    50
}
fn default_sort() -> String {
    "desc".to_string()
}

fn parse_from_ts(s: &Option<String>) -> Option<DateTime<Utc>> {
    let v = s.as_deref()?.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
}

fn parse_to_ts(s: &Option<String>) -> Option<DateTime<Utc>> {
    let v = s.as_deref()?.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(23, 59, 59))
        .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
}

fn push_log_filters(qb: &mut QueryBuilder<'_, sqlx::Postgres>, p: &BulkMilkLogQuery) {
    qb.push(" WHERE 1=1 ");

    if let Some(cp) = p.company_prefix.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND company_prefix = ").push_bind(cp.to_string());
    }
    if let Some(cu) = p.company_url.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND company_url = ").push_bind(cu.to_string());
    }
    if let Some(u) = p.user_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND user_id = ").push_bind(u.to_string());
    }
    if let Some(s) = p.status.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND status = ").push_bind(s.to_string());
    }
    if let Some(b) = p.batch_id.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND batch_id = ").push_bind(b.to_string());
    }
    if let Some(from) = parse_from_ts(&p.from) {
        qb.push(" AND submitted_at >= ").push_bind(from);
    }
    if let Some(to) = parse_to_ts(&p.to) {
        qb.push(" AND submitted_at <= ").push_bind(to);
    }
    if let Some(q) = p.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let pat = format!("%{q}%");
        qb.push(" AND (username ILIKE ")
            .push_bind(pat.clone())
            .push(" OR real_name ILIKE ")
            .push_bind(pat.clone())
            .push(" OR email ILIKE ")
            .push_bind(pat.clone())
            .push(" OR user_id ILIKE ")
            .push_bind(pat.clone())
            .push(" OR route_name ILIKE ")
            .push_bind(pat.clone())
            .push(" OR store_name ILIKE ")
            .push_bind(pat.clone())
            .push(" OR reference_number ILIKE ")
            .push_bind(pat.clone())
            .push(" OR batch_id ILIKE ")
            .push_bind(pat.clone())
            .push(" OR site_name ILIKE ")
            .push_bind(pat)
            .push(")");
    }
}

const BULK_MILK_LOG_COLUMNS: &str = r#"id, batch_id, company_url, company_prefix, site_name,
        user_id, username, real_name, email,
        route_id, route_name, store_code, store_name, shift, pricing_mode,
        reference_number, invoice_date, due_date, confirmed_resubmit,
        record_count, total_quantity, records,
        status, duplicate_count, error_message,
        ip_address, user_agent, browser_name, browser_version,
        os_name, os_version, platform,
        screen_width, screen_height, viewport_width, viewport_height,
        device_pixel_ratio, color_depth, timezone, timezone_offset_minutes,
        language, hardware_concurrency, device_memory_gb,
        lat, lon, location_accuracy, client_timestamp, submitted_at"#;

/// GET /bulk-milk-log — paginated, filterable accountability log for the
/// admin "Bulk Milk Log" page.
pub async fn get_bulk_milk_log(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<BulkMilkLogQuery>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let page_size = params.page_size.clamp(1, 500);
    let offset = (page - 1) * page_size;
    let sort_dir = if params.sort.eq_ignore_ascii_case("asc") {
        "ASC"
    } else {
        "DESC"
    };

    let mut count_qb = QueryBuilder::new("SELECT COUNT(*) FROM bulk_milk_submission_log");
    push_log_filters(&mut count_qb, &params);
    let total: Result<(i64,), _> = count_qb.build_query_as().fetch_one(db.as_ref()).await;

    let mut data_qb = QueryBuilder::new(format!(
        "SELECT {BULK_MILK_LOG_COLUMNS} FROM bulk_milk_submission_log"
    ));
    push_log_filters(&mut data_qb, &params);
    data_qb.push(format!(" ORDER BY submitted_at {sort_dir} LIMIT "));
    data_qb.push_bind(page_size);
    data_qb.push(" OFFSET ");
    data_qb.push_bind(offset);

    let rows = data_qb
        .build_query_as::<BulkMilkLogRow>()
        .fetch_all(db.as_ref())
        .await;

    match (rows, total) {
        (Ok(data), Ok((total_count,))) => {
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
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("[BULK_MILK_LOG] fetch error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

#[derive(Serialize, sqlx::FromRow)]
pub struct BulkMilkLogTotals {
    pub total_submissions: i64,
    pub success_count: i64,
    pub partial_count: i64,
    pub failure_count: i64,
    pub distinct_users: i64,
    pub distinct_companies: i64,
    pub total_records_inserted: i64,
    pub total_quantity: f64,
    pub submissions_24h: i64,
}

/// GET /bulk-milk-log/summary — aggregate KPIs for the admin dashboard's
/// stat strip. Pre-aggregated in SQL for the same reason
/// `get_telemetry_summary` is: this table only grows.
pub async fn get_bulk_milk_log_summary(State(db): State<Arc<PgPool>>) -> impl IntoResponse {
    let totals = sqlx::query_as::<_, BulkMilkLogTotals>(
        r#"
        SELECT
            COUNT(*)                                                          AS total_submissions,
            COUNT(*) FILTER (WHERE status = 'success')                        AS success_count,
            COUNT(*) FILTER (WHERE status = 'partial')                        AS partial_count,
            COUNT(*) FILTER (WHERE status = 'failure')                        AS failure_count,
            COUNT(DISTINCT user_id)                                           AS distinct_users,
            COUNT(DISTINCT company_url)                                       AS distinct_companies,
            COALESCE(SUM(record_count) FILTER (WHERE status IN ('success', 'partial')), 0) AS total_records_inserted,
            COALESCE(SUM(total_quantity) FILTER (WHERE status IN ('success', 'partial')), 0) AS total_quantity,
            COUNT(*) FILTER (WHERE submitted_at > NOW() - INTERVAL '24 hours') AS submissions_24h
        FROM bulk_milk_submission_log
        "#,
    )
    .fetch_one(db.as_ref())
    .await;

    match totals {
        Ok(totals) => (StatusCode::OK, Json(serde_json::json!({ "totals": totals }))).into_response(),
        Err(e) => {
            tracing::error!("[BULK_MILK_LOG] summary query failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

const EXPORT_ROW_CAP: i64 = 500_000;

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn push_csv_row(out: &mut String, r: &BulkMilkLogRow) {
    let records_summary = r
        .records
        .as_ref()
        .map(|SqlxJson(items)| {
            items
                .iter()
                .map(|i| format!("{}:{}", i.supplier_id, i.quantity))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();

    let cols = [
        r.id.to_string(),
        r.submitted_at.to_rfc3339(),
        r.status.clone(),
        r.batch_id.clone(),
        r.site_name.clone(),
        r.company_url.clone(),
        r.company_prefix.clone(),
        r.username.clone(),
        r.real_name.clone(),
        r.email.clone(),
        r.user_id.clone(),
        r.route_name.clone(),
        r.store_name.clone(),
        r.shift.clone(),
        r.reference_number.clone(),
        r.invoice_date.clone().unwrap_or_default(),
        r.record_count.to_string(),
        r.total_quantity.to_string(),
        r.duplicate_count.map(|v| v.to_string()).unwrap_or_default(),
        r.error_message.clone().unwrap_or_default(),
        r.ip_address.clone(),
        r.browser_name.clone().unwrap_or_default(),
        r.browser_version.clone().unwrap_or_default(),
        r.os_name.clone().unwrap_or_default(),
        r.os_version.clone().unwrap_or_default(),
        r.platform.clone().unwrap_or_default(),
        r.screen_width.map(|v| v.to_string()).unwrap_or_default(),
        r.screen_height.map(|v| v.to_string()).unwrap_or_default(),
        r.timezone.clone().unwrap_or_default(),
        r.language.clone().unwrap_or_default(),
        r.hardware_concurrency.map(|v| v.to_string()).unwrap_or_default(),
        r.device_memory_gb.map(|v| v.to_string()).unwrap_or_default(),
        r.lat.map(|v| v.to_string()).unwrap_or_default(),
        r.lon.map(|v| v.to_string()).unwrap_or_default(),
        r.location_accuracy.map(|v| v.to_string()).unwrap_or_default(),
        r.user_agent.clone(),
        records_summary,
    ];
    out.push_str(&cols.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(","));
    out.push('\n');
}

/// GET /bulk-milk-log/export — every submission attempt matching the
/// current filters, as a downloadable CSV.
pub async fn export_bulk_milk_log(
    State(db): State<Arc<PgPool>>,
    Query(params): Query<BulkMilkLogQuery>,
) -> impl IntoResponse {
    let sort_dir = if params.sort.eq_ignore_ascii_case("asc") {
        "ASC"
    } else {
        "DESC"
    };

    let mut qb = QueryBuilder::new(format!(
        "SELECT {BULK_MILK_LOG_COLUMNS} FROM bulk_milk_submission_log"
    ));
    push_log_filters(&mut qb, &params);
    qb.push(format!(" ORDER BY submitted_at {sort_dir} LIMIT "));
    qb.push_bind(EXPORT_ROW_CAP);

    let rows = qb
        .build_query_as::<BulkMilkLogRow>()
        .fetch_all(db.as_ref())
        .await;

    match rows {
        Ok(data) => {
            let mut csv = String::with_capacity(data.len() * 220 + 256);
            csv.push_str(
                "id,submitted_at,status,batch_id,site_name,company_url,company_prefix,\
                 username,real_name,email,user_id,route_name,store_name,shift,\
                 reference_number,invoice_date,record_count,total_quantity,duplicate_count,\
                 error_message,ip_address,browser_name,browser_version,os_name,os_version,\
                 platform,screen_width,screen_height,timezone,language,hardware_concurrency,\
                 device_memory_gb,lat,lon,location_accuracy,user_agent,records\n",
            );
            for r in &data {
                push_csv_row(&mut csv, r);
            }

            let filename = format!(
                "bulk-milk-log-export-{}.csv",
                Utc::now().format("%Y%m%d-%H%M%S")
            );

            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "text/csv; charset=utf-8".parse().unwrap(),
            );
            headers.insert(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\"").parse().unwrap(),
            );

            (StatusCode::OK, headers, csv).into_response()
        }
        Err(e) => {
            tracing::error!("[BULK_MILK_LOG] export failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}
