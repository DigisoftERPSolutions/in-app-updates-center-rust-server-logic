#![recursion_limit = "512"]

mod authentication;
mod companies;
mod configuration;
mod telemetry;

use axum::{http::Method, Router};
use dotenvy::dotenv;
use sqlx::postgres::PgPoolOptions;
use std::{env, net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;

use crate::{
    authentication::{login::login_route, signup::signup_route},
    companies::operations::companies_route,
    configuration::operations::version_control,
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
    let db = Arc::new(
        PgPoolOptions::new()
            .max_connections(10)
            .connect(&database_url)
            .await?,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::PATCH])
        .allow_headers(Any);

    let app = Router::new()
        .nest(&format!("{base_path}/signup"), signup_route(db.clone()))
        .nest(&format!("{base_path}/login"), login_route(db.clone()))
        .nest(&format!("{base_path}/configure"), version_control(db.clone()))
        .nest(&format!("{base_path}/companies"), companies_route(db.clone()))
        .nest(&format!("{base_path}/telemetry"), telemetry_route(db.clone()))
        .layer(cors)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    let listener = TcpListener::bind(addr).await?;

    tracing::info!("Server listening on {addr}");
    println!("Server listening on {}",addr);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown())
        .await?;

    Ok(())
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