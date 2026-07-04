// Enable #[coverage(off)] when instrumented by cargo-llvm-cov (nightly only).
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

use captu::{config::Config, db, ingest, routes, scheduler};

// Server bootstrap: requires a live DB, scheduler, and network listener.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = if Path::new("config.toml").exists() {
        Config::load(Path::new("config.toml"))?
    } else {
        Config::default_dev()
    };
    let config = Arc::new(config);

    let addr = format!("{}:{}", config.server.host, config.server.port);

    // Pool size: concurrency workers + headroom for web handlers.
    let pool = db::init_db(&config.paths.db_path, config.ingest.concurrency + 5).await?;

    // Shared so a scheduled tick, the startup scan, and a manual scan
    // (POST /ingest/scan) never overlap each other.
    let ingest_guard: scheduler::IngestGuard = scheduler::new_guard();

    if config.ingest.run_on_startup {
        let cfg = config.clone();
        let pool_clone = pool.clone();
        let guard = ingest_guard.clone();
        tokio::spawn(async move {
            let _lock = guard.lock().await;
            tracing::info!("startup ingest: beginning scan");
            if let Err(e) = ingest::scan_and_ingest(cfg, pool_clone).await {
                tracing::error!("startup ingest failed: {:#}", e);
            }
        });
    }

    // Keep the scheduler alive for the whole process lifetime (drop = stop).
    let _scheduler = scheduler::start(config.clone(), pool.clone(), ingest_guard.clone()).await?;

    let state = routes::AppState {
        pool,
        config,
        gen_locks: Arc::new(Mutex::new(HashMap::new())),
        ingest_guard,
    };

    let app = routes::build_router(state).nest_service("/static", ServeDir::new("ui/static"));

    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
