use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};

use crate::config::Config;
use crate::ts::subtitle;

/// Compute the MJPEG stream frame indices for `count` evenly-spaced sample
/// points within the caption window.
///
/// - `pts_start_ms` / `pts_end_ms`: caption PTS bounds in milliseconds.
/// - `count`: number of thumbnails to generate.  Returns an empty vec when 0.
/// - `fps`: frame rate of the intermediate MJPEG stream (e.g. 30000/1001).
///
/// Negative relative times are clamped to frame 0 (occurs only when
/// `pts_start_ms` is very small).
fn frame_indices(pts_start_ms: i64, pts_end_ms: i64, count: usize, fps: f64) -> Vec<u64> {
    let pts_start_sec = pts_start_ms as f64 / 1000.0;
    let pts_end_sec   = pts_end_ms   as f64 / 1000.0;
    let pre_seek  = (pts_start_sec - 6.0).max(0.0);
    let win_start = pts_start_sec + 1.5;
    let win_end   = if pts_end_sec > win_start { pts_end_sec } else { win_start + 0.5 };

    (0..count)
        .map(|k| {
            let t = if count <= 1 {
                win_start
            } else {
                win_start + k as f64 * (win_end - win_start) / (count - 1) as f64
            };
            let rel = t - pre_seek;
            // Clamp to 0 before cast: negative rel would silently saturate on cast.
            (rel.max(0.0) * fps).round() as u64
        })
        .collect()
}

fn ts_stem(ts_path: &Path) -> String {
    ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Path where a contact-sheet thumbnail (small, display-only) is cached.
pub fn thumb_path(cache_dir: &Path, stem: &str, id: i64, n: u32) -> PathBuf {
    cache_dir
        .join(stem)
        .join("thumbs")
        .join(format!("{}_{:02}.jpg", id, n))
}

/// Path where a full-resolution download JPEG is cached.
pub fn full_path(cache_dir: &Path, stem: &str, id: i64, n: u32) -> PathBuf {
    cache_dir
        .join(stem)
        .join("full")
        .join(format!("{}_{:02}.jpg", id, n))
}

// ── ffmpeg pipeline ────────────────────────────────────────────────────────

/// Parameters for a single-frame or multi-frame ffmpeg capture.
struct CaptureParams<'a> {
    ts_path: &'a Path,
    /// Pre-seek position (seconds before the window start).
    pre_seek: f64,
    /// Duration to decode from pre_seek.
    dur: f64,
    /// Frame indices (counted from the first decoded frame after pre_seek).
    frame_nums: &'a [u64],
    /// Optional subtitle PNG overlay path.
    sub_png: Option<&'a Path>,
    /// Output width.
    width: u32,
    /// Output height.
    height: u32,
    /// ffmpeg -q:v value (lower = better quality).
    quality: u32,
    /// Output path pattern.  Use `%d` for multi-frame (ffmpeg 1-based), or
    /// a literal path for a single frame.
    out_pattern: &'a str,
}

/// Run the ffmpeg pipeline described by `p` and return the raw output.
///
/// Filter chain:
///   bwdif=mode=send_frame, scale=WxH, setsar=1, select='eq(n,X)+…', setpts=N/FRAME_RATE/TB
///
/// bwdif=mode=send_frame deinterlaces terrestrial 1080i sources without
/// changing the frame count (1 input frame → 1 output frame), so the
/// frame_indices / select='eq(n,X)' approach remains valid.
///
/// When a subtitle PNG is provided it is scaled to the output dimensions
/// before being overlaid (required when the PNG was rendered at full
/// resolution but the output is a smaller thumbnail).
fn run_ffmpeg(p: &CaptureParams<'_>) -> Result<std::process::Output> {
    let input_url = format!("file:{}", p.ts_path.to_str().unwrap_or(""));
    let pre_seek_str = format!("{:.6}", p.pre_seek);
    let dur_str      = format!("{:.6}", p.dur);
    let q_str        = p.quality.to_string();

    let select_expr = p.frame_nums
        .iter()
        .map(|n| format!("eq(n\\,{})", n))
        .collect::<Vec<_>>()
        .join("+");

    let out = if let Some(sub) = p.sub_png {
        let sub_str = sub.to_str().unwrap_or("");
        // Scale the subtitle PNG to the output dimensions in case it was
        // rendered at full resolution (1920×1080) but the target is smaller.
        let filter = format!(
            "[0:v]bwdif=mode=send_frame,scale={}:{},setsar=1,select='{}',setpts=N/FRAME_RATE/TB[v];\
             [1:v]scale={}:{}[s];\
             [v][s]overlay=eof_action=repeat[out]",
            p.width, p.height, select_expr,
            p.width, p.height,
        );
        Command::new("ffmpeg")
            .args([
                "-y",
                "-ss", &pre_seek_str,
                "-t",  &dur_str,
                "-i",  &input_url,
                "-i",  sub_str,
                "-filter_complex", &filter,
                "-map", "[out]",
                "-fps_mode", "vfr",
                "-q:v", &q_str,
                p.out_pattern,
            ])
            .output()?
    } else {
        let vf = format!(
            "bwdif=mode=send_frame,scale={}:{},setsar=1,select='{}',setpts=N/FRAME_RATE/TB",
            p.width, p.height, select_expr
        );
        Command::new("ffmpeg")
            .args([
                "-y",
                "-ss", &pre_seek_str,
                "-t",  &dur_str,
                "-i",  &input_url,
                "-vf", &vf,
                "-fps_mode", "vfr",
                "-q:v", &q_str,
                p.out_pattern,
            ])
            .output()?
    };

    Ok(out)
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Generate contact-sheet thumbnails (small, display-only) for all frames.
///
/// Output: `cache/{stem}/thumbs/{id}_{n:02}.jpg`
/// Resolution: `cfg.thumb_width × cfg.thumb_height`
/// Quality: `cfg.thumb_quality`
pub fn ensure_thumbnails(
    cfg: &Config,
    ts_path: &Path,
    id: i64,
    pts_start_ms: i64,
    pts_end_ms: i64,
) -> Result<()> {
    let stem = ts_stem(ts_path);
    let cache_dir = Path::new(&cfg.paths.cache_dir);
    let count = cfg.capture.thumb_count as usize;

    // Skip if every thumbnail is already cached.
    if (0..count as u32).all(|n| thumb_path(cache_dir, &stem, id, n).exists()) {
        return Ok(());
    }

    let thumbs_dir = cache_dir.join(&stem).join("thumbs");
    std::fs::create_dir_all(&thumbs_dir)?;

    let pts_start_sec = pts_start_ms as f64 / 1000.0;
    let pts_end_sec   = pts_end_ms   as f64 / 1000.0;

    let pre_seek  = (pts_start_sec - 6.0).max(0.0);
    let win_start = pts_start_sec + 1.5;
    let win_end   = if pts_end_sec > win_start { pts_end_sec } else { win_start + 0.5 };
    let dur       = (win_end - pre_seek) + 0.5;

    // Terrestrial broadcast: 29.97 fps = 30000/1001.
    let fps = 30_000.0_f64 / 1001.0;
    let frame_nums = frame_indices(pts_start_ms, pts_end_ms, count, fps);

    let sub_png_opt = subtitle::ensure_caption_png(
        &cfg.capture, cache_dir, ts_path, id, pts_start_ms,
    )?;

    let tmp_pattern = thumbs_dir.join("_tmp_%d.jpg");
    let tmp_str = tmp_pattern.to_str().unwrap_or("").to_string();

    let out = run_ffmpeg(&CaptureParams {
        ts_path,
        pre_seek,
        dur,
        frame_nums: &frame_nums,
        sub_png: sub_png_opt.as_deref(),
        width:   cfg.capture.thumb_width,
        height:  cfg.capture.thumb_height,
        quality: cfg.capture.thumb_quality,
        out_pattern: &tmp_str,
    })?;

    if !out.status.success() {
        bail!(
            "thumbnail pipeline failed for {}:\n  exit: {}\n  stderr:\n{}",
            ts_path.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Rename _tmp_{1..count}.jpg → {id}_{n:02}.jpg (ffmpeg 1-based → our 0-based n).
    for n in 0..count {
        let tmp = thumbs_dir.join(format!("_tmp_{}.jpg", n + 1));
        let dst = thumb_path(cache_dir, &stem, id, n as u32);
        if tmp.exists() {
            std::fs::rename(&tmp, &dst)?;
        } else {
            tracing::warn!(
                "thumbnail pipeline: expected {} but not found (caption {}, frame {})",
                tmp.display(),
                id,
                n,
            );
        }
    }

    Ok(())
}

/// Generate a single full-resolution JPEG for download / share.
///
/// Output: `cache/{stem}/full/{id}_{n:02}.jpg`
/// Resolution: `cfg.width × cfg.height` (full, e.g. 1920×1080)
/// Quality: `cfg.jpeg_quality`
///
/// Only the requested frame `n` is generated; other frames are not touched.
pub fn ensure_full(
    cfg: &Config,
    ts_path: &Path,
    id: i64,
    pts_start_ms: i64,
    pts_end_ms: i64,
    n: u32,
) -> Result<()> {
    let stem = ts_stem(ts_path);
    let cache_dir = Path::new(&cfg.paths.cache_dir);

    let dst = full_path(cache_dir, &stem, id, n);
    if dst.exists() {
        return Ok(());
    }

    let full_dir = cache_dir.join(&stem).join("full");
    std::fs::create_dir_all(&full_dir)?;

    let count = cfg.capture.thumb_count as usize;
    let pts_start_sec = pts_start_ms as f64 / 1000.0;
    let pts_end_sec   = pts_end_ms   as f64 / 1000.0;

    let pre_seek  = (pts_start_sec - 6.0).max(0.0);
    let win_start = pts_start_sec + 1.5;
    let win_end   = if pts_end_sec > win_start { pts_end_sec } else { win_start + 0.5 };
    let dur       = (win_end - pre_seek) + 0.5;

    let fps = 30_000.0_f64 / 1001.0;
    let all_frames = frame_indices(pts_start_ms, pts_end_ms, count, fps);

    // Select only frame n from the pre-computed sequence.
    let frame_num = all_frames.get(n as usize).copied().unwrap_or_else(|| {
        tracing::warn!("full: frame index {} out of range for caption {}, using 0", n, id);
        0
    });

    let sub_png_opt = subtitle::ensure_caption_png(
        &cfg.capture, cache_dir, ts_path, id, pts_start_ms,
    )?;

    let dst_str = dst.to_str().unwrap_or("").to_string();

    let out = run_ffmpeg(&CaptureParams {
        ts_path,
        pre_seek,
        dur,
        frame_nums: &[frame_num],
        sub_png: sub_png_opt.as_deref(),
        width:   cfg.capture.width,
        height:  cfg.capture.height,
        quality: cfg.capture.jpeg_quality,
        out_pattern: &dst_str,
    })?;

    if !out.status.success() {
        bail!(
            "full-resolution pipeline failed for {} frame {}:\n  exit: {}\n  stderr:\n{}",
            ts_path.display(),
            n,
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // ffmpeg outputs _tmp_1.jpg when out_pattern contains %d, but here we
    // pass a literal dst path (single frame), so ffmpeg writes directly.
    // Verify the file was created.
    if !dst.exists() {
        bail!(
            "full-resolution pipeline produced no output for caption {} frame {}",
            id,
            n,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{frame_indices, full_path, thumb_path, ts_stem};
    use std::path::Path;

    const FPS: f64 = 30_000.0 / 1001.0;

    // ── thumb_path ─────────────────────────────────────────────────────────────

    #[test]
    fn thumb_path_format() {
        let p = thumb_path(Path::new("/cache"), "ep01", 42, 3);
        assert_eq!(p, Path::new("/cache/ep01/thumbs/42_03.jpg"));
    }

    #[test]
    fn thumb_path_zero_padded_n() {
        // n is zero-padded to 2 digits
        let p = thumb_path(Path::new("/cache"), "ep01", 1, 0);
        assert_eq!(p, Path::new("/cache/ep01/thumbs/1_00.jpg"));
    }

    // ── full_path ──────────────────────────────────────────────────────────────

    #[test]
    fn full_path_format() {
        let p = full_path(Path::new("/cache"), "ep01", 42, 3);
        assert_eq!(p, Path::new("/cache/ep01/full/42_03.jpg"));
    }

    #[test]
    fn full_path_zero_padded_n() {
        let p = full_path(Path::new("/cache"), "ep01", 1, 0);
        assert_eq!(p, Path::new("/cache/ep01/full/1_00.jpg"));
    }

    // ── ts_stem ────────────────────────────────────────────────────────────────

    #[test]
    fn ts_stem_normal() {
        assert_eq!(ts_stem(Path::new("/nas/video/ep01.ts")), "ep01");
    }

    #[test]
    fn ts_stem_no_extension() {
        assert_eq!(ts_stem(Path::new("/nas/video/ep01")), "ep01");
    }

    #[test]
    fn ts_stem_no_file_component_fallback() {
        // An empty path has no file_stem → falls back to "unknown"
        assert_eq!(ts_stem(Path::new("")), "unknown");
    }

    // ── frame_indices ──────────────────────────────────────────────────────────

    #[test]
    fn frame_indices_empty_when_count_zero() {
        assert!(frame_indices(10_000, 15_000, 0, FPS).is_empty());
    }

    #[test]
    fn frame_indices_count_one() {
        let frames = frame_indices(10_000, 15_000, 1, FPS);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn frame_indices_count_three_sorted() {
        let frames = frame_indices(10_000, 20_000, 3, FPS);
        assert_eq!(frames.len(), 3);
        // Frames should be non-decreasing (even sample spacing)
        assert!(frames[0] <= frames[1] && frames[1] <= frames[2]);
    }

    #[test]
    fn frame_indices_first_and_last_differ_for_wide_window() {
        // With a 5-second window and 3 frames, first and last must differ.
        let frames = frame_indices(10_000, 15_000, 3, FPS);
        assert!(frames[0] < frames[2], "first and last frames should differ");
    }

    #[test]
    fn frame_indices_negative_pts_clamps_to_zero() {
        // Very negative PTS → relative time is negative → frame index must be 0, not panic.
        let frames = frame_indices(-10_000, -5_000, 3, FPS);
        for f in &frames {
            assert_eq!(*f, 0, "frame index must clamp to 0 for negative relative time");
        }
    }

    #[test]
    fn frame_indices_zero_pts() {
        // pts_start=0: pre_seek clamped to 0, win_start=1.5s → positive relative time
        let frames = frame_indices(0, 5_000, 3, FPS);
        assert!(frames[0] > 0);
    }
}
