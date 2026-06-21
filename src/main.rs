use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::{routing::{get, post}, Router};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

use captu::{config::Config, db, ingest};

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

    if config.ingest.run_on_startup {
        let cfg = config.clone();
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            tracing::info!("startup ingest: beginning scan");
            if let Err(e) = ingest::scan_and_ingest(cfg, pool_clone).await {
                tracing::error!("startup ingest failed: {:#}", e);
            }
        });
    }

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
        .route("/select/:id/:n", post(routes::capture::select_frame))
        .route("/api/episodes", get(routes::episodes::episodes))
        .route("/ingest/status", get(routes::ingest::status))
        .route("/reingest/:id", post(routes::ingest::reingest))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state);

    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
