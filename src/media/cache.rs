//! Image-cache maintenance: size accounting, LRU cleanup, and manual clearing.
//!
//! All functions in this module operate only on the per-TS image
//! subdirectories listed in [`IMAGE_SUBDIRS`]. The `captions.pes` blob lives
//! next to them in `cache/{stem}/` and is never counted or deleted here —
//! images are cheap to regenerate on demand, the PES blob is not.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

/// Cache subdirectories (under `cache/{stem}/`) holding regenerable images.
pub const IMAGE_SUBDIRS: [&str; 4] = ["thumbs", "full", "preview", "sub"];

/// Resolve the per-TS cache subdirectory from the stored path string.
///
/// Returns `None` when the path has no file stem (e.g. dotfiles, directory paths)
/// to prevent `remove_dir_all` from targeting the cache root.
pub fn cache_subtree(cache_dir: &Path, ts_path_str: &str) -> Option<PathBuf> {
    let stem = Path::new(ts_path_str).file_stem()?;
    let stem_str = stem.to_string_lossy();
    if stem_str.is_empty() {
        return None;
    }
    Some(cache_dir.join(stem_str.as_ref()))
}

/// Collect every image-cache file as (path, size, mtime).
///
/// The layout is flat (`cache/{stem}/{subdir}/{file}`), so only two directory
/// levels are walked. A missing cache dir yields an empty list.
fn image_files(cache_dir: &Path) -> Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(_) => return Ok(files), // cache dir absent (e.g. never created)
    };

    for stem_entry in entries.filter_map(Result::ok) {
        if !stem_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        for sub in IMAGE_SUBDIRS {
            let sub_dir = stem_entry.path().join(sub);
            let Ok(sub_entries) = fs::read_dir(&sub_dir) else {
                continue;
            };
            for f in sub_entries.filter_map(Result::ok) {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((f.path(), meta.len(), mtime));
            }
        }
    }
    Ok(files)
}

/// Total (file_count, bytes) of all image caches under `cache_dir`.
pub fn image_cache_stats(cache_dir: &Path) -> Result<(u64, u64)> {
    let files = image_files(cache_dir)?;
    let bytes = files.iter().map(|(_, size, _)| size).sum();
    Ok((files.len() as u64, bytes))
}

/// Delete the oldest image-cache files (by mtime) until the total size is at
/// most `max_bytes`. Returns the number of bytes freed.
///
/// Emptied image subdirectories are removed as well. Deletion failures on
/// individual files are logged and skipped so one bad file cannot wedge the
/// whole cleanup.
pub fn enforce_image_cache_limit(cache_dir: &Path, max_bytes: u64) -> Result<u64> {
    let mut files = image_files(cache_dir)?;
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    if total <= max_bytes {
        return Ok(0);
    }

    // Oldest first (LRU by mtime).
    files.sort_by_key(|(_, _, mtime)| *mtime);

    let mut freed: u64 = 0;
    for (path, size, _) in files {
        if total <= max_bytes {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                total -= size;
                freed += size;
                // Drop the parent subdir once empty; ignore "not empty" races.
                if let Some(parent) = path.parent() {
                    let _ = fs::remove_dir(parent);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "image cache cleanup: failed to remove {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }
    Ok(freed)
}

/// Delete all image caches for one TS file, keeping `captions.pes` intact.
pub fn clear_image_cache_for(cache_dir: &Path, ts_path_str: &str) -> Result<u64> {
    let Some(subtree) = cache_subtree(cache_dir, ts_path_str) else {
        return Ok(0);
    };
    clear_image_subdirs(&subtree)
}

/// Delete all image caches for every TS file, keeping `captions.pes` blobs
/// intact. Returns the number of bytes freed.
pub fn clear_all_image_cache(cache_dir: &Path) -> Result<u64> {
    let entries = match fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };

    let mut freed = 0u64;
    for stem_entry in entries.filter_map(Result::ok) {
        if stem_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            freed += clear_image_subdirs(&stem_entry.path())?;
        }
    }
    Ok(freed)
}

/// Remove the image subdirectories under one `cache/{stem}/` tree.
/// Returns the number of bytes freed.
fn clear_image_subdirs(subtree: &Path) -> Result<u64> {
    let mut freed = 0u64;
    for sub in IMAGE_SUBDIRS {
        let dir = subtree.join(sub);
        if !dir.exists() {
            continue;
        }
        // Sum sizes before removal for reporting.
        if let Ok(entries) = fs::read_dir(&dir) {
            for f in entries.filter_map(Result::ok) {
                if let Ok(meta) = f.metadata() {
                    if meta.is_file() {
                        freed += meta.len();
                    }
                }
            }
        }
        fs::remove_dir_all(&dir)?;
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    /// Create a file with `size` bytes and an mtime `age_secs` in the past.
    fn make_file(path: &Path, size: usize, age_secs: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = File::create(path).unwrap();
        f.write_all(&vec![0u8; size]).unwrap();
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        f.set_modified(mtime).unwrap();
    }

    /// Standard fixture: one TS stem with images in every subdir + captions.pes.
    fn seed_stem(cache: &Path, stem: &str) {
        make_file(&cache.join(stem).join("thumbs/1_00.jpg"), 100, 300);
        make_file(&cache.join(stem).join("full/1_00.jpg"), 400, 200);
        make_file(&cache.join(stem).join("preview/1.jpg"), 50, 100);
        make_file(&cache.join(stem).join("sub/1.png"), 30, 50);
        make_file(&cache.join(stem).join("captions.pes"), 999, 400);
    }

    // ── cache_subtree (moved from ingest.rs) ──────────────────────────────────

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
        assert!(cache_subtree(cache, "/").is_none());
    }

    #[test]
    fn cache_subtree_never_returns_cache_root() {
        // Empty string has no stem → must not return the cache dir itself
        let cache = Path::new("/cache");
        assert!(cache_subtree(cache, "").is_none());
    }

    // ── image_cache_stats ─────────────────────────────────────────────────────

    #[test]
    fn stats_counts_images_but_not_pes() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");

        let (files, bytes) = image_cache_stats(dir.path()).unwrap();
        assert_eq!(files, 4, "captions.pes must not be counted");
        assert_eq!(bytes, 100 + 400 + 50 + 30);
    }

    #[test]
    fn stats_missing_cache_dir_is_empty() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(image_cache_stats(&missing).unwrap(), (0, 0));
    }

    // ── enforce_image_cache_limit ─────────────────────────────────────────────

    #[test]
    fn enforce_under_limit_deletes_nothing() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");

        let freed = enforce_image_cache_limit(dir.path(), 10_000).unwrap();
        assert_eq!(freed, 0);
        let (files, _) = image_cache_stats(dir.path()).unwrap();
        assert_eq!(files, 4);
    }

    #[test]
    fn enforce_deletes_oldest_first_until_under_limit() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");
        // Total images = 580 bytes. Cap at 100:
        // deletion order by age: thumbs(100,oldest) → full(400) → preview(50) → sub(30)
        // after thumbs+full: 80 <= 100 → stop. preview + sub survive.
        let freed = enforce_image_cache_limit(dir.path(), 100).unwrap();
        assert_eq!(freed, 500);

        assert!(!dir.path().join("ep01/thumbs/1_00.jpg").exists());
        assert!(!dir.path().join("ep01/full/1_00.jpg").exists());
        assert!(dir.path().join("ep01/preview/1.jpg").exists());
        assert!(dir.path().join("ep01/sub/1.png").exists());
        // The PES blob is never deleted.
        assert!(dir.path().join("ep01/captions.pes").exists());
    }

    #[test]
    fn enforce_zero_limit_deletes_all_images_but_keeps_pes() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");

        let freed = enforce_image_cache_limit(dir.path(), 0).unwrap();
        assert_eq!(freed, 580);
        let (files, bytes) = image_cache_stats(dir.path()).unwrap();
        assert_eq!((files, bytes), (0, 0));
        assert!(dir.path().join("ep01/captions.pes").exists());
    }

    #[test]
    fn enforce_removes_emptied_subdirs() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");

        enforce_image_cache_limit(dir.path(), 0).unwrap();
        for sub in IMAGE_SUBDIRS {
            assert!(
                !dir.path().join("ep01").join(sub).exists(),
                "{sub} should be removed once empty"
            );
        }
    }

    // ── clear_image_cache_for / clear_all_image_cache ─────────────────────────

    #[test]
    fn clear_for_removes_images_keeps_pes() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");
        seed_stem(dir.path(), "ep02");

        let freed = clear_image_cache_for(dir.path(), "/nas/ep01.ts").unwrap();
        assert_eq!(freed, 580);

        assert!(!dir.path().join("ep01/thumbs").exists());
        assert!(dir.path().join("ep01/captions.pes").exists());
        // Other stems untouched.
        assert!(dir.path().join("ep02/thumbs/1_00.jpg").exists());
    }

    #[test]
    fn clear_for_stemless_path_is_noop() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");
        assert_eq!(clear_image_cache_for(dir.path(), "/").unwrap(), 0);
        assert!(dir.path().join("ep01/thumbs/1_00.jpg").exists());
    }

    #[test]
    fn clear_all_removes_all_images_keeps_all_pes() {
        let dir = TempDir::new().unwrap();
        seed_stem(dir.path(), "ep01");
        seed_stem(dir.path(), "ep02");

        let freed = clear_all_image_cache(dir.path()).unwrap();
        assert_eq!(freed, 580 * 2);

        let (files, _) = image_cache_stats(dir.path()).unwrap();
        assert_eq!(files, 0);
        assert!(dir.path().join("ep01/captions.pes").exists());
        assert!(dir.path().join("ep02/captions.pes").exists());
    }

    #[test]
    fn clear_all_missing_cache_dir_is_noop() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(clear_all_image_cache(&missing).unwrap(), 0);
    }
}
