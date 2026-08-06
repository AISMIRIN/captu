// Route handlers: GET /thumb/{id}/{n}, GET /full/{id}/{n}, GET /preview/{id},
//                 GET /sub/{id}, POST /select/{id}/{n}, POST /recapture/{id}
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::media::capture::{self, SubMode};
use crate::ts::subtitle;

use super::AppState;

/// Query string for `GET /full/{id}/{n}`.
///
/// `sub=0` asks for the raw frame so the client can composite `/sub/{id}`
/// itself; any other value (including an absent parameter) keeps the historical
/// behaviour of burning the subtitles in with ffmpeg.
#[derive(Debug, Deserialize)]
pub struct FullQuery {
    sub: Option<u8>,
}

impl FullQuery {
    fn sub_mode(&self) -> SubMode {
        match self.sub {
            Some(0) => SubMode::Raw,
            _ => SubMode::Burned,
        }
    }
}

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
// axum handler; drives ensure_thumbnails → real ffmpeg on a live TS file + DB write.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
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
    sqlx::query!(
        "INSERT OR IGNORE INTO thumbnails(caption_id, selected_frame) VALUES (?, ?)",
        id,
        default_frame,
    )
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
    let path = capture::thumb_path(
        std::path::Path::new(&state.config.paths.cache_dir),
        &stem,
        id,
        n,
    );

    serve_image(path, "image/jpeg").await
}

/// GET /preview/:id  — serve a subtitle-free single-frame preview JPEG.
///
/// Generated on first access; subsequent requests return the cached file.
/// Used by search results before full contact-sheet thumbnails are available.
/// Does not write to the `thumbnails` table — subtitle-composited thumbnail
/// generation (via /thumb) remains a separate step triggered by the contact sheet.
// axum handler; calls ensure_preview → real ffmpeg on a live TS file.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn preview(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let (ts_path, pts_start, pts_end) = lookup_caption(&state, id).await?;

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
        capture::ensure_preview(&cfg, &ts_path_cl, id, pts_start, pts_end)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!("preview gen failed {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path = capture::preview_path(
        std::path::Path::new(&state.config.paths.cache_dir),
        &stem,
        id,
    );

    serve_image(path, "image/jpeg").await
}

/// POST /select/:id/:n  — persist the user's chosen frame for a caption.
///
/// Upserts into thumbnails so the selection survives page reloads and appears
/// as the preview image in search results.
pub async fn select_frame(
    State(state): State<AppState>,
    Path((id, n)): Path<(i64, u32)>,
) -> StatusCode {
    let frame = n as i64;
    match sqlx::query!(
        "INSERT INTO thumbnails(caption_id, selected_frame) VALUES (?, ?)
         ON CONFLICT(caption_id) DO UPDATE SET selected_frame = excluded.selected_frame",
        id,
        frame,
    )
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

/// GET /full/:id/:n  — serve a full-resolution (download) JPEG for a single frame.
///
/// Generates the frame on first access using the full `cfg.width × cfg.height`
/// resolution and `cfg.jpeg_quality`.  Subsequent requests return the cached file.
/// Uses the same per-caption lock as `thumb` to avoid duplicate ffmpeg runs.
///
/// `?sub=0` returns the frame without the subtitle overlay, cached separately as
/// `full/{id}_{n:02}_nosub.jpg`.  The contact sheet uses this variant and draws
/// `/sub/{id}` on top in a canvas so the subtitle can be toggled without
/// another ffmpeg run.
// axum handler; calls ensure_full → real ffmpeg on a live TS file.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn full(
    State(state): State<AppState>,
    Path((id, n)): Path<(i64, u32)>,
    Query(q): Query<FullQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let (ts_path, pts_start, pts_end) = lookup_caption(&state, id).await?;
    let mode = q.sub_mode();

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
        capture::ensure_full(&cfg, &ts_path_cl, id, pts_start, pts_end, n, mode)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!("full gen failed {}/{}: {:#}", id, n, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path = capture::full_path(
        std::path::Path::new(&state.config.paths.cache_dir),
        &stem,
        id,
        n,
        mode,
    );

    serve_image(path, "image/jpeg").await
}

/// GET /sub/:id  — serve the rendered ARIB subtitle overlay as a PNG.
///
/// The PNG is a full-frame (`cfg.capture.width × height`) RGBA canvas that is
/// transparent everywhere except the caption, so a client can scale it to the
/// displayed size and draw it at (0, 0) — exactly what the ffmpeg overlay branch
/// in `build_ffmpeg_args` does.
///
/// Returns `204 No Content` when the caption has no renderable subtitle (no PES
/// blob, empty caption list, or a fully transparent render).  Callers treat that
/// as "no overlay layer" rather than an error.
// axum handler; drives ensure_caption_png → aribcaption FFI over a live PES blob.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn sub_png(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, StatusCode> {
    let (ts_path, pts_start, _) = lookup_caption(&state, id).await?;

    let lock: Arc<AsyncMutex<()>> = {
        let mut map = state.gen_locks.lock().unwrap();
        map.entry(id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let cfg = state.config.clone();
    let ts_path_cl = ts_path.clone();
    let png = tokio::task::spawn_blocking(move || {
        let cache_dir = std::path::Path::new(&cfg.paths.cache_dir);
        subtitle::ensure_caption_png(&cfg.capture, cache_dir, &ts_path_cl, id, pts_start)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!("subtitle PNG render failed for {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match png {
        Some(path) => Ok(serve_image(path, "image/png").await?.into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// POST /recapture/:id  — clear the cached images for a single caption.
///
/// Deletes thumbs, full-resolution JPEGs and the subtitle PNG so the next
/// /thumb or /full request regenerates them from the TS file.
/// Uses the same per-caption lock as `thumb`/`full` to prevent races with
/// in-flight generation.
// axum handler; clears cached images via blocking IO, then relies on live TS + ffmpeg to regen.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
pub async fn recapture(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let (ts_path, _, _) = match lookup_caption(&state, id).await {
        Ok(v) => v,
        Err(s) => return s.into_response(),
    };

    let lock: Arc<AsyncMutex<()>> = {
        let mut map = state.gen_locks.lock().unwrap();
        map.entry(id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let cache_dir = std::path::PathBuf::from(&state.config.paths.cache_dir);

    match tokio::task::spawn_blocking(move || capture::clear_caption_cache(&cache_dir, &stem, id))
        .await
    {
        Ok(Ok(())) => (StatusCode::OK, "ok").into_response(),
        Ok(Err(e)) => {
            tracing::error!("recapture clear_cache failed {}: {:#}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, "error").into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "error").into_response(),
    }
}

// ------ helpers ------

async fn lookup_caption(state: &AppState, id: i64) -> Result<(PathBuf, i64, i64), StatusCode> {
    let row = sqlx::query!(
        "SELECT f.path, c.pts_start, c.pts_end \
         FROM captions c \
         JOIN ts_files f ON c.ts_file_id = f.id \
         WHERE c.id = ?",
        id,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db lookup failed for caption {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    // Rows written before the 33-bit unwrap fix can still hold a corrupt
    // timeline.  Refuse them here so no doomed ffmpeg process is spawned against
    // the NAS; the row is recoverable only by re-ingesting the file.
    if !crate::ts::pts::is_plausible_pts(row.pts_start, row.pts_end) {
        tracing::warn!(
            "caption {} has implausible pts {}..{}; refusing capture (re-ingest the file)",
            id,
            row.pts_start,
            row.pts_end,
        );
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    Ok((PathBuf::from(row.path), row.pts_start, row.pts_end))
}

// Async IO helper: reads a cached image from disk and builds the HTTP response.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
async fn serve_image(
    path: PathBuf,
    content_type: &'static str,
) -> Result<impl IntoResponse, StatusCode> {
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        tracing::error!("failed to read image at {}: {}", path.display(), e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(([(header::CONTENT_TYPE, content_type)], Bytes::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::{FullQuery, SubMode};

    #[test]
    fn sub_zero_selects_raw_mode() {
        assert_eq!(FullQuery { sub: Some(0) }.sub_mode(), SubMode::Raw);
    }

    #[test]
    fn absent_sub_keeps_burned_mode() {
        // Existing /full/{id}/{n} URLs must keep burning subtitles in.
        assert_eq!(FullQuery { sub: None }.sub_mode(), SubMode::Burned);
    }

    #[test]
    fn sub_one_selects_burned_mode() {
        assert_eq!(FullQuery { sub: Some(1) }.sub_mode(), SubMode::Burned);
    }
}
