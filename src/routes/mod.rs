pub mod capture;
pub mod contact;
pub mod episodes;
pub mod ingest;
pub mod search;
pub mod tags;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::FromRef;
use sqlx::SqlitePool;
use tokio::sync::Mutex as AsyncMutex;

use captu::config::Config;

/// Shared application state.
/// FromRef<AppState> for SqlitePool allows existing search handlers
/// to keep using State<SqlitePool> without modification.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    /// Per-caption generation locks: prevents concurrent ffmpeg pipelines for the same caption.
    pub gen_locks: Arc<Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>>,
}

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

/// Format milliseconds as HH:MM:SS or MM:SS for display.
/// Negative values are clamped to zero (displayed as 00:00).
pub(crate) fn fmt_ms(ms: i64) -> String {
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

/// Escape LIKE special characters (%, _, \) so user input is treated literally.
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::{display_title, fmt_ms, like_escape};

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
