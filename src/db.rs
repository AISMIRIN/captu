use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;

/// Initialize the SQLite connection pool and apply all pending migrations.
/// max_connections should be concurrency + a small web overhead (e.g. concurrency + 5).
pub async fn init_db(db_path: &str, max_connections: u32) -> Result<SqlitePool> {
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let url = format!("sqlite://{}?mode=rwc", db_path);

    // WAL: readers never block writers and vice-versa.
    // synchronous=NORMAL: safe with WAL (no data loss on OS crash, fsync on checkpoint).
    // busy_timeout: parallel writers queue up to 5 s rather than returning SQLITE_BUSY.
    let opts = SqliteConnectOptions::from_str(&url)?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await?;

    // Apply all pending migrations from the migrations/ directory.
    sqlx::migrate!("./migrations").run(&pool).await?;

    // Recover from crashes: stale 'ingesting' records → 'pending'
    sqlx::query!(
        "UPDATE ts_files SET status = 'pending', error_msg = NULL WHERE status = 'ingesting'",
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}
