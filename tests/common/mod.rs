// Shared test utilities for HTTP route integration tests.
//
// Each test creates an isolated in-memory DB, builds the router through
// `build_router`, and sends requests via `tower::ServiceExt::oneshot`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{body::Body, http::Request, Router};
use captu::{
    config::{CaptureConfig, Config, IngestConfig, PathsConfig, ServerConfig},
    db::init_db,
    routes::{self, AppState},
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

// ── App builder ───────────────────────────────────────────────────────────────

/// Build a test router backed by a fresh isolated SQLite DB.
/// Returns `(router, tmp_dir)` — caller must keep `tmp_dir` alive.
pub async fn make_app() -> (Router, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let pool = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("init_db failed");

    let config = Arc::new(Config {
        paths: PathsConfig {
            nas_mount: dir.path().to_string_lossy().to_string(),
            ts_glob: "**/*.ts".to_string(),
            cache_dir: dir.path().join("cache").to_string_lossy().to_string(),
            db_path: db_path.to_string_lossy().to_string(),
        },
        capture: CaptureConfig {
            thumb_count: 6,
            thumb_width: 640,
            thumb_height: 360,
            thumb_quality: 4,
            width: 1920,
            height: 1080,
            jpeg_quality: 2,
        },
        ingest: IngestConfig {
            schedule_cron: String::new(),
            run_on_startup: false,
            concurrency: 1,
            require_captions: false,
            filter_include: vec![],
            filter_exclude: vec![],
        },
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8000,
        },
    });

    let state = AppState {
        pool,
        config,
        gen_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    let router = routes::build_router(state);
    (router, dir)
}

// ── Seed helpers ──────────────────────────────────────────────────────────────

/// Seed a minimal ts_file + program + caption and return (ts_file_id, caption_id).
pub async fn seed_caption(router: &Router) -> (i64, i64) {
    // Extract the pool via a helper request — actually we can't do that here.
    // Instead, callers that need DB seeding use `make_app_with_state` below.
    let _ = router; // suppress warning
    panic!("use make_app_seeded() instead");
}

/// State + pool exposed for seeding without going through HTTP.
pub struct TestApp {
    pub router: Router,
    pub state: AppState,
    /// Kept alive to prevent temp dir deletion during the test.
    pub _dir: TempDir,
}

/// Build a test router and expose the raw `AppState` so tests can seed directly.
pub async fn make_app_seeded() -> TestApp {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let pool = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("init_db failed");

    let config = Arc::new(Config {
        paths: PathsConfig {
            nas_mount: dir.path().to_string_lossy().to_string(),
            ts_glob: "**/*.ts".to_string(),
            cache_dir: dir.path().join("cache").to_string_lossy().to_string(),
            db_path: db_path.to_string_lossy().to_string(),
        },
        capture: CaptureConfig {
            thumb_count: 6,
            thumb_width: 640,
            thumb_height: 360,
            thumb_quality: 4,
            width: 1920,
            height: 1080,
            jpeg_quality: 2,
        },
        ingest: IngestConfig {
            schedule_cron: String::new(),
            run_on_startup: false,
            concurrency: 1,
            require_captions: false,
            filter_include: vec![],
            filter_exclude: vec![],
        },
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8000,
        },
    });

    let state = AppState {
        pool,
        config,
        gen_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    let router = routes::build_router(state.clone());
    TestApp {
        router,
        state,
        _dir: dir,
    }
}

// ── Request helpers ───────────────────────────────────────────────────────────

/// GET request with empty body.
pub fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// POST request with URL-encoded form body.
pub fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Collect the response body as a UTF-8 string.
pub async fn body_string(body: axum::body::Body) -> String {
    let bytes = body.collect().await.expect("body").to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Convenience: send a request and return `(status_code, body_string)`.
pub async fn oneshot(router: Router, req: Request<Body>) -> (u16, String) {
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status().as_u16();
    let body = body_string(resp.into_body()).await;
    (status, body)
}
