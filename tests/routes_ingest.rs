// Integration tests for /ingest/* routes.
//
// Uses an isolated SQLite DB (via tempfile) and tower::oneshot.
// SQLx non-macro API is used so tests work under SQLX_OFFLINE=true.

mod common;

use common::{get, make_app_seeded, oneshot, post_form};

// ── helpers ──────────────────────────────────────────────────────────────────

async fn insert_ts_file(pool: &sqlx::SqlitePool, path: &str, name: &str, status: &str) -> i64 {
    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, ?, ?)")
        .bind(path)
        .bind(name)
        .bind(status)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn insert_ts_file_with_error(
    pool: &sqlx::SqlitePool,
    path: &str,
    name: &str,
    error_msg: &str,
) -> i64 {
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status, error_msg) VALUES (?, ?, 'error', ?)",
    )
    .bind(path)
    .bind(name)
    .bind(error_msg)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

async fn insert_caption(pool: &sqlx::SqlitePool, file_id: i64, text: &str) -> i64 {
    sqlx::query("INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 0, 500, ?)")
        .bind(file_id)
        .bind(text)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn count_captions(pool: &sqlx::SqlitePool, file_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM captions WHERE ts_file_id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn get_status(pool: &sqlx::SqlitePool, file_id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM ts_files WHERE id = ?")
        .bind(file_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── GET /ingest/status ────────────────────────────────────────────────────────

#[tokio::test]
async fn status_empty_db_returns_200() {
    let app = make_app_seeded().await;
    let (status, body) = oneshot(app.router, get("/ingest/status")).await;
    assert_eq!(status, 200, "body: {body}");
}

#[tokio::test]
async fn status_counts_are_visible_in_page() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/pending.ts", "pending.ts", "pending").await;
    insert_ts_file(pool, "/nas/done.ts", "done.ts", "done").await;

    let (status, body) = oneshot(app.router, get("/ingest/status")).await;
    assert_eq!(status, 200, "body: {body}");
    // Page should render without error and contain status-related content.
    assert!(body.len() > 100, "body should be non-trivially long");
}

#[tokio::test]
async fn status_shows_error_file() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file_with_error(pool, "/nas/bad.ts", "bad.ts", "parse failed").await;

    let (status, body) = oneshot(app.router, get("/ingest/status")).await;
    assert_eq!(status, 200);
    // Either the filename or the error message should be visible.
    assert!(
        body.contains("bad.ts") || body.contains("parse failed"),
        "error file should appear in body, got: {}",
        &body[..body.len().min(500)]
    );
}

#[tokio::test]
async fn status_shows_regenerating_files() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    sqlx::query(
        "INSERT INTO ts_files (path, filename, status, pes_regen) VALUES (?, ?, 'done', 1)",
    )
    .bind("/nas/regen.ts")
    .bind("regen.ts")
    .execute(pool)
    .await
    .unwrap();

    let (status, _body) = oneshot(app.router, get("/ingest/status")).await;
    assert_eq!(status, 200);
}

// ── GET /ingest/files ─────────────────────────────────────────────────────────

#[tokio::test]
async fn files_empty_db_returns_200() {
    let app = make_app_seeded().await;
    let (status, body) = oneshot(app.router, get("/ingest/files")).await;
    assert_eq!(status, 200, "body: {body}");
}

#[tokio::test]
async fn files_lists_inserted_file() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/show.ts", "show.ts", "done").await;

    let (status, body) = oneshot(app.router, get("/ingest/files")).await;
    assert_eq!(status, 200);
    assert!(body.contains("show.ts"), "file should appear in list");
}

#[tokio::test]
async fn files_status_filter_all_shows_every_status() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/a.ts", "a.ts", "pending").await;
    insert_ts_file(pool, "/nas/b.ts", "b.ts", "done").await;
    insert_ts_file_with_error(pool, "/nas/c.ts", "c.ts", "oops").await;

    let (status, body) = oneshot(app.router, get("/ingest/files?status=all")).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("a.ts") && body.contains("b.ts") && body.contains("c.ts"),
        "all files should appear in body"
    );
}

#[tokio::test]
async fn files_status_filter_pending_only() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/p.ts", "p.ts", "pending").await;
    insert_ts_file(pool, "/nas/d.ts", "d.ts", "done").await;

    let (status, body) = oneshot(app.router, get("/ingest/files?status=pending")).await;
    assert_eq!(status, 200);
    assert!(body.contains("p.ts"), "pending file should appear");
    assert!(
        !body.contains("d.ts"),
        "done file should not appear when filtering pending"
    );
}

#[tokio::test]
async fn files_search_query_filters_by_filename() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/magic.ts", "magic.ts", "done").await;
    insert_ts_file(pool, "/nas/other.ts", "other.ts", "done").await;

    let (status, body) = oneshot(app.router, get("/ingest/files?q=magic")).await;
    assert_eq!(status, 200);
    assert!(body.contains("magic.ts"), "searched file should appear");
    assert!(
        !body.contains("other.ts"),
        "non-matching file should be hidden"
    );
}

#[tokio::test]
async fn files_search_with_status_filter_combined() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    insert_ts_file(pool, "/nas/combo_done.ts", "combo_done.ts", "done").await;
    insert_ts_file(pool, "/nas/combo_pend.ts", "combo_pend.ts", "pending").await;

    let (status, body) = oneshot(app.router, get("/ingest/files?q=combo&status=done")).await;
    assert_eq!(status, 200);
    assert!(body.contains("combo_done.ts"), "done match should appear");
    assert!(
        !body.contains("combo_pend.ts"),
        "pending match should not appear when filtering done"
    );
}

#[tokio::test]
async fn files_pagination_page_param() {
    let app = make_app_seeded().await;
    let (s1, _) = oneshot(app.router.clone(), get("/ingest/files?page=0")).await;
    let (s2, _) = oneshot(app.router, get("/ingest/files?page=1")).await;
    assert_eq!(s1, 200);
    assert_eq!(s2, 200);
}

// ── GET /ingest/file/:id ──────────────────────────────────────────────────────

#[tokio::test]
async fn file_detail_missing_id_returns_404() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/ingest/file/9999")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn file_detail_existing_id_returns_200() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let id = insert_ts_file(pool, "/nas/detail.ts", "detail.ts", "done").await;

    let (status, body) = oneshot(app.router, get(&format!("/ingest/file/{id}"))).await;
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("detail.ts"),
        "filename should appear in detail page"
    );
}

#[tokio::test]
async fn file_detail_shows_caption_count() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let file_id = insert_ts_file(pool, "/nas/caps.ts", "caps.ts", "done").await;
    for i in 0..3i64 {
        sqlx::query(
            "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, ?, ?, ?)",
        )
        .bind(file_id)
        .bind(i * 1000)
        .bind(i * 1000 + 500)
        .bind(format!("caption {i}"))
        .execute(pool)
        .await
        .unwrap();
    }

    let (status, body) = oneshot(app.router, get(&format!("/ingest/file/{file_id}"))).await;
    assert_eq!(status, 200);
    assert!(
        body.contains('3'),
        "caption count of 3 should appear in detail page"
    );
}

#[tokio::test]
async fn file_detail_with_program_shows_program_title() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let prog_id: i64 = sqlx::query(
        "INSERT INTO programs (title, normalized_title) VALUES ('DetailProg', 'detailprog')",
    )
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid();

    let file_id: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status, program_id) VALUES (?, ?, 'done', ?)",
    )
    .bind("/nas/dp.ts")
    .bind("dp.ts")
    .bind(prog_id)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid();

    let (status, body) = oneshot(app.router, get(&format!("/ingest/file/{file_id}"))).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("DetailProg"),
        "program title should appear in detail page"
    );
}

// ── POST /ingest/clear/:id ────────────────────────────────────────────────────

#[tokio::test]
async fn clear_existing_file_returns_200() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let file_id = insert_ts_file(pool, "/nas/clr.ts", "clr.ts", "done").await;
    insert_caption(pool, file_id, "clearable").await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/ingest/clear/{file_id}"), ""),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn clear_removes_captions_from_db() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let file_id = insert_ts_file(pool, "/nas/clr2.ts", "clr2.ts", "done").await;
    insert_caption(pool, file_id, "to clear").await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/ingest/clear/{file_id}"), ""),
    )
    .await;
    assert_eq!(status, 200);

    let count = count_captions(pool, file_id).await;
    assert_eq!(count, 0, "captions should be removed after clear");
}

// ── POST /reingest/:id ────────────────────────────────────────────────────────

#[tokio::test]
async fn reingest_existing_file_returns_200() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let file_id = insert_ts_file(pool, "/nas/reingest.ts", "reingest.ts", "done").await;

    let (status, _) = oneshot(app.router, post_form(&format!("/reingest/{file_id}"), "")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn reingest_resets_status_to_pending() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let file_id = insert_ts_file(pool, "/nas/ri2.ts", "ri2.ts", "done").await;

    let (status, _) = oneshot(app.router, post_form(&format!("/reingest/{file_id}"), "")).await;
    assert_eq!(status, 200);

    let st = get_status(pool, file_id).await;
    assert_eq!(st, "pending", "reingest should reset status to pending");
}

#[tokio::test]
async fn reingest_missing_file_returns_500() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, post_form("/reingest/9999", "")).await;
    assert_eq!(status, 500);
}
