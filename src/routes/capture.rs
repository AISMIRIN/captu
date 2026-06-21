use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
};
use sqlx::Row;
use tokio::sync::Mutex as AsyncMutex;

use captu::media::capture;

use super::AppState;

/// GET /thumb/:id/:n  — serve a contact-sheet thumbnail JPEG.
///
/// Acquires a per-caption async lock before calling ensure_thumbnails so that
/// concurrent requests for the same caption (e.g. the 6-frame grid) do not
/// launch parallel ffmpeg pipelines.  The first request runs generation;
/// subsequent requests find the files already cached and return immediately.
///
/// On successful generation, records the caption in `thumbnails` with the
/// default selected_frame (middle frame).  OR IGNORE means an existing
/// user selection is never overwritten.
pub async fn thumb(
    State(state): State<AppState>,
    Path((id, n)): Path<(i64, u32)>,
) -> Result<impl IntoResponse, StatusCode> {
    let (ts_path, pts_start, pts_end) = lookup_caption(&state, id).await?;

    // Acquire (or create) the per-caption generation lock.
    let lock: Arc<AsyncMutex<()>> = {
        let mut map = state.gen_locks.lock().unwrap();
        map.entry(id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let cfg = state.config.clone();
    let ts_path_cl = ts_path.clone();
    tokio::task::spawn_blocking(move || {
        capture::ensure_thumbnails(&cfg, &ts_path_cl, id, pts_start, pts_end)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!("thumb gen failed {}/{}: {:#}", id, n, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Record successful generation in thumbnails (default = middle frame).
    // OR IGNORE preserves any existing user-selected frame.
    let default_frame = state.config.capture.thumb_count as i64 / 2;
    sqlx::query(
        "INSERT OR IGNORE INTO thumbnails(caption_id, selected_frame) VALUES (?, ?)",
    )
    .bind(id)
    .bind(default_frame)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("thumbnails insert failed for {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path =
        capture::thumb_path(std::path::Path::new(&state.config.paths.cache_dir), &stem, id, n);

    serve_jpeg(path).await
}

/// POST /select/:id/:n  — persist the user's chosen frame for a caption.
///
/// Upserts into thumbnails so the selection survives page reloads and appears
/// as the preview image in search results.
pub async fn select_frame(
    State(state): State<AppState>,
    Path((id, n)): Path<(i64, u32)>,
) -> StatusCode {
    match sqlx::query(
        "INSERT INTO thumbnails(caption_id, selected_frame) VALUES (?, ?)
         ON CONFLICT(caption_id) DO UPDATE SET selected_frame = excluded.selected_frame",
    )
    .bind(id)
    .bind(n as i64)
    .execute(&state.pool)
    .await
    {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::error!("select_frame failed {}/{}: {:#}", id, n, e);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ------ helpers ------

async fn lookup_caption(
    state: &AppState,
    id: i64,
) -> Result<(PathBuf, i64, i64), StatusCode> {
    let row = sqlx::query(
        "SELECT f.path, c.pts_start, c.pts_end \
         FROM captions c \
         JOIN ts_files f ON c.ts_file_id = f.id \
         WHERE c.id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db lookup failed for caption {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    let path: String = row.get("path");
    let pts_start: i64 = row.get("pts_start");
    let pts_end: i64 = row.get("pts_end");
    Ok((PathBuf::from(path), pts_start, pts_end))
}

async fn serve_jpeg(path: PathBuf) -> Result<impl IntoResponse, StatusCode> {
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        tracing::error!("failed to read JPEG at {}: {}", path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((
        [(header::CONTENT_TYPE, "image/jpeg")],
        Bytes::from(bytes),
    ))
}
