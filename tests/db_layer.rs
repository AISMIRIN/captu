// Integration tests for the DB layer.
//
// These tests use a real SQLite file (via tempfile) so that FTS5 triggers,
// schema constraints, and the startup ingesting→pending recovery are all
// exercised exactly as in production.

use captu::db::init_db;
use captu::ingest::{delete_ts_file, reset_ts_file};
use sqlx::Row;
use tempfile::TempDir;

/// Create an isolated temp DB and return (pool, TempDir).
/// The TempDir must stay alive for the duration of the test.
async fn open_test_db() -> (sqlx::SqlitePool, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let pool = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("init_db failed");
    (pool, dir)
}

// ── Schema creation ────────────────────────────────────────────────────────────

#[tokio::test]
async fn schema_created_successfully() {
    let (pool, _dir) = open_test_db().await;

    // All expected tables should exist after init_db.
    for table in &["programs", "ts_files", "captions", "tags", "thumbnails"] {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(*table)
                .fetch_one(&pool)
                .await
                .expect("query failed");
        assert_eq!(count, 1, "table '{}' not found", table);
    }
}

// ── FTS5 trigger: captions_ai / captions_ad ────────────────────────────────────

#[tokio::test]
async fn fts5_insert_trigger_indexes_text() {
    let (pool, _dir) = open_test_db().await;

    // Insert a ts_file so captions can reference it.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/test/ep01.ts', 'ep01.ts', 'done')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let ts_file_id: i64 =
        sqlx::query_scalar("SELECT id FROM ts_files WHERE path = '/test/ep01.ts'")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Insert a caption with a unique phrase.
    sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 1000, 2000, ?)",
    )
    .bind(ts_file_id)
    .bind("テストの字幕テキスト uniquephrase")
    .execute(&pool)
    .await
    .unwrap();

    // The captions_ai trigger should have inserted into captions_fts.
    let hits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captions_fts WHERE text MATCH ?")
        .bind("uniquephrase")
        .fetch_one(&pool)
        .await
        .expect("FTS query failed");
    assert_eq!(hits, 1, "FTS should have one hit after insert");
}

#[tokio::test]
async fn fts5_delete_trigger_removes_from_index() {
    let (pool, _dir) = open_test_db().await;

    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/test/ep02.ts', 'ep02.ts', 'done')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let ts_file_id: i64 =
        sqlx::query_scalar("SELECT id FROM ts_files WHERE path = '/test/ep02.ts'")
            .fetch_one(&pool)
            .await
            .unwrap();

    sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 1000, ?)",
    )
    .bind(ts_file_id)
    .bind("deleteme phrase")
    .execute(&pool)
    .await
    .unwrap();

    // Confirm it's indexed
    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM captions_fts WHERE text MATCH 'deleteme'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, 1);

    // Delete the caption — captions_ad trigger should remove from FTS.
    sqlx::query("DELETE FROM captions WHERE ts_file_id = ?")
        .bind(ts_file_id)
        .execute(&pool)
        .await
        .unwrap();

    let after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM captions_fts WHERE text MATCH 'deleteme'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, 0, "FTS entry should be removed after caption delete");
}

// ── ingesting → pending recovery ──────────────────────────────────────────────

#[tokio::test]
async fn stale_ingesting_rows_reset_to_pending_on_init() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("recover.db");

    // First init: create schema.
    let pool = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("first init");

    // Simulate a crash: insert a row stuck in 'ingesting'.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/stuck.ts', 'stuck.ts', 'ingesting')",
    )
    .execute(&pool)
    .await
    .unwrap();

    pool.close().await;

    // Second init (simulates app restart): the recovery UPDATE should fire.
    let pool2 = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("second init");

    let status: String =
        sqlx::query_scalar("SELECT status FROM ts_files WHERE path = '/nas/stuck.ts'")
            .fetch_one(&pool2)
            .await
            .expect("row not found");

    assert_eq!(
        status, "pending",
        "stuck ingesting row should be reset to pending"
    );
}

// ── delete_ts_file: cache subtree guard ───────────────────────────────────────

#[tokio::test]
async fn delete_ts_file_removes_only_cache_subtree() {
    let (pool, _dir) = open_test_db().await;

    // Set up a temp cache directory with two subdirs: one for the file we delete
    // and one "other" that must survive.
    let cache_tmp = TempDir::new().expect("cache tempdir");
    let cache_dir = cache_tmp.path();

    let subtree = cache_dir.join("ep01");
    std::fs::create_dir_all(&subtree).unwrap();
    std::fs::write(subtree.join("captions.pes"), b"dummy").unwrap();

    let other = cache_dir.join("ep02");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("captions.pes"), b"other").unwrap();

    // Insert the ts_file record.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/ep01.ts', 'ep01.ts', 'done')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM ts_files WHERE path = '/nas/ep01.ts'")
        .fetch_one(&pool)
        .await
        .unwrap();

    delete_ts_file(&pool, id, cache_dir)
        .await
        .expect("delete_ts_file failed");

    assert!(!subtree.exists(), "ep01 subtree should be deleted");
    assert!(other.exists(), "ep02 subtree must NOT be deleted");
}

#[tokio::test]
async fn delete_ts_file_empty_stem_does_not_remove_cache_root() {
    // This tests the cache_subtree guard: a ts path with no stem must not
    // cause remove_dir_all on the cache directory itself.
    let (pool, _dir) = open_test_db().await;

    let cache_tmp = TempDir::new().expect("cache tempdir");
    let cache_dir = cache_tmp.path();
    let sentinel = cache_dir.join("important_data");
    std::fs::create_dir_all(&sentinel).unwrap();

    // Insert a path that has no file stem (trailing slash = directory path).
    // SQLite allows any string, so we can store a pathological value.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/nofolder/', 'nofolder', 'done')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM ts_files WHERE path = '/nas/nofolder/'")
        .fetch_one(&pool)
        .await
        .unwrap();

    delete_ts_file(&pool, id, cache_dir)
        .await
        .expect("delete_ts_file failed");

    assert!(
        sentinel.exists(),
        "cache root contents must survive when ts_path has no file stem"
    );
}

// ── reset_ts_file ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reset_ts_file_clears_metadata_and_returns_to_pending() {
    let (pool, _dir) = open_test_db().await;
    let cache_tmp = TempDir::new().expect("cache tempdir");

    sqlx::query(
        "INSERT INTO ts_files (path, filename, status, episode_number)
         VALUES ('/nas/ep03.ts', 'ep03.ts', 'done', 5)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM ts_files WHERE path = '/nas/ep03.ts'")
        .fetch_one(&pool)
        .await
        .unwrap();

    reset_ts_file(&pool, id, cache_tmp.path())
        .await
        .expect("reset_ts_file failed");

    let row = sqlx::query("SELECT status, episode_number FROM ts_files WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let status: String = row.get("status");
    let ep: Option<i64> = row.get("episode_number");

    assert_eq!(status, "pending");
    assert_eq!(ep, None, "episode_number should be cleared after reset");
}
