use anyhow::Result;
use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Deserializer};
use sqlx::Row;

use super::{display_title, like_escape, AppState};

const PAGE_SIZE: i64 = 50;

/// Treat missing or empty-string query params as None for integer fields.
fn empty_as_none_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = Option::<String>::deserialize(d)?;
    match s.as_deref() {
        None | Some("") => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(serde::de::Error::custom),
    }
}

/// Treat missing or blank-string query params as None for string fields.
/// Prevents always-present empty form fields (e.g. permanent date pickers)
/// from being treated as active filters and breaking SQL comparisons.
fn empty_as_none_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = Option::<String>::deserialize(d)?;
    match s {
        None => Ok(None),
        Some(v) => {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() { Ok(None) } else { Ok(Some(trimmed)) }
        }
    }
}

pub struct ProgramItem {
    pub id: i64,
    pub title: String,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub query: String,
    pub programs: Vec<ProgramItem>,
}

#[derive(Template)]
#[template(path = "search_results.html")]
pub struct SearchResultsTemplate {
    pub results: Vec<SearchResult>,
    /// Current page (0-based).
    pub page: i64,
    /// Total number of matching rows.
    pub total: i64,
    /// Rows loaded so far: min((page+1)*PAGE_SIZE, total).
    pub loaded: i64,
    pub has_next: bool,
}

pub struct SearchResult {
    pub id: i64,
    pub text: String,
    pub time_str: String,
    pub display_title: String,
    pub air_date: Option<String>,
    /// Whether thumbnails have been generated for this caption.
    pub has_thumb: bool,
    /// Frame index to show as preview (user-selected, or middle frame as fallback).
    pub preview_frame: i64,
}

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none_i64")]
    pub program_id: Option<i64>,
    #[serde(default, deserialize_with = "empty_as_none_i64")]
    pub ep: Option<i64>,
    #[serde(default, deserialize_with = "empty_as_none_string")]
    pub sub: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none_string")]
    pub date_from: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none_string")]
    pub date_to: Option<String>,
    pub filter: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none_i64")]
    pub page: Option<i64>,
}

pub async fn index(State(state): State<AppState>) -> Result<IndexTemplate, StatusCode> {
    let rows = sqlx::query("SELECT id, title FROM programs ORDER BY title")
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("index programs query: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let programs = rows
        .iter()
        .map(|r| ProgramItem {
            id: r.get("id"),
            title: r.get("title"),
        })
        .collect();

    Ok(IndexTemplate {
        query: String::new(),
        programs,
    })
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<SearchResultsTemplate, StatusCode> {
    let q = params.q.as_deref().unwrap_or("").trim().to_string();
    let filter = params
        .filter
        .as_deref()
        .unwrap_or("all")
        .to_string();
    let page = params.page.unwrap_or(0).max(0);
    let rep_frame = state.config.capture.thumb_count as i64 / 2;

    let active_q = if q.len() >= 2 { Some(q.as_str()) } else { None };
    let has_filter = params.program_id.is_some()
        || params.ep.is_some()
        || params.sub.is_some()
        || params.date_from.is_some()
        || params.date_to.is_some();
    let tab_active = filter != "all";

    // Return empty unless something is actually specified.
    if active_q.is_none() && !has_filter && !tab_active {
        return Ok(SearchResultsTemplate {
            results: vec![],
            page: 0,
            total: 0,
            loaded: 0,
            has_next: false,
        });
    }

    let (results, total) =
        run_search(&state, active_q, &params, &filter, page, rep_frame)
            .await
            .map_err(|e| {
                tracing::error!("search error: {:#}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let loaded = ((page + 1) * PAGE_SIZE).min(total);
    let has_next = (page + 1) * PAGE_SIZE < total;

    Ok(SearchResultsTemplate {
        results,
        page,
        total,
        loaded,
        has_next,
    })
}

async fn run_search(
    state: &AppState,
    q: Option<&str>,
    params: &SearchParams,
    filter: &str,
    page: i64,
    rep_frame: i64,
) -> Result<(Vec<SearchResult>, i64)> {
    let sub_trim = params.sub.as_deref().unwrap_or("").trim();

    // Pre-compute escaped LIKE patterns.
    let bind_text: Option<String> = q.map(|t| format!("%{}%", like_escape(t)));
    let bind_sub: Option<String> = if !sub_trim.is_empty() {
        Some(format!("%{}%", like_escape(sub_trim)))
    } else {
        None
    };

    // Build WHERE conditions (filter conditions use subqueries, no extra binds).
    let mut conditions: Vec<String> = Vec::new();

    if bind_text.is_some() {
        conditions.push("c.text LIKE ? ESCAPE '\\'".into());
    }
    if params.program_id.is_some() {
        conditions.push("f.program_id = ?".into());
    }
    if params.ep.is_some() {
        conditions.push("f.episode_number = ?".into());
    }
    if bind_sub.is_some() {
        conditions.push("f.episode_title LIKE ? ESCAPE '\\'".into());
    }
    if params.date_from.is_some() {
        conditions.push("f.air_date >= ?".into());
    }
    if params.date_to.is_some() {
        conditions.push("f.air_date <= ?".into());
    }
    match filter {
        "generated" => {
            conditions.push(
                "EXISTS(SELECT 1 FROM thumbnails th WHERE th.caption_id = c.id)".into(),
            );
        }
        "pending" => {
            conditions.push(
                "NOT EXISTS(SELECT 1 FROM thumbnails th WHERE th.caption_id = c.id)".into(),
            );
        }
        _ => {}
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    // ── COUNT query ──────────────────────────────────────────────────────────
    let count_sql = format!(
        "SELECT COUNT(*) FROM captions c \
         JOIN ts_files f ON c.ts_file_id = f.id \
         LEFT JOIN programs p ON f.program_id = p.id \
         {where_clause}"
    );

    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(ref t) = bind_text  { cq = cq.bind(t.as_str()); }
    if let Some(pid) = params.program_id { cq = cq.bind(pid); }
    if let Some(ep)  = params.ep    { cq = cq.bind(ep); }
    if let Some(ref s) = bind_sub   { cq = cq.bind(s.as_str()); }
    if let Some(ref d) = params.date_from { cq = cq.bind(d.as_str()); }
    if let Some(ref d) = params.date_to   { cq = cq.bind(d.as_str()); }

    let total: i64 = cq.fetch_one(&state.pool).await?;

    if total == 0 {
        return Ok((vec![], 0));
    }

    // ── Main query ───────────────────────────────────────────────────────────
    // COALESCE bind (rep_frame) must come BEFORE the WHERE binds.
    let main_sql = format!(
        r#"SELECT
            c.id         AS caption_id,
            c.text,
            c.pts_start  AS pts_start_ms,
            c.pts_end    AS pts_end_ms,
            f.air_date,
            f.episode_number,
            f.episode_title,
            COALESCE(p.title, f.filename) AS title,
            CASE WHEN t.caption_id IS NOT NULL THEN 1 ELSE 0 END AS has_thumb,
            COALESCE(t.selected_frame, ?) AS preview_frame
        FROM captions c
        JOIN ts_files  f ON c.ts_file_id = f.id
        LEFT JOIN programs  p ON f.program_id = p.id
        LEFT JOIN thumbnails t ON t.caption_id = c.id
        {where_clause}
        ORDER BY f.air_date DESC, f.episode_number, c.pts_start
        LIMIT ? OFFSET ?"#
    );

    let offset = page * PAGE_SIZE;

    let mut mq = sqlx::query(&main_sql);
    mq = mq.bind(rep_frame);                             // COALESCE fallback
    if let Some(ref t) = bind_text  { mq = mq.bind(t.as_str()); }
    if let Some(pid) = params.program_id { mq = mq.bind(pid); }
    if let Some(ep)  = params.ep    { mq = mq.bind(ep); }
    if let Some(ref s) = bind_sub   { mq = mq.bind(s.as_str()); }
    if let Some(ref d) = params.date_from { mq = mq.bind(d.as_str()); }
    if let Some(ref d) = params.date_to   { mq = mq.bind(d.as_str()); }
    mq = mq.bind(PAGE_SIZE).bind(offset);

    let rows = mq.fetch_all(&state.pool).await?;

    let results = rows
        .iter()
        .map(|row| {
            let start_ms: i64 = row.get("pts_start_ms");
            let end_ms: i64 = row.get("pts_end_ms");
            let has_thumb_int: i64 = row.get("has_thumb");
            SearchResult {
                id: row.get("caption_id"),
                text: row.get("text"),
                time_str: format!(
                    "{} – {}",
                    super::fmt_ms(start_ms),
                    super::fmt_ms(end_ms)
                ),
                display_title: display_title(
                    &row.get::<String, _>("title"),
                    row.get("episode_number"),
                    row.get::<Option<String>, _>("episode_title").as_deref(),
                ),
                air_date: row.get("air_date"),
                has_thumb: has_thumb_int != 0,
                preview_frame: row.get("preview_frame"),
            }
        })
        .collect();

    Ok((results, total))
}
