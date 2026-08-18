#![recursion_limit = "512"]

mod authentication;
mod companies;
mod configuration;
mod meter_ocr;
mod telemetry;

use axum::{http::{Method, StatusCode}, middleware, response::IntoResponse, Json, Router};
use dotenvy::dotenv;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{any::Any as PanicPayload, env, net::SocketAddr, str::FromStr, sync::Arc};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;

use crate::{
    authentication::{guard::require_auth, login::login_route, signup::signup_route},
    companies::operations::companies_route,
    configuration::operations::version_control,
    meter_ocr::operations::meter_ocr_route,
    telemetry::pings::telemetry_route,
};




#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    init_tracing();

    tracing::info!("Starting Backup service");

    let database_url = env::var("DATABASE_URL")?;
    let base_path    = env::var("BASE_URL")?; 
    let signup_path = format!("{base_path}/signup");  
println!("Signup mounted at: {}", &signup_path);
    // `DATABASE_URL` points at Neon's pooled endpoint (PgBouncer, transaction
    // mode) rather than a direct Postgres connection. sqlx defaults to
    // caching server-side prepared statements per-connection, but under
    // transaction pooling the physical backend connection can change between
    // statements — so a cached "prepared" statement from one backend gets
    // replayed against another that never prepared it, and Postgres rejects
    // it. That surfaces here as a generic query failure ("Database error")
    // that gets worse under real concurrent load and is very hard to
    // reproduce with a single quick request. Disabling the statement cache
    // makes sqlx always send unprepared (simple-protocol) queries, which is
    // safe for PgBouncer transaction pooling and is Neon's own recommended
    // setting for sqlx. See https://neon.tech/docs/guides/sqlx
    let connect_options = PgConnectOptions::from_str(&database_url)?.statement_cache_capacity(0);
    let db = Arc::new(
        PgPoolOptions::new()
            .max_connections(10)
            .connect_with(connect_options)
            .await?,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::PATCH])
        .allow_headers(Any);

    // The sites directory is an admin-only surface — guard every method
    // behind a valid session. `/configure` and `/telemetry` are each split
    // internally by method (see `version_control` / `telemetry_route`):
    // the mobile app calls `GET /configure` (update check) and
    // `POST /telemetry` (device pings) with no login at all, and both are
    // already live on real devices, so those two stay public while every
    // admin mutation/view on the same routers stays behind `require_auth`.
    let app = Router::new()
        .nest(&format!("{base_path}/signup"), signup_route(db.clone()))
        .nest(&format!("{base_path}/login"), login_route(db.clone()))
        .nest(&format!("{base_path}/configure"), version_control(db.clone()))
        .nest(
            &format!("{base_path}/companies"),
            companies_route(db.clone()).route_layer(middleware::from_fn(require_auth)),
        )
        .nest(&format!("{base_path}/telemetry"), telemetry_route(db.clone()))
        // Public/no-login, same trust model as /configure and /telemetry above
        // (see meter_ocr::operations::meter_ocr_route's doc comment for why
        // this one still isn't a free-for-all despite that).
        .nest(&format!("{base_path}/meter-ocr"), meter_ocr_route(db.clone()))
        .layer(cors)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // A panicking handler used to kill the connection outright — the
        // client sees a bare network error with no status code or body,
        // which is exactly what turns into an undiagnosable "Could not
        // load fleet summary" on the dashboard with nothing to go on. This
        // converts any panic into a normal logged 500 JSON response instead.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(handle_panic));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    let listener = TcpListener::bind(addr).await?;

    tracing::info!("Server listening on {addr}");
    println!("Server listening on {}",addr);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown())
        .await?;

    Ok(())
}

fn handle_panic(err: Box<dyn PanicPayload + Send + 'static>) -> axum::response::Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "unknown panic".to_string()
    };
    tracing::error!("[PANIC] handler panicked: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "Internal server error" })),
    )
        .into_response()
}

async fn shutdown() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    println!("Signal received, shutting down gracefully");
}

fn init_tracing() {
    let file_appender = rolling::daily("logs", "app.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    std::mem::forget(guard);
}