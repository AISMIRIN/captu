use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sqlx::{QueryBuilder, Row, Sqlite};
use std::path::PathBuf;

use captu::ingest;

use super::{display_title, like_escape, AppState};

const FILE_PAGE_SIZE: i64 = 50;

pub struct ErrorEntry {
    pub filename: String,
    pub error_msg: String,
}

// ── File list ──────────────────────────────────────────────────────────────

pub struct FileListItem {
    pub id: i64,
    pub filename: String,
    pub status: String,
    pub error_msg: Option<String>,
    pub display_title: String,
    pub air_date: Option<String>,
    pub caption_count: i64,
}

#[derive(Template)]
#[template(path = "pages/ingest_files.html")]
pub struct IngestFilesTemplate {
    pub q: String,
    pub status_filter: String,
    pub files: Vec<FileListItem>,
    pub page: i64,
    pub total: i64,
    pub has_prev: bool,
    pub has_next: bool,
}

#[derive(Deserialize)]
pub struct FilesParams {
    pub q: Option<String>,
    pub status: Option<String>,
    pub page: Option<i64>,
}

// ── File detail ────────────────────────────────────────────────────────────

pub struct FileDetail {
    pub id: i64,
    pub filename: String,
    pub path: String,
    pub status: String,
    pub error_msg: Option<String>,
    pub ingested_at: Option<String>,
    pub display_title: String,
    pub air_date: Option<String>,
    pub caption_count: i64,
}

#[derive(Template)]
#[template(path = "pages/ingest_file.html")]
pub struct IngestFileTemplate {
    pub file: FileDetail,
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
    let count_rows = sqlx::query!(
        r#"SELECT status, COUNT(*) AS "cnt!: i64" FROM ts_files GROUP BY status"#
    )
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
        match row.status.as_str() {
            "pending"   => pending   = row.cnt,
            "ingesting" => ingesting = row.cnt,
            "done"      => done      = row.cnt,
            "error"     => error     = row.cnt,
            _ => {}
        }
    }
    let total = pending + ingesting + done + error;

    // List files currently being ingested.
    let ing_rows = sqlx::query!(
        "SELECT filename FROM ts_files WHERE status = 'ingesting'",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("/ingest/status (ingesting list) db error: {:#}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let ingesting_files: Vec<String> = ing_rows.into_iter().map(|r| r.filename).collect();

    // Most recent errors (up to 5).
    let err_rows = sqlx::query!(
        r#"SELECT filename, COALESCE(error_msg, '(unknown)') AS "error_msg!: String"
         FROM ts_files
         WHERE status = 'error'
         ORDER BY ingested_at DESC
         LIMIT 5"#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("/ingest/status (error list) db error: {:#}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let recent_errors = err_rows
        .into_iter()
        .map(|r| ErrorEntry {
            filename: r.filename,
            error_msg: r.error_msg,
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

/// GET /ingest/files — searchable, paginated list of all TS files.
pub async fn files(
    State(state): State<AppState>,
    Query(params): Query<FilesParams>,
) -> Result<IngestFilesTemplate, StatusCode> {
    let q = params.q.as_deref().unwrap_or("").trim().to_string();
    let status_filter = params
        .status
        .as_deref()
        .unwrap_or("all")
        .trim()
        .to_string();
    let page = params.page.unwrap_or(0).max(0);
    let offset = page * FILE_PAGE_SIZE;

    let bind_q: Option<String> = if q.is_empty() {
        None
    } else {
        Some(format!("%{}%", like_escape(&q)))
    };

    let base_from = "FROM ts_files f LEFT JOIN programs p ON f.program_id = p.id";

    // ── COUNT query ──────────────────────────────────────────────────────────
    let total: i64 = {
        let mut qb = QueryBuilder::<Sqlite>::new(format!("SELECT COUNT(*) {base_from}"));
        push_files_where(&mut qb, bind_q.as_deref(), &status_filter);
        qb.build_query_scalar()
            .fetch_one(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("/ingest/files count error: {:#}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    };

    // ── Main query ───────────────────────────────────────────────────────────
    let rows = {
        let mut qb = QueryBuilder::<Sqlite>::new(format!(
            "SELECT f.id, f.filename, f.status, f.error_msg, f.ingested_at, \
                    f.episode_number, f.episode_title, f.air_date, \
                    COALESCE(p.title, f.filename) AS title, \
                    (SELECT COUNT(*) FROM captions c WHERE c.ts_file_id = f.id) AS caption_count \
             {base_from}"
        ));
        push_files_where(&mut qb, bind_q.as_deref(), &status_filter);
        qb.push(" ORDER BY f.ingested_at DESC, f.id DESC LIMIT ");
        qb.push_bind(FILE_PAGE_SIZE);
        qb.push(" OFFSET ");
        qb.push_bind(offset);
        qb.build()
            .fetch_all(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("/ingest/files list error: {:#}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
    };

    let file_items: Vec<FileListItem> = rows
        .iter()
        .map(|r| {
            let title: String = r.get("title");
            let ep: Option<i64> = r.get("episode_number");
            let sub: Option<String> = r.get("episode_title");
            FileListItem {
                id: r.get("id"),
                filename: r.get("filename"),
                status: r.get("status"),
                error_msg: r.get("error_msg"),
                display_title: display_title(&title, ep, sub.as_deref()),
                air_date: r.get("air_date"),
                caption_count: r.get("caption_count"),
            }
        })
        .collect();

    let has_next = offset + FILE_PAGE_SIZE < total;
    let has_prev = page > 0;

    Ok(IngestFilesTemplate {
        q,
        status_filter,
        files: file_items,
        page,
        total,
        has_prev,
        has_next,
    })
}

/// Append a WHERE clause to a QueryBuilder for the files() list.
/// Conditions: filename LIKE (if bind_q is Some) and/or status = (if not "all").
fn push_files_where(qb: &mut QueryBuilder<Sqlite>, bind_q: Option<&str>, status_filter: &str) {
    let mut n = 0usize;

    macro_rules! cond {
        ($qb:expr, $n:ident) => {{
            if $n == 0 {
                $qb.push(" WHERE ");
            } else {
                $qb.push(" AND ");
            }
            $n += 1;
        }};
    }

    if let Some(bq) = bind_q {
        cond!(qb, n);
        qb.push("f.filename LIKE ").push_bind(bq.to_owned()).push(" ESCAPE '\\'");
    }
    if status_filter != "all" {
        cond!(qb, n);
        qb.push("f.status = ").push_bind(status_filter.to_owned());
    }
    // Silence unused_assignments: n is a write-only counter after the last condition.
    let _ = n;
}

/// GET /ingest/file/:id — detail page for a single TS file.
pub async fn file_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<IngestFileTemplate, StatusCode> {
    let row = sqlx::query!(
        r#"SELECT f.id AS "id!: i64", f.filename, f.path, f.status, f.error_msg, f.ingested_at,
                f.episode_number, f.episode_title, f.air_date,
                COALESCE(p.title, f.filename) AS "title!: String",
                (SELECT COUNT(*) FROM captions c WHERE c.ts_file_id = f.id) AS "caption_count!: i64"
         FROM ts_files f LEFT JOIN programs p ON f.program_id = p.id
         WHERE f.id = ?"#,
        id,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("/ingest/file/{} db error: {:#}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    Ok(IngestFileTemplate {
        file: FileDetail {
            id: row.id,
            filename: row.filename,
            path: row.path,
            status: row.status,
            error_msg: row.error_msg,
            // ingested_at is DATETIME; chrono decodes as NaiveDateTime — convert to String.
            ingested_at: row.ingested_at.map(|dt| dt.to_string()),
            display_title: display_title(&row.title, row.episode_number, row.episode_title.as_deref()),
            // air_date is DATE; chrono decodes as NaiveDate — convert to String.
            air_date: row.air_date.map(|d| d.to_string()),
            caption_count: row.caption_count,
        },
    })
}

/// POST /ingest/clear/:id — delete captions/cache for a TS file, keep the row.
pub async fn clear(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let cache_dir = PathBuf::from(&state.config.paths.cache_dir);

    match ingest::clear_subtitles(&state.pool, id, &cache_dir).await {
        Ok(()) => (StatusCode::OK, "字幕情報を削除しました").into_response(),
        Err(e) => {
            tracing::error!("ingest/clear error for id={}: {:#}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, "エラーが発生しました").into_response()
        }
    }
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
