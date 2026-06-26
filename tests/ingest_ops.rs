// Integration tests for ingest.rs database/filesystem operations.
//
// Exercises scan_and_enqueue, reconcile_deleted, enqueue_missing_pes, and
// reset_program using real (tempfile) TS files and in-memory SQLite DBs.
// Functions that drive ffmpeg/FFI (run_workers, do_ingest, etc.) are
// exercised separately and remain under #[coverage(off)].

use std::path::PathBuf;
use std::sync::Arc;

use captu::{
    config::{CaptureConfig, Config, IngestConfig, PathsConfig, ServerConfig},
    db::init_db,
    ingest::{
        clear_subtitles, delete_ts_file, enqueue_missing_pes, reconcile_deleted, reset_program,
        reset_ts_file, scan_and_enqueue,
    },
};
use sqlx::Row;
use tempfile::TempDir;

// ── test helper ──────────────────────────────────────────────────────────────

struct TestEnv {
    pool: sqlx::SqlitePool,
    config: Arc<Config>,
    _dir: TempDir,
}

/// Build a Config whose nas_mount, cache_dir, and db_path all live in the same tempdir.
async fn make_env() -> TestEnv {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let pool = init_db(db_path.to_str().unwrap(), 2)
        .await
        .expect("init_db");

    let config = Arc::new(Config {
        paths: PathsConfig {
            nas_mount: dir.path().to_string_lossy().to_string(),
            ts_glob: "*.ts".to_string(),
            cache_dir: dir.path().join("cache").to_string_lossy().to_string(),
            db_path: db_path.to_string_lossy().to_string(),
        },
        capture: CaptureConfig {
            thumb_count: 6,
            thumb_width: 640,
            thumb_height: 360,
            thumb_quality: 4,
            width: 1920,
            height: 1080,
            jpeg_quality: 2,
        },
        ingest: IngestConfig {
            schedule_cron: String::new(),
            run_on_startup: false,
            concurrency: 1,
            require_captions: false,
            filter_include: vec![],
            filter_exclude: vec![],
        },
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 8000,
        },
    });

    TestEnv {
        pool,
        config,
        _dir: dir,
    }
}

/// Create an empty .ts file in the nas_mount dir.
fn create_ts_file(env: &TestEnv, name: &str) -> PathBuf {
    let path = std::path::Path::new(&env.config.paths.nas_mount).join(name);
    std::fs::File::create(&path).expect("create ts file");
    path
}

// ── scan_and_enqueue ──────────────────────────────────────────────────────────

#[tokio::test]
async fn scan_and_enqueue_empty_dir_queues_nothing() {
    let env = make_env().await;
    let count = scan_and_enqueue(&env.config, &env.pool)
        .await
        .expect("scan");
    assert_eq!(count, 0, "no .ts files → nothing queued");
}

#[tokio::test]
async fn scan_and_enqueue_discovers_new_ts_files() {
    let env = make_env().await;
    create_ts_file(&env, "ep01.ts");
    create_ts_file(&env, "ep02.ts");

    let count = scan_and_enqueue(&env.config, &env.pool)
        .await
        .expect("scan");
    assert_eq!(count, 2, "two new .ts files should be queued");

    let status: String =
        sqlx::query_scalar("SELECT status FROM ts_files WHERE filename = 'ep01.ts'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn scan_and_enqueue_skips_already_done_files() {
    let env = make_env().await;
    let path = create_ts_file(&env, "done.ts");

    // Pre-insert as 'done'.
    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'done.ts', 'done')")
        .bind(path.to_string_lossy().as_ref())
        .execute(&env.pool)
        .await
        .unwrap();

    let count = scan_and_enqueue(&env.config, &env.pool)
        .await
        .expect("scan");
    assert_eq!(count, 0, "already-done file should not be re-queued");
}

#[tokio::test]
async fn scan_and_enqueue_skips_ingesting_files() {
    let env = make_env().await;
    let path = create_ts_file(&env, "ing.ts");

    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'ing.ts', 'ingesting')")
        .bind(path.to_string_lossy().as_ref())
        .execute(&env.pool)
        .await
        .unwrap();

    let count = scan_and_enqueue(&env.config, &env.pool)
        .await
        .expect("scan");
    assert_eq!(count, 0, "actively-ingesting file should not be re-queued");
}

#[tokio::test]
async fn scan_and_enqueue_exclude_filter_skips_matching_files() {
    let env = make_env().await;
    create_ts_file(&env, "keep.ts");
    create_ts_file(&env, "skip_me.ts");

    let mut config = (*env.config).clone();
    // Use **/ prefix so the pattern can match the full absolute path (glob * doesn't cross /).
    config.ingest.filter_exclude = vec!["**/*skip*.ts".to_string()];
    let config = Arc::new(config);

    let count = scan_and_enqueue(&config, &env.pool).await.expect("scan");
    // Only "keep.ts" should be queued.
    assert_eq!(count, 1, "excluded file should not be queued");
}

#[tokio::test]
async fn scan_and_enqueue_include_filter_accepts_only_matching_files() {
    let env = make_env().await;
    create_ts_file(&env, "accept.ts");
    create_ts_file(&env, "reject.ts");

    let mut config = (*env.config).clone();
    config.ingest.filter_include = vec!["**/*accept*".to_string()];
    let config = Arc::new(config);

    let count = scan_and_enqueue(&config, &env.pool).await.expect("scan");
    assert_eq!(count, 1, "only included file should be queued");
}

// ── reconcile_deleted ─────────────────────────────────────────────────────────

#[tokio::test]
async fn reconcile_deleted_skips_when_nas_mount_missing() {
    let env = make_env().await;
    let mut config = (*env.config).clone();
    config.paths.nas_mount = "/nonexistent/nas/mountpoint".to_string();
    let config = Arc::new(config);

    let removed = reconcile_deleted(&config, &env.pool, 5)
        .await
        .expect("reconcile");
    assert_eq!(removed, 0, "missing NAS mountpoint → reconcile skipped");
}

#[tokio::test]
async fn reconcile_deleted_skips_when_glob_returned_zero() {
    let env = make_env().await;
    // disk_file_count=0 guards against NAS unmount.
    let removed = reconcile_deleted(&env.config, &env.pool, 0)
        .await
        .expect("reconcile");
    assert_eq!(removed, 0, "zero glob count → reconcile skipped");
}

#[tokio::test]
async fn reconcile_deleted_removes_rows_for_missing_files() {
    let env = make_env().await;

    // Insert a 'done' row whose path no longer exists on disk.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nonexistent/ghost.ts', 'ghost.ts', 'done')"
    )
    .execute(&env.pool).await.unwrap();

    let removed = reconcile_deleted(&env.config, &env.pool, 1)
        .await
        .expect("reconcile");
    assert_eq!(removed, 1, "ghost file should be removed");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ts_files")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "ts_files table should be empty after reconcile");
}

#[tokio::test]
async fn reconcile_deleted_keeps_files_still_on_disk() {
    let env = make_env().await;
    let path = create_ts_file(&env, "present.ts");

    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'present.ts', 'done')")
        .bind(path.to_string_lossy().as_ref())
        .execute(&env.pool)
        .await
        .unwrap();

    let removed = reconcile_deleted(&env.config, &env.pool, 1)
        .await
        .expect("reconcile");
    assert_eq!(removed, 0, "file that still exists should not be removed");
}

#[tokio::test]
async fn reconcile_deleted_skips_ingesting_rows() {
    let env = make_env().await;

    // 'ingesting' rows are excluded from reconcile to avoid races with workers.
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/gone/active.ts', 'active.ts', 'ingesting')"
    )
    .execute(&env.pool).await.unwrap();

    let removed = reconcile_deleted(&env.config, &env.pool, 1)
        .await
        .expect("reconcile");
    assert_eq!(removed, 0, "ingesting rows should not be reconciled");
}

// ── enqueue_missing_pes ───────────────────────────────────────────────────────

#[tokio::test]
async fn enqueue_missing_pes_skips_when_cache_dir_missing() {
    let env = make_env().await;
    // cache_dir does not exist yet.
    let count = enqueue_missing_pes(&env.config, &env.pool)
        .await
        .expect("enqueue");
    assert_eq!(count, 0, "missing cache_dir → skip");
}

#[tokio::test]
async fn enqueue_missing_pes_queues_done_file_without_blob() {
    let env = make_env().await;
    let pool = &env.pool;

    // Create cache_dir so the guard passes.
    std::fs::create_dir_all(&env.config.paths.cache_dir).unwrap();
    // Create a real (empty) TS file so the path-exists check passes.
    let ts_path = create_ts_file(&env, "regen_me.ts");

    let file_id: i64 = sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'regen_me.ts', 'done') RETURNING id")
        .bind(ts_path.to_string_lossy().as_ref())
        .fetch_one(pool).await.unwrap()
        .get(0);

    sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 500, 'txt')",
    )
    .bind(file_id)
    .execute(pool)
    .await
    .unwrap();

    let count = enqueue_missing_pes(&env.config, pool)
        .await
        .expect("enqueue");
    assert_eq!(count, 1, "one file should be queued for PES regen");

    let pes_regen: i64 = sqlx::query_scalar("SELECT pes_regen FROM ts_files WHERE id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(pes_regen, 1, "pes_regen should be set to 1");
}

#[tokio::test]
async fn enqueue_missing_pes_skips_file_with_blob_present() {
    let env = make_env().await;
    let pool = &env.pool;

    std::fs::create_dir_all(&env.config.paths.cache_dir).unwrap();
    let ts_path = create_ts_file(&env, "has_blob.ts");

    let file_id: i64 = sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'has_blob.ts', 'done') RETURNING id")
        .bind(ts_path.to_string_lossy().as_ref())
        .fetch_one(pool).await.unwrap()
        .get(0);

    sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 500, 'txt')",
    )
    .bind(file_id)
    .execute(pool)
    .await
    .unwrap();

    // Create the captions.pes blob so the function sees it as already cached.
    let stem = ts_path.file_stem().unwrap().to_string_lossy();
    let blob_dir = std::path::Path::new(&env.config.paths.cache_dir).join(stem.as_ref());
    std::fs::create_dir_all(&blob_dir).unwrap();
    std::fs::File::create(blob_dir.join("captions.pes")).unwrap();

    let count = enqueue_missing_pes(&env.config, pool)
        .await
        .expect("enqueue");
    assert_eq!(count, 0, "file with existing blob should not be queued");
}

#[tokio::test]
async fn enqueue_missing_pes_skips_done_file_with_no_captions() {
    let env = make_env().await;
    let pool = &env.pool;

    std::fs::create_dir_all(&env.config.paths.cache_dir).unwrap();
    let ts_path = create_ts_file(&env, "no_caps.ts");

    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, 'no_caps.ts', 'done')")
        .bind(ts_path.to_string_lossy().as_ref())
        .execute(pool)
        .await
        .unwrap();

    // No captions → file is excluded by the EXISTS check.
    let count = enqueue_missing_pes(&env.config, pool)
        .await
        .expect("enqueue");
    assert_eq!(count, 0, "file without captions should not be queued");
}

// ── reset_program ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reset_program_resets_all_files_in_program() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let prog_id: i64 = sqlx::query(
        "INSERT INTO programs (title, normalized_title) VALUES ('Test', 'test') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap()
    .get(0);

    let f1 = create_ts_file(&env, "f1.ts");
    let f2 = create_ts_file(&env, "f2.ts");

    let mut file_ids: Vec<i64> = Vec::new();
    for (path, name) in [(&f1, "f1.ts"), (&f2, "f2.ts")] {
        let id: i64 = sqlx::query(
            "INSERT INTO ts_files (path, filename, status, program_id) VALUES (?, ?, 'done', ?) RETURNING id"
        )
        .bind(path.to_string_lossy().as_ref()).bind(name).bind(prog_id)
        .fetch_one(pool).await.unwrap().get(0);
        file_ids.push(id);
    }

    reset_program(pool, prog_id, cache_dir)
        .await
        .expect("reset_program");

    // reset_ts_file sets program_id = NULL, so query by id instead.
    for id in &file_ids {
        let status: String = sqlx::query_scalar("SELECT status FROM ts_files WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(status, "pending", "file id={id} should be reset to pending");
    }
}

#[tokio::test]
async fn reset_program_with_no_files_is_noop() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let prog_id: i64 = sqlx::query(
        "INSERT INTO programs (title, normalized_title) VALUES ('Empty', 'empty') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap()
    .get(0);

    // No ts_files for this program.
    reset_program(pool, prog_id, cache_dir)
        .await
        .expect("reset_program noop");
}

// ── clear_subtitles ───────────────────────────────────────────────────────────

#[tokio::test]
async fn clear_subtitles_deletes_captions_and_tags() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let file_id: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/x.ts', 'x.ts', 'done') RETURNING id"
    )
    .fetch_one(pool).await.unwrap()
    .get(0);

    let cap_id: i64 = sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 500, 'hi') RETURNING id"
    )
    .bind(file_id)
    .fetch_one(pool).await.unwrap()
    .get(0);

    sqlx::query("INSERT INTO tags (caption_id, tag) VALUES (?, 'funny')")
        .bind(cap_id)
        .execute(pool)
        .await
        .unwrap();

    clear_subtitles(pool, file_id, cache_dir)
        .await
        .expect("clear");

    let cap_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captions WHERE ts_file_id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let tag_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE caption_id = ?")
        .bind(cap_id)
        .fetch_one(pool)
        .await
        .unwrap();

    assert_eq!(cap_count, 0, "captions should be cleared");
    assert_eq!(tag_count, 0, "tags should be cleared");
}

#[tokio::test]
async fn clear_subtitles_keeps_ts_file_row_and_status() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let file_id: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/y.ts', 'y.ts', 'done') RETURNING id"
    )
    .fetch_one(pool).await.unwrap()
    .get(0);

    clear_subtitles(pool, file_id, cache_dir)
        .await
        .expect("clear");

    let status: String = sqlx::query_scalar("SELECT status FROM ts_files WHERE id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        status, "done",
        "ts_file row and status should remain intact"
    );
}

#[tokio::test]
async fn clear_subtitles_missing_id_is_noop() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    // Should not error on a missing id.
    clear_subtitles(pool, 9999, cache_dir)
        .await
        .expect("clear noop");
}

// ── delete_ts_file ────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_ts_file_removes_row_captions_and_tags() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let file_id: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status) VALUES ('/nas/del.ts', 'del.ts', 'done') RETURNING id"
    )
    .fetch_one(pool).await.unwrap()
    .get(0);

    let cap_id: i64 = sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 500, 'del') RETURNING id"
    )
    .bind(file_id)
    .fetch_one(pool).await.unwrap()
    .get(0);

    sqlx::query("INSERT INTO tags (caption_id, tag) VALUES (?, 'tag_to_del')")
        .bind(cap_id)
        .execute(pool)
        .await
        .unwrap();

    delete_ts_file(pool, file_id, cache_dir)
        .await
        .expect("delete");

    let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ts_files WHERE id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let cap_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captions WHERE ts_file_id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();

    assert_eq!(row_count, 0, "ts_files row should be deleted");
    assert_eq!(cap_count, 0, "captions should be deleted");
}

// ── reset_ts_file ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn reset_ts_file_clears_metadata_and_sets_pending() {
    let env = make_env().await;
    let pool = &env.pool;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);

    let prog_id: i64 = sqlx::query(
        "INSERT INTO programs (title, normalized_title) VALUES ('P', 'p') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap()
    .get(0);

    let file_id: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status, program_id, episode_number, episode_title)
         VALUES ('/nas/rs.ts', 'rs.ts', 'done', ?, 3, 'ep3') RETURNING id",
    )
    .bind(prog_id)
    .fetch_one(pool)
    .await
    .unwrap()
    .get(0);

    reset_ts_file(pool, file_id, cache_dir)
        .await
        .expect("reset");

    let row = sqlx::query("SELECT status, program_id, episode_number FROM ts_files WHERE id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap();

    let status: String = row.get(0);
    let program_id: Option<i64> = row.get(1);
    let episode_number: Option<i64> = row.get(2);

    assert_eq!(status, "pending");
    assert!(program_id.is_none(), "program_id should be cleared");
    assert!(episode_number.is_none(), "episode_number should be cleared");
}

#[tokio::test]
async fn reset_ts_file_missing_id_returns_error() {
    let env = make_env().await;
    let cache_dir = std::path::Path::new(&env.config.paths.cache_dir);
    let result = reset_ts_file(&env.pool, 9999, cache_dir).await;
    assert!(result.is_err(), "missing id should return an error");
}
