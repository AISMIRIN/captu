pub mod capture;
pub mod contact;
pub mod episodes;
pub mod ingest;
pub mod search;
pub mod tags;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::FromRef,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use sqlx::SqlitePool;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::scheduler::IngestGuard;

/// Newtype wrapper that turns any askama Template into an axum IntoResponse.
/// Replaces the deprecated askama_axum crate.
pub struct HtmlTemplate<T>(pub T);

impl<T: askama::Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                tracing::error!("template render error: {:#}", e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Shared application state.
/// FromRef<AppState> for SqlitePool allows existing search handlers
/// to keep using State<SqlitePool> without modification.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    /// Per-caption generation locks: prevents concurrent ffmpeg pipelines for the same caption.
    pub gen_locks: Arc<Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>>,
    /// Shared with the startup scan and the cron scheduler so that a manual
    /// scan, a scheduled tick, and the startup scan never overlap.
    pub ingest_guard: IngestGuard,
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

/// Wire all application routes to a `Router` except `/static` (ServeDir).
///
/// Excludes the static file service so callers (main.rs, tests) can layer it
/// separately; integration tests typically skip it entirely.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(search::index))
        .route("/search", get(search::search))
        .route("/contact/{id}", get(contact::contact))
        .route("/thumb/{id}/{n}", get(capture::thumb))
        .route("/full/{id}/{n}", get(capture::full))
        .route("/preview/{id}", get(capture::preview))
        .route("/sub/{id}", get(capture::sub_png))
        .route("/select/{id}/{n}", post(capture::select_frame))
        .route("/api/episodes", get(episodes::episodes))
        .route("/api/tags", get(tags::tag_options))
        .route("/caption/{id}/tags", post(tags::add_tag))
        .route("/caption/{id}/tags/delete", post(tags::delete_tag))
        .route("/ingest/status", get(ingest::status))
        .route("/ingest/scan", post(ingest::scan))
        .route("/ingest/files", get(ingest::files))
        .route("/ingest/file/{id}", get(ingest::file_detail))
        .route("/ingest/clear/{id}", post(ingest::clear))
        .route("/ingest/cache/clear", post(ingest::clear_all_image_caches))
        .route("/ingest/cache/clear/{id}", post(ingest::clear_image_cache))
        .route("/reingest/{id}", post(ingest::reingest))
        .route("/recapture/{id}", post(capture::recapture))
        .with_state(state)
}

/// Format milliseconds as HH:MM:SS or MM:SS for display.
/// Negative values are clamped to zero (displayed as 00:00).
///
/// Values beyond the plausible range render as `--:--`.  `{:02}` does not
/// truncate, so a corrupt timestamp would otherwise print an 11-digit hour
/// field; such a value means a broken PTS timeline, not a long recording.
/// No logging here — this runs twice per rendered row.
pub(crate) fn fmt_ms(ms: i64) -> String {
    if ms > crate::ts::pts::MAX_PLAUSIBLE_PTS_MS {
        return "--:--".to_string();
    }
    let ms = ms.max(0);
    let total = ms / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

/// Build a display title from program title, optional episode number, and optional episode subtitle.
/// Non-empty parts are joined with a single space.
pub(crate) fn display_title(title: &str, ep: Option<i64>, sub: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !title.is_empty() {
        parts.push(title.to_string());
    }
    if let Some(n) = ep {
        parts.push(format!("#{}", n));
    }
    if let Some(s) = sub {
        let s = s.trim();
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    }
    parts.join(" ")
}

/// Format a byte count as a human-readable string (B / KiB / MiB / GiB).
pub(crate) fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", v, UNITS[unit])
    }
}

/// Escape LIKE special characters (%, _, \) so user input is treated literally.
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::{display_title, fmt_bytes, fmt_ms, like_escape};

    // ── fmt_bytes ─────────────────────────────────────────────────────────────

    #[test]
    fn fmt_bytes_plain_bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
    }

    #[test]
    fn fmt_bytes_kib_mib_gib() {
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn fmt_bytes_caps_at_gib() {
        // Above GiB the unit stays GiB (no TiB tier).
        assert_eq!(fmt_bytes(2048 * 1024 * 1024 * 1024), "2048.0 GiB");
    }

    // ── fmt_ms ────────────────────────────────────────────────────────────────

    #[test]
    fn fmt_ms_zero() {
        assert_eq!(fmt_ms(0), "00:00");
    }

    #[test]
    fn fmt_ms_negative_clamped_to_zero() {
        assert_eq!(fmt_ms(-1000), "00:00");
        assert_eq!(fmt_ms(i64::MIN), "00:00");
    }

    #[test]
    fn fmt_ms_under_one_hour() {
        // 1m 30s
        assert_eq!(fmt_ms(90_000), "01:30");
    }

    #[test]
    fn fmt_ms_exactly_one_hour() {
        assert_eq!(fmt_ms(3_600_000), "01:00:00");
    }

    #[test]
    fn fmt_ms_with_hours() {
        // 2h 5m 9s
        let ms = 2 * 3_600_000 + 5 * 60_000 + 9 * 1_000;
        assert_eq!(fmt_ms(ms), "02:05:09");
    }

    #[test]
    fn fmt_ms_huge_value_returns_placeholder() {
        // The value that was actually rendered before the 33-bit unwrap fix.
        assert_eq!(fmt_ms(204_963_822_946_342_000), "--:--");
        assert_eq!(fmt_ms(i64::MAX), "--:--");
    }

    #[test]
    fn fmt_ms_at_plausibility_boundary() {
        assert_eq!(fmt_ms(crate::ts::pts::MAX_PLAUSIBLE_PTS_MS), "24:00:00");
        assert_eq!(fmt_ms(crate::ts::pts::MAX_PLAUSIBLE_PTS_MS + 1), "--:--");
    }

    #[test]
    fn fmt_ms_59s_boundary() {
        assert_eq!(fmt_ms(59_999), "00:59");
        assert_eq!(fmt_ms(60_000), "01:00");
    }

    // ── display_title ─────────────────────────────────────────────────────────

    #[test]
    fn display_title_title_only() {
        assert_eq!(display_title("番組名", None, None), "番組名");
    }

    #[test]
    fn display_title_empty_title() {
        // Empty title is omitted
        assert_eq!(display_title("", Some(3), Some("タイトル")), "#3 タイトル");
    }

    #[test]
    fn display_title_all_parts() {
        assert_eq!(
            display_title("シリーズ名", Some(12), Some("サブタイトル")),
            "シリーズ名 #12 サブタイトル"
        );
    }

    #[test]
    fn display_title_sub_trimmed() {
        // Leading/trailing whitespace in sub should be trimmed
        assert_eq!(display_title("番組", None, Some("  サブ  ")), "番組 サブ");
    }

    #[test]
    fn display_title_blank_sub_omitted() {
        assert_eq!(display_title("番組", None, Some("   ")), "番組");
    }

    // ── like_escape ────────────────────────────────────────────────────────────

    #[test]
    fn like_escape_percent() {
        assert_eq!(like_escape("100%"), "100\\%");
    }

    #[test]
    fn like_escape_underscore() {
        assert_eq!(like_escape("a_b"), "a\\_b");
    }

    #[test]
    fn like_escape_backslash() {
        assert_eq!(like_escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn like_escape_combined() {
        // Input "10% off_\\special" should escape all three characters.
        // The backslash must be escaped first to avoid double-escaping.
        assert_eq!(like_escape("10% off_\\special"), "10\\% off\\_\\\\special");
    }

    #[test]
    fn like_escape_injection_attempt() {
        // A string that looks like a LIKE wildcard must be neutralised.
        assert_eq!(like_escape("%all%"), "\\%all\\%");
    }

    #[test]
    fn like_escape_plain_string() {
        assert_eq!(like_escape("hello"), "hello");
    }
}
