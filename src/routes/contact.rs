use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use sqlx::Row;

use super::{display_title, AppState};

#[derive(Template)]
#[template(path = "contact.html")]
pub struct ContactTemplate {
    pub caption_id: i64,
    pub ts_file_id: i64,
    pub display_title: String,
    pub time_str: String,
    pub text: String,
    /// Frame indices 0..thumb_count for the contact-sheet grid.
    pub frames: Vec<u32>,
    /// Last user-selected frame (or middle frame if never chosen).
    pub selected_frame: i64,
}

pub async fn contact(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<ContactTemplate, StatusCode> {
    let rep_frame = state.config.capture.thumb_count as i64 / 2;

    let row = sqlx::query(
        r#"
        SELECT
            f.id          AS ts_file_id,
            c.pts_start,
            c.pts_end,
            c.text,
            COALESCE(p.title, f.filename) AS title,
            f.episode_number,
            f.episode_title,
            COALESCE(t.selected_frame, ?) AS selected_frame
        FROM captions c
        JOIN ts_files f ON c.ts_file_id = f.id
        LEFT JOIN programs p ON f.program_id = p.id
        LEFT JOIN thumbnails t ON t.caption_id = c.id
        WHERE c.id = ?
        "#,
    )
    .bind(rep_frame)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error on /contact/{}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    let ts_file_id: i64 = row.get("ts_file_id");
    let pts_start: i64 = row.get("pts_start");
    let pts_end: i64 = row.get("pts_end");
    let text: String = row.get("text");
    let title: String = row.get("title");
    let episode_number: Option<i64> = row.get("episode_number");
    let episode_title: Option<String> = row.get("episode_title");
    let selected_frame: i64 = row.get("selected_frame");
    let thumb_count = state.config.capture.thumb_count;

    // Thumbnail generation is deferred to /thumb/:id/:n requests (with per-caption locking).

    Ok(ContactTemplate {
        caption_id: id,
        ts_file_id,
        display_title: display_title(&title, episode_number, episode_title.as_deref()),
        time_str: format!("{} – {}", super::fmt_ms(pts_start), super::fmt_ms(pts_end)),
        text,
        frames: (0..thumb_count).collect(),
        selected_frame,
    })
}
