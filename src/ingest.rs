use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::{NaiveDateTime, Utc};
use sqlx::SqlitePool;
use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::ts::{epg, pes as arib_pes, subtitle};

/// Scan nas_mount, apply filters, and enqueue new files as 'pending'.
/// Returns the number of newly queued files.
pub async fn scan_and_enqueue(config: &Config, pool: &SqlitePool) -> Result<usize> {
    let pattern = format!("{}/{}", config.paths.nas_mount, config.paths.ts_glob);
    tracing::info!("ingest scan: pattern = {}", pattern);

    let paths: Vec<PathBuf> = glob::glob(&pattern)?
        .filter_map(Result::ok)
        .collect();
    tracing::info!("ingest scan: {} TS files found", paths.len());

    // Reconcile before enqueueing: remove DB rows whose .ts no longer exists on disk.
    // Pass the glob count so reconcile_deleted can guard against NAS unmount (glob 0).
    let removed = reconcile_deleted(config, pool, paths.len()).await?;
    if removed > 0 {
        tracing::info!("ingest scan: {} stale DB rows removed", removed);
    }

    // Files already processed don't need re-queueing.
    let skip: HashSet<String> = sqlx::query_scalar(
        "SELECT path FROM ts_files WHERE status IN ('done', 'error', 'ingesting')",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    tracing::info!("ingest scan: {} already processed, skipping", skip.len());

    // Pre-compile filter patterns.
    let exclude_patterns: Vec<glob::Pattern> = config
        .ingest
        .filter_exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    let include_patterns: Vec<glob::Pattern> = config
        .ingest
        .filter_include
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    let mut queued = 0usize;

    for path in paths {
        let path_str = path.to_string_lossy().to_string();

        if skip.contains(&path_str) {
            continue;
        }

        // Exclude pattern check.
        if exclude_patterns.iter().any(|p| p.matches(&path_str)) {
            tracing::debug!("ingest filter (excluded): {}", path_str);
            continue;
        }

        // Include pattern check: empty list means allow all.
        if !include_patterns.is_empty()
            && !include_patterns.iter().any(|p| p.matches(&path_str))
        {
            tracing::debug!("ingest filter (not included): {}", path_str);
            continue;
        }

        // Optional: skip files with no ARIB caption PID.
        // find_caption_pid reads PMT-range packets only (~100-1000 × 188 B), fast over NAS.
        if config.ingest.require_captions {
            let path_c = path.clone();
            let has_cap = tokio::task::spawn_blocking(move || {
                arib_pes::find_caption_pid(&path_c).is_some()
            })
            .await
            .unwrap_or(false);

            if !has_cap {
                tracing::info!("ingest skip (no ARIB caption PID): {}", path_str);
                continue;
            }
        }

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let affected = sqlx::query(
            "INSERT OR IGNORE INTO ts_files (path, filename, status) VALUES (?, ?, 'pending')",
        )
        .bind(&path_str)
        .bind(&filename)
        .execute(pool)
        .await?
        .rows_affected();

        // Only count and log genuinely new rows (0 = path already in DB).
        if affected > 0 {
            tracing::info!("ingest queued: {}", filename);
            queued += 1;
        }
    }

    tracing::info!("ingest scan: {} new files queued", queued);
    Ok(queued)
}

/// Start `config.ingest.concurrency` parallel workers and wait for all pending files.
pub async fn run_workers(config: Arc<Config>, pool: SqlitePool) -> Result<()> {
    let concurrency = config.ingest.concurrency.max(1) as usize;
    tracing::info!("ingest: starting {} worker(s)", concurrency);

    let mut handles = Vec::with_capacity(concurrency);
    for worker_id in 0..concurrency {
        let cfg = config.clone();
        let p = pool.clone();
        handles.push(tokio::spawn(async move {
            worker_loop(worker_id, cfg, p).await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    tracing::info!("ingest: all workers done");
    Ok(())
}

/// Convenience wrapper: scan → enqueue → run workers.
pub async fn scan_and_ingest(config: Arc<Config>, pool: SqlitePool) -> Result<()> {
    scan_and_enqueue(&config, &pool).await?;
    run_workers(config, pool).await
}

/// Each worker atomically claims one pending file at a time and processes it.
async fn worker_loop(worker_id: usize, config: Arc<Config>, pool: SqlitePool) {
    loop {
        // Atomically move one 'pending' row to 'ingesting'.
        // SQLite evaluates the subquery and WHERE atomically, so concurrent
        // workers cannot claim the same file.
        let row: Option<(i64, String)> = match sqlx::query_as(
            "UPDATE ts_files SET status = 'ingesting'
             WHERE id = (SELECT id FROM ts_files WHERE status = 'pending' LIMIT 1)
               AND status = 'pending'
             RETURNING id, path",
        )
        .fetch_optional(&pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("[worker {}] claim error: {:#}", worker_id, e);
                break;
            }
        };

        match row {
            Some((ts_file_id, path_str)) => {
                let path = PathBuf::from(&path_str);
                tracing::info!("[worker {}] ingest start: {}", worker_id, path_str);
                if let Err(e) = do_ingest(&path, &config, &pool, ts_file_id).await {
                    tracing::error!(
                        "[worker {}] ingest error on {}: {:#}",
                        worker_id,
                        path_str,
                        e
                    );
                    let msg = format!("{:#}", e);
                    let _ = sqlx::query(
                        "UPDATE ts_files SET status = 'error', error_msg = ? WHERE id = ?",
                    )
                    .bind(&msg)
                    .bind(ts_file_id)
                    .execute(&pool)
                    .await;
                }
            }
            None => {
                tracing::debug!("[worker {}] no more pending files, exiting", worker_id);
                break;
            }
        }
    }
}

async fn do_ingest(
    path: &Path,
    config: &Config,
    pool: &SqlitePool,
    ts_file_id: i64,
) -> Result<()> {
    let path_buf = path.to_path_buf();
    let cache_dir = PathBuf::from(&config.paths.cache_dir);

    // Parse PAT + PMT once to get both caption service IDs and the caption PES PID.
    // This avoids re-reading the TS header in the two extraction steps below.
    let psi = {
        let p = path_buf.clone();
        tokio::task::spawn_blocking(move || arib_pes::scan_psi(&p)).await?
    };

    // Run EIT scan (EPG) and PES demux (captions) in parallel on separate
    // blocking threads.  Neither depends on the other's result.
    let epg_task = {
        let p = path_buf.clone();
        let svcs = psi.caption_services.clone();
        tokio::task::spawn_blocking(move || epg::extract_epg(&p, &svcs))
    };
    let cap_task = {
        let p = path_buf.clone();
        let c = cache_dir.clone();
        let pid = psi.caption_pid;
        tokio::task::spawn_blocking(move || subtitle::extract_captions(&p, &c, pid))
    };
    let (epg_res, cap_res) = tokio::join!(epg_task, cap_task);
    let epg = epg_res??;
    let captions = cap_res??;

    // Resolve program_id using series_title so episodes of the same series
    // share one programs row. Fall back to the raw title when no series separator.
    let program_key = if !epg.series_title.is_empty() {
        epg.series_title.clone()
    } else if !epg.title.is_empty() && epg.title != "(unknown)" {
        epg.title.clone()
    } else {
        String::new()
    };

    let program_id: Option<i64> = if !program_key.is_empty() {
        let normalized = normalize_title(&program_key);
        sqlx::query("INSERT OR IGNORE INTO programs (title, normalized_title) VALUES (?, ?)")
            .bind(&program_key)
            .bind(&normalized)
            .execute(pool)
            .await?;

        let id: i64 = sqlx::query_scalar("SELECT id FROM programs WHERE title = ?")
            .bind(&program_key)
            .fetch_one(pool)
            .await?;
        Some(id)
    } else {
        None
    };

    let episode_number = epg.episode_number.map(|n| n as i64);
    let episode_title = epg.sub_title.as_deref().map(str::to_string);
    let air_date = epg.air_datetime.map(|dt| dt.date_naive().to_string());
    let ingested_at: NaiveDateTime = Utc::now().naive_utc();

    sqlx::query(
        "UPDATE ts_files
         SET status = 'done', ingested_at = ?, program_id = ?,
             episode_number = ?, episode_title = ?, air_date = ?
         WHERE id = ?",
    )
    .bind(ingested_at)
    .bind(program_id)
    .bind(episode_number)
    .bind(episode_title)
    .bind(air_date)
    .bind(ts_file_id)
    .execute(pool)
    .await?;

    // Insert all captions in a single transaction.
    let mut tx = pool.begin().await?;
    for cap in &captions {
        sqlx::query(
            "INSERT INTO captions (ts_file_id, pts_start, pts_end, text) VALUES (?, ?, ?, ?)",
        )
        .bind(ts_file_id)
        .bind(cap.pts_start_ms)
        .bind(cap.pts_end_ms)
        .bind(&cap.text)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    tracing::info!(
        "ingest done: {} | {} captions | program_id={:?}",
        path.display(),
        captions.len(),
        program_id,
    );

    Ok(())
}

/// Permanently remove a ts_file and everything derived from it:
/// tags, captions (+ FTS rows via captions_ad trigger), thumbnails (via ON DELETE CASCADE),
/// the cache directory, and the ts_files row itself.
pub async fn delete_ts_file(pool: &SqlitePool, ts_file_id: i64, cache_dir: &Path) -> Result<()> {
    // Fetch path first — needed for cache dir removal.
    let path_str: Option<String> =
        sqlx::query_scalar("SELECT path FROM ts_files WHERE id = ?")
            .bind(ts_file_id)
            .fetch_optional(pool)
            .await?;

    let path_str = match path_str {
        Some(p) => p,
        None => {
            tracing::warn!("delete_ts_file: id={} not found, skipping", ts_file_id);
            return Ok(());
        }
    };

    // tags has no ON DELETE CASCADE, so delete before captions.
    sqlx::query("DELETE FROM tags WHERE caption_id IN (SELECT id FROM captions WHERE ts_file_id = ?)")
        .bind(ts_file_id)
        .execute(pool)
        .await?;

    // captions_ad trigger removes captions from captions_fts; thumbnails cascade automatically.
    sqlx::query("DELETE FROM captions WHERE ts_file_id = ?")
        .bind(ts_file_id)
        .execute(pool)
        .await?;

    // Remove cache subtree for this TS (subtitle PNGs + contact-sheet JPEGs).
    // cache_subtree returns None when path_str has no file stem, preventing
    // remove_dir_all from targeting the cache root.
    if let Some(cache_path) = cache_subtree(cache_dir, &path_str) {
        if cache_path.exists() {
            std::fs::remove_dir_all(&cache_path)?;
        }
    }

    sqlx::query("DELETE FROM ts_files WHERE id = ?")
        .bind(ts_file_id)
        .execute(pool)
        .await?;

    tracing::info!("deleted ts_file id={} ({})", ts_file_id, path_str);
    Ok(())
}

/// Remove DB rows whose source .ts no longer exists on disk.
///
/// `disk_file_count` is the number of .ts found by the current glob scan.
/// When 0 — which happens when the NAS is unmounted but its mountpoint directory
/// still exists — the reconcile is skipped entirely to prevent mass-deleting the DB.
/// `nas_mount` not existing on disk is treated the same way.
///
/// `ingesting` rows are excluded to avoid racing with active workers.
/// Returns the number of rows removed.
pub async fn reconcile_deleted(
    config: &Config,
    pool: &SqlitePool,
    disk_file_count: usize,
) -> Result<usize> {
    // Guard 1: nas_mount does not exist (e.g. never mounted).
    if !Path::new(&config.paths.nas_mount).exists() {
        tracing::warn!(
            "reconcile: nas_mount '{}' does not exist — skipping reconcile",
            config.paths.nas_mount
        );
        return Ok(0);
    }

    // Guard 2: glob returned 0 files — most likely an NAS unmount whose mountpoint
    // directory still exists but is empty.
    if disk_file_count == 0 {
        tracing::warn!(
            "reconcile: glob returned 0 .ts files — skipping reconcile to avoid mass-delete \
             (NAS may be unmounted)"
        );
        return Ok(0);
    }

    // Collect all trackable rows: done / error / pending.  Exclude ingesting.
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, path FROM ts_files WHERE status IN ('done', 'error', 'pending')",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Check path existence in a blocking thread (many stat(2) calls).
    let missing_ids: Vec<i64> = tokio::task::spawn_blocking(move || {
        rows.into_iter()
            .filter(|(_, path)| !Path::new(path).exists())
            .map(|(id, _)| id)
            .collect()
    })
    .await?;

    let cache_dir = PathBuf::from(&config.paths.cache_dir);
    let count = missing_ids.len();

    for id in missing_ids {
        delete_ts_file(pool, id, &cache_dir).await?;
    }

    // Purge programs that are no longer referenced by any ts_file.
    if count > 0 {
        sqlx::query(
            "DELETE FROM programs \
             WHERE id NOT IN \
               (SELECT DISTINCT program_id FROM ts_files WHERE program_id IS NOT NULL)",
        )
        .execute(pool)
        .await?;
    }

    Ok(count)
}

/// Reset a single TS file: delete captions (FTS synced via captions_ad trigger),
/// clear metadata, and set status back to 'pending'.
pub async fn reset_ts_file(
    pool: &SqlitePool,
    ts_file_id: i64,
    cache_dir: &Path,
) -> Result<()> {
    let path_str: Option<String> =
        sqlx::query_scalar("SELECT path FROM ts_files WHERE id = ?")
            .bind(ts_file_id)
            .fetch_optional(pool)
            .await?;

    let path_str =
        path_str.ok_or_else(|| anyhow::anyhow!("ts_file id={} not found", ts_file_id))?;

    // Delete captions; the captions_ad trigger removes them from captions_fts.
    sqlx::query("DELETE FROM captions WHERE ts_file_id = ?")
        .bind(ts_file_id)
        .execute(pool)
        .await?;

    // Clear metadata and reset to 'pending'.
    sqlx::query(
        "UPDATE ts_files
         SET status = 'pending', error_msg = NULL, ingested_at = NULL,
             program_id = NULL, episode_number = NULL, episode_title = NULL, air_date = NULL
         WHERE id = ?",
    )
    .bind(ts_file_id)
    .execute(pool)
    .await?;

    // Remove cached subtitle / thumbnail files for this TS.
    if let Some(cache_path) = cache_subtree(cache_dir, &path_str) {
        if cache_path.exists() {
            std::fs::remove_dir_all(&cache_path)?;
        }
    }

    tracing::info!("reset: ts_file id={} ({})", ts_file_id, path_str);
    Ok(())
}

/// Reset all TS files belonging to a program.
pub async fn reset_program(
    pool: &SqlitePool,
    program_id: i64,
    cache_dir: &Path,
) -> Result<()> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM ts_files WHERE program_id = ?")
            .bind(program_id)
            .fetch_all(pool)
            .await?;

    let count = ids.len();
    for id in ids {
        reset_ts_file(pool, id, cache_dir).await?;
    }

    tracing::info!("reset: program id={} ({} files reset)", program_id, count);
    Ok(())
}

fn normalize_title(title: &str) -> String {
    // NFKC normalization: full-width ASCII → half-width, compatibility equivalents unified
    title.nfkc().collect::<String>().to_lowercase()
}

/// Resolve the per-TS cache subdirectory from the stored path string.
///
/// Returns `None` when the path has no file stem (e.g. dotfiles, directory paths)
/// to prevent `remove_dir_all` from targeting the cache root.
fn cache_subtree(cache_dir: &Path, ts_path_str: &str) -> Option<PathBuf> {
    let stem = Path::new(ts_path_str).file_stem()?;
    let stem_str = stem.to_string_lossy();
    if stem_str.is_empty() {
        return None;
    }
    Some(cache_dir.join(stem_str.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::{cache_subtree, normalize_title};
    use std::path::Path;

    // ── normalize_title ────────────────────────────────────────────────────────

    #[test]
    fn normalize_title_fullwidth_to_halfwidth() {
        // Full-width ASCII digits/letters should become half-width
        assert_eq!(normalize_title("ＡＢＣ１２３"), "abc123");
    }

    #[test]
    fn normalize_title_lowercase() {
        assert_eq!(normalize_title("ABC"), "abc");
    }

    #[test]
    fn normalize_title_mixed() {
        // NFKC converts both full-width letters and ideographic space (U+3000) to half-width
        assert_eq!(normalize_title("ＳＨＩＲＯ　the hero"), "shiro the hero");
    }

    #[test]
    fn normalize_title_idempotent() {
        let s = "アニメ abc 123";
        assert_eq!(normalize_title(&normalize_title(s)), normalize_title(s));
    }

    #[test]
    fn normalize_title_empty() {
        assert_eq!(normalize_title(""), "");
    }

    // ── cache_subtree ──────────────────────────────────────────────────────────

    #[test]
    fn cache_subtree_normal_path() {
        let cache = Path::new("/cache");
        let result = cache_subtree(cache, "/nas/video/ep01.ts");
        assert_eq!(result, Some(Path::new("/cache/ep01").to_path_buf()));
    }

    #[test]
    fn cache_subtree_no_extension() {
        let cache = Path::new("/cache");
        let result = cache_subtree(cache, "/nas/video/ep01");
        assert_eq!(result, Some(Path::new("/cache/ep01").to_path_buf()));
    }

    #[test]
    fn cache_subtree_no_stem_returns_none() {
        // Root path "/" has no file component at all → file_stem() = None → returns None
        let cache = Path::new("/cache");
        let result = cache_subtree(cache, "/");
        assert!(result.is_none(), "root-only path must return None");
    }

    #[test]
    fn cache_subtree_never_returns_cache_root() {
        // Empty string has no stem → must not return the cache dir itself
        let cache = Path::new("/cache");
        let result = cache_subtree(cache, "");
        assert!(result.is_none());
    }
}
