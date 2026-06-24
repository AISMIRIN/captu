use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::{routing::{get, post}, Router};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

use captu::{config::Config, db, ingest, scheduler};

mod routes;

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

    // Shared so a scheduled tick won't overlap the startup scan or another tick.
    let ingest_guard: scheduler::IngestGuard =
        std::sync::Arc::new(tokio::sync::Mutex::new(()));

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
    let _scheduler =
        scheduler::start(config.clone(), pool.clone(), ingest_guard).await?;

    let state = routes::AppState {
        pool,
        config,
        gen_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(routes::search::index))
        .route("/search", get(routes::search::search))
        .route("/contact/:id", get(routes::contact::contact))
        .route("/thumb/:id/:n", get(routes::capture::thumb))
        .route("/full/:id/:n", get(routes::capture::full))
        .route("/select/:id/:n", post(routes::capture::select_frame))
        .route("/api/episodes", get(routes::episodes::episodes))
        .route("/api/tags", get(routes::tags::tag_options))
        .route("/caption/:id/tags", post(routes::tags::add_tag))
        .route("/caption/:id/tags/delete", post(routes::tags::delete_tag))
        .route("/ingest/status", get(routes::ingest::status))
        .route("/ingest/files", get(routes::ingest::files))
        .route("/ingest/file/:id", get(routes::ingest::file_detail))
        .route("/ingest/clear/:id", post(routes::ingest::clear))
        .route("/reingest/:id", post(routes::ingest::reingest))
        .route("/recapture/:id", post(routes::capture::recapture))
        .nest_service("/static", ServeDir::new("ui/static"))
        .with_state(state);

    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
