use anyhow::Result;
use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::{HeaderName, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};

use super::AppState;

/// Fetch all tags for a caption, ordered alphabetically.
pub(crate) async fn load_tags(pool: &SqlitePool, caption_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT tag FROM tags WHERE caption_id = ? ORDER BY tag")
        .bind(caption_id)
        .fetch_all(pool)
        .await?;

    Ok(rows.iter().map(|r| r.get::<String, _>("tag")).collect())
}

#[derive(Deserialize)]
pub struct TagForm {
    pub tag: String,
}

#[derive(Template)]
#[template(path = "fragments/tags.html")]
pub struct TagsFragment {
    pub caption_id: i64,
    pub tags: Vec<String>,
}

#[derive(Template)]
#[template(path = "fragments/tag_options.html")]
pub struct TagOptionsTemplate {
    pub tags: Vec<String>,
}

/// Response header announcing that the global tag list changed.
/// htmx listeners with `hx-trigger="tagsChanged from:body"` will re-fetch /api/tags.
const HX_TRIGGER: (HeaderName, &str) = (
    HeaderName::from_static("hx-trigger"),
    "tagsChanged",
);

/// Wrap a TagsFragment with the tagsChanged trigger header.
fn with_tags_trigger(frag: TagsFragment) -> impl IntoResponse {
    ([HX_TRIGGER], frag)
}

/// POST /caption/:id/tags — add a tag (idempotent via INSERT OR IGNORE).
pub async fn add_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<TagForm>,
) -> Result<impl IntoResponse, StatusCode> {
    let tag = form.tag.trim().to_string();

    if !tag.is_empty() {
        sqlx::query("INSERT OR IGNORE INTO tags(caption_id, tag) VALUES (?, ?)")
            .bind(id)
            .bind(&tag)
            .execute(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("add_tag {}/{:?}: {:#}", id, tag, e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    let tags = load_tags(&state.pool, id).await.map_err(|e| {
        tracing::error!("load_tags after add {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(with_tags_trigger(TagsFragment { caption_id: id, tags }))
}

/// POST /caption/:id/tags/delete — remove a tag.
pub async fn delete_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<TagForm>,
) -> Result<impl IntoResponse, StatusCode> {
    let tag = form.tag.trim().to_string();

    sqlx::query("DELETE FROM tags WHERE caption_id = ? AND tag = ?")
        .bind(id)
        .bind(&tag)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("delete_tag {}/{:?}: {:#}", id, tag, e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let tags = load_tags(&state.pool, id).await.map_err(|e| {
        tracing::error!("load_tags after delete {}: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(with_tags_trigger(TagsFragment { caption_id: id, tags }))
}

/// GET /api/tags — list all distinct tags for autocomplete and filter select.
pub async fn tag_options(State(state): State<AppState>) -> Result<TagOptionsTemplate, StatusCode> {
    let rows = sqlx::query("SELECT DISTINCT tag FROM tags ORDER BY tag")
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("tag_options query: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let tags = rows.iter().map(|r| r.get::<String, _>("tag")).collect();

    Ok(TagOptionsTemplate { tags })
}
