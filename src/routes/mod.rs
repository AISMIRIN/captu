pub mod capture;
pub mod contact;
pub mod episodes;
pub mod ingest;
pub mod search;

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
pub(crate) fn fmt_ms(ms: i64) -> String {
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
