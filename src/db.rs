use anyhow::Result;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;

/// Initialize the SQLite connection pool.
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

    create_schema(&pool).await?;

    // Recover from crashes: stale 'ingesting' records → 'pending'
    sqlx::query(
        "UPDATE ts_files SET status = 'pending', error_msg = NULL WHERE status = 'ingesting'",
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}

async fn create_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS programs (
            id               INTEGER PRIMARY KEY,
            title            TEXT NOT NULL UNIQUE,
            normalized_title TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ts_files (
            id             INTEGER PRIMARY KEY,
            path           TEXT UNIQUE NOT NULL,
            filename       TEXT NOT NULL,
            status         TEXT NOT NULL DEFAULT 'pending'
                           CHECK(status IN ('pending', 'ingesting', 'done', 'error')),
            error_msg      TEXT,
            ingested_at    DATETIME,
            program_id     INTEGER REFERENCES programs(id),
            episode_number INTEGER,
            episode_title  TEXT,
            air_date       DATE
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS captions (
            id         INTEGER PRIMARY KEY,
            ts_file_id INTEGER NOT NULL REFERENCES ts_files(id),
            pts_start  INTEGER NOT NULL,
            pts_end    INTEGER NOT NULL,
            text       TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE VIRTUAL TABLE IF NOT EXISTS captions_fts USING fts5(
            text,
            content=captions,
            content_rowid=id,
            tokenize='trigram'
        )",
    )
    .execute(pool)
    .await?;

    // Insert trigger: keep FTS in sync when captions are added.
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS captions_ai AFTER INSERT ON captions BEGIN
            INSERT INTO captions_fts(rowid, text) VALUES (new.id, new.text);
        END",
    )
    .execute(pool)
    .await?;

    // Delete trigger: keep FTS in sync when captions are removed (e.g. reingest reset).
    sqlx::query(
        "CREATE TRIGGER IF NOT EXISTS captions_ad AFTER DELETE ON captions BEGIN
            INSERT INTO captions_fts(captions_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
        END",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tags (
            id         INTEGER PRIMARY KEY,
            caption_id INTEGER NOT NULL REFERENCES captions(id) ON DELETE CASCADE,
            tag        TEXT NOT NULL,
            UNIQUE(caption_id, tag)
        )",
    )
    .execute(pool)
    .await?;

    // Index for tag-based filtering queries.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag)")
        .execute(pool)
        .await?;

    // Tracks which captions have had thumbnails generated, and which frame was selected.
    // ON DELETE CASCADE ensures rows are removed automatically when captions are deleted (e.g. reingest).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS thumbnails (
            caption_id     INTEGER PRIMARY KEY
                           REFERENCES captions(id) ON DELETE CASCADE,
            selected_frame INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}
