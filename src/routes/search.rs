use anyhow::Result;
use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Deserializer};
use sqlx::Row;
use std::collections::HashMap;

use super::{display_title, like_escape, AppState};

const PAGE_SIZE: i64 = 50;
/// Maximum accepted page number from user input.  Pages beyond this are clamped
/// so that `(page + 1) * PAGE_SIZE` can never overflow i64.
const MAX_PAGE: i64 = 1_000_000;

/// Compute pagination window from a (already-clamped) page number and total count.
/// Returns (loaded, has_next, offset).
fn page_window(page: i64, total: i64) -> (i64, bool, i64) {
    let next_start = page.saturating_add(1).saturating_mul(PAGE_SIZE);
    let loaded = next_start.min(total);
    let has_next = next_start < total;
    let offset = page.saturating_mul(PAGE_SIZE);
    (loaded, has_next, offset)
}

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
    /// Tags attached to this caption.
    pub tags: Vec<String>,
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
    /// Newline-separated list of tags for AND filtering.
    #[serde(default, deserialize_with = "empty_as_none_string")]
    pub tags: Option<String>,
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
    let page = params.page.unwrap_or(0).clamp(0, MAX_PAGE);
    let rep_frame = state.config.capture.thumb_count as i64 / 2;

    let active_q = if q.len() >= 2 { Some(q.as_str()) } else { None };
    let tag_list: Vec<&str> = params
        .tags
        .as_deref()
        .unwrap_or("")
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let has_filter = params.program_id.is_some()
        || params.ep.is_some()
        || params.sub.is_some()
        || params.date_from.is_some()
        || params.date_to.is_some()
        || !tag_list.is_empty();
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
        run_search(&state, active_q, &params, &tag_list, &filter, page, rep_frame)
            .await
            .map_err(|e| {
                tracing::error!("search error: {:#}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    let (loaded, has_next, _) = page_window(page, total);

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
    tag_list: &[&str],
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
    // Each tag in tag_list adds an independent EXISTS condition (AND semantics).
    for _ in tag_list {
        conditions.push(
            "EXISTS(SELECT 1 FROM tags tg WHERE tg.caption_id = c.id AND tg.tag = ?)".into(),
        );
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
    for t in tag_list { cq = cq.bind(*t); }

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

    let (_, _, offset) = page_window(page, total);

    let mut mq = sqlx::query(&main_sql);
    mq = mq.bind(rep_frame);                             // COALESCE fallback
    if let Some(ref t) = bind_text  { mq = mq.bind(t.as_str()); }
    if let Some(pid) = params.program_id { mq = mq.bind(pid); }
    if let Some(ep)  = params.ep    { mq = mq.bind(ep); }
    if let Some(ref s) = bind_sub   { mq = mq.bind(s.as_str()); }
    if let Some(ref d) = params.date_from { mq = mq.bind(d.as_str()); }
    if let Some(ref d) = params.date_to   { mq = mq.bind(d.as_str()); }
    for t in tag_list { mq = mq.bind(*t); }
    mq = mq.bind(PAGE_SIZE).bind(offset);

    let rows = mq.fetch_all(&state.pool).await?;

    let mut results: Vec<SearchResult> = rows
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
                tags: vec![],
            }
        })
        .collect();

    // Bulk-load tags for the current page of results (single query, no N+1).
    // IDs are i64 literals — no user input involved, so inline formatting is safe.
    if !results.is_empty() {
        let id_list: Vec<String> = results.iter().map(|r| r.id.to_string()).collect();
        let tags_sql = format!(
            "SELECT caption_id, tag FROM tags WHERE caption_id IN ({}) ORDER BY tag",
            id_list.join(",")
        );
        let tag_rows = sqlx::query(&tags_sql).fetch_all(&state.pool).await?;
        let mut tags_map: HashMap<i64, Vec<String>> = HashMap::new();
        for row in &tag_rows {
            tags_map
                .entry(row.get::<i64, _>("caption_id"))
                .or_default()
                .push(row.get::<String, _>("tag"));
        }
        for r in &mut results {
            r.tags = tags_map.remove(&r.id).unwrap_or_default();
        }
    }

    Ok((results, total))
}

#[cfg(test)]
mod tests {
    use super::{empty_as_none_i64, empty_as_none_string, page_window, MAX_PAGE, PAGE_SIZE};
    use serde::de::IntoDeserializer;
    use serde_json::Value as JValue;

    // ── empty_as_none_i64 ─────────────────────────────────────────────────────
    // The deserializer expects Option<String> semantics: None (absent) or Some(string).
    // We simulate this via serde_json::Value (Null → absent, String → present).

    fn de_i64(opt: Option<&str>) -> Option<i64> {
        let v: JValue = match opt {
            None => JValue::Null,
            Some(s) => JValue::String(s.to_string()),
        };
        empty_as_none_i64(v.into_deserializer()).expect("deserialize failed")
    }

    #[test]
    fn empty_i64_absent_gives_none() {
        assert_eq!(de_i64(None), None);
    }

    #[test]
    fn empty_i64_empty_string_gives_none() {
        assert_eq!(de_i64(Some("")), None);
    }

    #[test]
    fn empty_i64_valid_number() {
        assert_eq!(de_i64(Some("42")), Some(42));
    }

    #[test]
    fn empty_i64_negative() {
        assert_eq!(de_i64(Some("-1")), Some(-1));
    }

    #[test]
    fn empty_i64_parse_error() {
        let v: JValue = JValue::String("abc".to_string());
        assert!(empty_as_none_i64(v.into_deserializer()).is_err());
    }

    // ── empty_as_none_string ──────────────────────────────────────────────────

    fn de_str(opt: Option<&str>) -> Option<String> {
        let v: JValue = match opt {
            None => JValue::Null,
            Some(s) => JValue::String(s.to_string()),
        };
        empty_as_none_string(v.into_deserializer()).expect("deserialize failed")
    }

    #[test]
    fn empty_string_absent_gives_none() {
        assert_eq!(de_str(None), None);
    }

    #[test]
    fn empty_string_gives_none() {
        assert_eq!(de_str(Some("")), None);
    }

    #[test]
    fn blank_string_gives_none() {
        assert_eq!(de_str(Some("   ")), None);
    }

    #[test]
    fn trimmed_string_returned() {
        assert_eq!(de_str(Some("  abc  ")), Some("abc".to_string()));
    }

    #[test]
    fn nonempty_string_passthrough() {
        assert_eq!(de_str(Some("hello")), Some("hello".to_string()));
    }

    // ── active_q byte-length rule ─────────────────────────────────────────────

    #[test]
    fn active_q_single_ascii_char_inactive() {
        // Single ASCII byte (len=1) must NOT activate search
        let q = "a";
        assert!(q.len() < 2, "single ASCII char should have len < 2");
    }

    #[test]
    fn active_q_single_japanese_char_active() {
        // Single hiragana is 3 bytes in UTF-8 → activates search
        let q = "あ";
        assert!(q.len() >= 2, "single Japanese char should have len >= 2");
    }

    #[test]
    fn active_q_two_ascii_chars_active() {
        let q = "ab";
        assert!(q.len() >= 2);
    }

    // ── tag_list parsing ──────────────────────────────────────────────────────

    fn parse_tag_list(s: &str) -> Vec<String> {
        s.split('\n')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn tag_list_splits_on_newline() {
        assert_eq!(parse_tag_list("a\nb\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn tag_list_trims_whitespace() {
        assert_eq!(parse_tag_list(" tag1 \n  tag2  "), vec!["tag1", "tag2"]);
    }

    #[test]
    fn tag_list_drops_empty_lines() {
        assert_eq!(parse_tag_list("a\n\nb"), vec!["a", "b"]);
    }

    #[test]
    fn tag_list_empty_input() {
        let result: Vec<String> = parse_tag_list("");
        assert!(result.is_empty());
    }

    // ── page_window ───────────────────────────────────────────────────────────

    #[test]
    fn page_window_first_page() {
        let total = 120;
        let (loaded, has_next, offset) = page_window(0, total);
        assert_eq!(loaded, PAGE_SIZE);
        assert!(has_next);
        assert_eq!(offset, 0);
    }

    #[test]
    fn page_window_last_page() {
        // 120 total, page 2 (rows 100–119)
        let total = 120;
        let (loaded, has_next, offset) = page_window(2, total);
        assert_eq!(loaded, total);
        assert!(!has_next);
        assert_eq!(offset, 2 * PAGE_SIZE);
    }

    #[test]
    fn page_window_partial_last_page() {
        // 75 total, page 1 (rows 50–74 = 25 rows)
        let total = 75;
        let (loaded, has_next, offset) = page_window(1, total);
        assert_eq!(loaded, total);
        assert!(!has_next);
        assert_eq!(offset, PAGE_SIZE);
    }

    #[test]
    fn page_window_page_beyond_total() {
        // page far past end — loaded should be clamped to total
        let total = 10;
        let (loaded, has_next, offset) = page_window(100, total);
        assert_eq!(loaded, total);
        assert!(!has_next);
        assert_eq!(offset, 100 * PAGE_SIZE);
    }

    #[test]
    fn page_window_max_page_no_overflow() {
        // MAX_PAGE must not overflow i64 with saturating arithmetic
        let total = i64::MAX;
        let (loaded, has_next, offset) = page_window(MAX_PAGE, total);
        assert!(loaded > 0);
        assert!(offset >= 0, "offset must not wrap to negative");
        let _ = has_next; // just exercise the branch
    }

    #[test]
    fn page_window_huge_page_no_overflow() {
        // Even i64::MAX as page must not panic
        let total = 100;
        let (loaded, has_next, offset) = page_window(i64::MAX, total);
        assert_eq!(loaded, total);
        assert!(!has_next);
        assert!(offset >= 0, "offset must not wrap to negative");
    }
}
