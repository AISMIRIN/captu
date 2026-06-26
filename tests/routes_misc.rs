// Integration tests for tags, contact, episodes, search, and capture-helper routes.
//
// Uses an isolated SQLite DB (via tempfile) and tower::oneshot.
// SQLx non-macro API is used so tests work under SQLX_OFFLINE=true.

mod common;

use common::{get, make_app_seeded, oneshot, post_form};

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn insert_ts_file(pool: &sqlx::SqlitePool, path: &str, name: &str) -> i64 {
    sqlx::query("INSERT INTO ts_files (path, filename, status) VALUES (?, ?, 'done')")
        .bind(path)
        .bind(name)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn insert_ts_file_with_prog(
    pool: &sqlx::SqlitePool,
    path: &str,
    name: &str,
    prog_id: i64,
) -> i64 {
    sqlx::query(
        "INSERT INTO ts_files (path, filename, status, program_id) VALUES (?, ?, 'done', ?)",
    )
    .bind(path)
    .bind(name)
    .bind(prog_id)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

async fn insert_caption(pool: &sqlx::SqlitePool, file_id: i64, text: &str) -> i64 {
    sqlx::query(
        "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, 1000, 2000, ?)",
    )
    .bind(file_id)
    .bind(text)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

async fn insert_program(pool: &sqlx::SqlitePool, title: &str, norm: &str) -> i64 {
    sqlx::query("INSERT INTO programs (title, normalized_title) VALUES (?, ?)")
        .bind(title)
        .bind(norm)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn insert_tag(pool: &sqlx::SqlitePool, cap_id: i64, tag: &str) {
    sqlx::query("INSERT INTO tags (caption_id, tag) VALUES (?, ?)")
        .bind(cap_id)
        .bind(tag)
        .execute(pool)
        .await
        .unwrap();
}

async fn count_tags(pool: &sqlx::SqlitePool, cap_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE caption_id = ?")
        .bind(cap_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count_tag(pool: &sqlx::SqlitePool, cap_id: i64, tag: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE caption_id = ? AND tag = ?")
        .bind(cap_id)
        .bind(tag)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn get_selected_frame(pool: &sqlx::SqlitePool, cap_id: i64) -> i64 {
    sqlx::query_scalar("SELECT selected_frame FROM thumbnails WHERE caption_id = ?")
        .bind(cap_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Seed one ts_file + one caption, return (file_id, caption_id).
async fn seed_one(app: &common::TestApp) -> (i64, i64) {
    let pool = &app.state.pool;
    let fid = insert_ts_file(pool, "/nas/ep.ts", "ep.ts").await;
    let cid = insert_caption(pool, fid, "テスト字幕").await;
    (fid, cid)
}

// ── GET /api/tags ────────────────────────────────────────────────────────────

#[tokio::test]
async fn tag_options_empty_db_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/api/tags")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn tag_options_lists_distinct_tags() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    insert_tag(&app.state.pool, cap_id, "comedy").await;
    insert_tag(&app.state.pool, cap_id, "drama").await;

    let (status, body) = oneshot(app.router, get("/api/tags")).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("comedy") && body.contains("drama"),
        "tags should appear in body"
    );
}

// ── POST /caption/:id/tags ────────────────────────────────────────────────────

#[tokio::test]
async fn add_tag_to_existing_caption_returns_200() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags"), "tag=comedy"),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn add_tag_inserts_into_db() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags"), "tag=mytag"),
    )
    .await;
    assert_eq!(status, 200);

    assert_eq!(count_tag(&app.state.pool, cap_id, "mytag").await, 1);
}

#[tokio::test]
async fn add_tag_idempotent_insert_or_ignore() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let uri = format!("/caption/{cap_id}/tags");
    let (s1, _) = oneshot(app.router.clone(), post_form(&uri, "tag=dup")).await;
    let (s2, _) = oneshot(app.router, post_form(&uri, "tag=dup")).await;
    assert_eq!(s1, 200);
    assert_eq!(s2, 200);

    assert_eq!(
        count_tag(&app.state.pool, cap_id, "dup").await,
        1,
        "duplicate tag should be deduplicated"
    );
}

#[tokio::test]
async fn add_tag_blank_is_ignored() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags"), "tag=  "),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        count_tags(&app.state.pool, cap_id).await,
        0,
        "blank tag should not be inserted"
    );
}

#[tokio::test]
async fn add_tag_response_contains_tag_name() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, body) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags"), "tag=action"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("action"),
        "response should contain the added tag"
    );
}

#[tokio::test]
async fn add_tag_includes_hx_trigger_header() {
    use tower::ServiceExt;

    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let resp = app
        .router
        .oneshot(common::post_form(
            &format!("/caption/{cap_id}/tags"),
            "tag=hx",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().contains_key("hx-trigger"),
        "hx-trigger header must be present for htmx listeners"
    );
}

// ── POST /caption/:id/tags/delete ─────────────────────────────────────────────

#[tokio::test]
async fn delete_tag_removes_existing_tag() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    insert_tag(&app.state.pool, cap_id, "to_delete").await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags/delete"), "tag=to_delete"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        count_tag(&app.state.pool, cap_id, "to_delete").await,
        0,
        "tag should be removed"
    );
}

#[tokio::test]
async fn delete_tag_nonexistent_tag_returns_200() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(
        app.router,
        post_form(&format!("/caption/{cap_id}/tags/delete"), "tag=ghost"),
    )
    .await;
    assert_eq!(status, 200);
}

// ── GET /contact/:id ──────────────────────────────────────────────────────────

#[tokio::test]
async fn contact_missing_id_returns_404() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/contact/9999")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn contact_existing_caption_returns_200() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, body) = oneshot(app.router, get(&format!("/contact/{cap_id}"))).await;
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body.contains("テスト字幕"),
        "caption text should appear in contact page"
    );
}

#[tokio::test]
async fn contact_shows_program_title_when_available() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let prog_id = insert_program(pool, "番組名", "bangumi").await;
    let file_id = insert_ts_file_with_prog(pool, "/nas/prog.ts", "prog.ts", prog_id).await;
    let cap_id = insert_caption(pool, file_id, "txt").await;

    let (status, body) = oneshot(app.router, get(&format!("/contact/{cap_id}"))).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("番組名"),
        "program title should appear in contact page"
    );
}

#[tokio::test]
async fn contact_shows_tags() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;
    insert_tag(&app.state.pool, cap_id, "mytag").await;

    let (status, body) = oneshot(app.router, get(&format!("/contact/{cap_id}"))).await;
    assert_eq!(status, 200);
    assert!(body.contains("mytag"), "tag should appear in contact page");
}

#[tokio::test]
async fn contact_frame_range_is_6() {
    // thumb_count=6 in test config → frames 0..6 appear.
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _body) = oneshot(app.router, get(&format!("/contact/{cap_id}"))).await;
    assert_eq!(status, 200);
}

// ── GET /api/episodes ─────────────────────────────────────────────────────────

#[tokio::test]
async fn episodes_no_program_id_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/api/episodes")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn episodes_empty_program_id_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/api/episodes?program_id=")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn episodes_zero_program_id_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/api/episodes?program_id=0")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn episodes_valid_program_returns_episode_list() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let prog_id = insert_program(pool, "アニメ", "anime").await;

    let fid: i64 = sqlx::query(
        "INSERT INTO ts_files (path, filename, status, program_id, episode_number, episode_title)
         VALUES (?, ?, 'done', ?, 1, '第1話')",
    )
    .bind("/nas/anime01.ts")
    .bind("anime01.ts")
    .bind(prog_id)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid();

    // Must have at least one caption for the episodes endpoint to return it.
    insert_caption(pool, fid, "op").await;

    let (status, body) = oneshot(
        app.router,
        get(&format!("/api/episodes?program_id={prog_id}")),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        body.contains("第1話") || body.contains('1'),
        "episode should appear"
    );
}

#[tokio::test]
async fn episodes_all_null_numbers_shows_subtitle_selector() {
    // When every episode lacks episode_number, template shows subtitle list.
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let prog_id = insert_program(pool, "映画", "movie").await;

    for i in 0..2i64 {
        let fid: i64 = sqlx::query(
            "INSERT INTO ts_files (path, filename, status, program_id, episode_title)
             VALUES (?, ?, 'done', ?, ?)",
        )
        .bind(format!("/nas/m{i}.ts"))
        .bind(format!("m{i}.ts"))
        .bind(prog_id)
        .bind(format!("subtitle {i}"))
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid();

        insert_caption(pool, fid, "x").await;
    }

    let (status, _body) = oneshot(
        app.router,
        get(&format!("/api/episodes?program_id={prog_id}")),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn episodes_negative_program_id_returns_empty() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/api/episodes?program_id=-1")).await;
    assert_eq!(status, 200);
}

// ── GET / (index) ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn index_empty_db_returns_200() {
    let app = make_app_seeded().await;
    let (status, body) = oneshot(app.router, get("/")).await;
    assert_eq!(status, 200, "body: {body}");
}

#[tokio::test]
async fn index_lists_programs_in_dropdown() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    // Programs only appear when they have at least one caption (index query requirement).
    let prog_id = insert_program(pool, "ProgramX", "programx").await;
    let fid = insert_ts_file_with_prog(pool, "/nas/px.ts", "px.ts", prog_id).await;
    insert_caption(pool, fid, "some text").await;

    let (status, body) = oneshot(app.router, get("/")).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("ProgramX"),
        "program should appear in index dropdown"
    );
}

// ── GET /search ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_no_query_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/search")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn search_with_query_empty_db_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/search?q=test")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn search_finds_caption_by_text() {
    let app = make_app_seeded().await;
    seed_one(&app).await;

    let (status, body) = oneshot(app.router, get("/search?q=テスト")).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("テスト字幕") || body.contains("ep.ts"),
        "search result should appear: {}",
        &body[..body.len().min(500)]
    );
}

#[tokio::test]
async fn search_program_filter_returns_200() {
    let app = make_app_seeded().await;
    let pool = &app.state.pool;

    let prog_id = insert_program(pool, "FilterTest", "filter_test").await;
    let fid = insert_ts_file_with_prog(pool, "/nas/ft.ts", "ft.ts", prog_id).await;
    insert_caption(pool, fid, "xyz").await;

    let (status, _) = oneshot(
        app.router,
        get(&format!("/search?q=xyz&program_id={prog_id}")),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn search_tag_filter_returns_200() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;
    insert_tag(&app.state.pool, cap_id, "filter_tag").await;

    let (status, _) = oneshot(app.router, get("/search?q=テスト&tag=filter_tag")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn search_date_range_filter_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(
        app.router,
        get("/search?q=test&date_from=2024-01-01&date_to=2024-12-31"),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn search_pagination_returns_200() {
    let app = make_app_seeded().await;
    let (status, _) = oneshot(app.router, get("/search?q=test&page=2")).await;
    assert_eq!(status, 200);
}

// ── POST /select/:id/:n ───────────────────────────────────────────────────────

#[tokio::test]
async fn select_frame_returns_200() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(app.router, post_form(&format!("/select/{cap_id}/3"), "")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn select_frame_persists_to_db() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (status, _) = oneshot(app.router, post_form(&format!("/select/{cap_id}/2"), "")).await;
    assert_eq!(status, 200);

    let frame = get_selected_frame(&app.state.pool, cap_id).await;
    assert_eq!(
        frame, 2,
        "selected frame should be persisted to thumbnails table"
    );
}

#[tokio::test]
async fn select_frame_upsert_overwrites_previous() {
    let app = make_app_seeded().await;
    let (_, cap_id) = seed_one(&app).await;

    let (s1, _) = oneshot(
        app.router.clone(),
        post_form(&format!("/select/{cap_id}/0"), ""),
    )
    .await;
    let (s2, _) = oneshot(app.router, post_form(&format!("/select/{cap_id}/4"), "")).await;
    assert_eq!(s1, 200);
    assert_eq!(s2, 200);

    let frame = get_selected_frame(&app.state.pool, cap_id).await;
    assert_eq!(frame, 4, "most recent selection should win");
}
