use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
};

use super::{display_title, AppState};

#[derive(Template)]
#[template(path = "pages/contact.html")]
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
    /// Tags attached to this caption.
    pub tags: Vec<String>,
}

pub async fn contact(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<ContactTemplate, StatusCode> {
    let rep_frame = state.config.capture.thumb_count as i64 / 2;

    let row = sqlx::query!(
        r#"
        SELECT
            f.id          AS "ts_file_id!: i64",
            c.pts_start   AS "pts_start!: i64",
            c.pts_end     AS "pts_end!: i64",
            c.text,
            COALESCE(p.title, f.filename) AS "title!: String",
            f.episode_number,
            f.episode_title,
            COALESCE(t.selected_frame, ?) AS "selected_frame!: i64"
        FROM captions c
        JOIN ts_files f ON c.ts_file_id = f.id
        LEFT JOIN programs p ON f.program_id = p.id
        LEFT JOIN thumbnails t ON t.caption_id = c.id
        WHERE c.id = ?
        "#,
        rep_frame,
        id,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error on /contact/{}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    let thumb_count = state.config.capture.thumb_count;

    // Thumbnail generation is deferred to /thumb/:id/:n requests (with per-caption locking).

    // Load tags for this caption.
    let tags = super::tags::load_tags(&state.pool, id).await.map_err(|e| {
        tracing::error!("load_tags {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(ContactTemplate {
        caption_id: id,
        ts_file_id: row.ts_file_id,
        display_title: display_title(&row.title, row.episode_number, row.episode_title.as_deref()),
        time_str: format!("{} – {}", super::fmt_ms(row.pts_start), super::fmt_ms(row.pts_end)),
        text: row.text,
        frames: (0..thumb_count).collect(),
        selected_frame: row.selected_frame,
        tags,
    })
}
