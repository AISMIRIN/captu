use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sqlx::Row;
use std::path::PathBuf;

use captu::ingest;

use super::AppState;

pub struct ErrorEntry {
    pub filename: String,
    pub error_msg: String,
}

#[derive(Template)]
#[template(path = "pages/ingest_status.html")]
pub struct IngestStatusTemplate {
    pub pending: i64,
    pub ingesting: i64,
    pub done: i64,
    pub error: i64,
    pub total: i64,
    pub ingesting_files: Vec<String>,
    pub recent_errors: Vec<ErrorEntry>,
}

pub async fn status(State(state): State<AppState>) -> Result<IngestStatusTemplate, StatusCode> {
    // Aggregate status counts.
    let count_rows = sqlx::query("SELECT status, COUNT(*) as cnt FROM ts_files GROUP BY status")
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("/ingest/status db error: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut pending = 0i64;
    let mut ingesting = 0i64;
    let mut done = 0i64;
    let mut error = 0i64;

    for row in &count_rows {
        let s: String = row.get("status");
        let c: i64 = row.get("cnt");
        match s.as_str() {
            "pending" => pending = c,
            "ingesting" => ingesting = c,
            "done" => done = c,
            "error" => error = c,
            _ => {}
        }
    }
    let total = pending + ingesting + done + error;

    // List files currently being ingested.
    let ing_rows = sqlx::query("SELECT filename FROM ts_files WHERE status = 'ingesting'")
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("/ingest/status (ingesting list) db error: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let ingesting_files: Vec<String> = ing_rows.iter().map(|r| r.get("filename")).collect();

    // Most recent errors (up to 5).
    let err_rows = sqlx::query(
        "SELECT filename, COALESCE(error_msg, '(unknown)') AS error_msg
         FROM ts_files
         WHERE status = 'error'
         ORDER BY ingested_at DESC
         LIMIT 5",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("/ingest/status (error list) db error: {:#}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let recent_errors = err_rows
        .iter()
        .map(|r| ErrorEntry {
            filename: r.get("filename"),
            error_msg: r.get("error_msg"),
        })
        .collect();

    Ok(IngestStatusTemplate {
        pending,
        ingesting,
        done,
        error,
        total,
        ingesting_files,
        recent_errors,
    })
}

/// Reset one TS file and kick off background workers to reprocess it.
pub async fn reingest(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let cache_dir = PathBuf::from(&state.config.paths.cache_dir);

    match ingest::reset_ts_file(&state.pool, id, &cache_dir).await {
        Ok(()) => {
            // Spawn workers in the background. If existing workers are still running
            // they will claim the newly-reset file; if not, the new workers handle it.
            let cfg = state.config.clone();
            let pool = state.pool.clone();
            tokio::spawn(async move {
                if let Err(e) = ingest::run_workers(cfg, pool).await {
                    tracing::error!("reingest background worker error: {:#}", e);
                }
            });

            (StatusCode::OK, "再取り込みを開始しました").into_response()
        }
        Err(e) => {
            tracing::error!("reingest reset error for id={}: {:#}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, "エラーが発生しました").into_response()
        }
    }
}
