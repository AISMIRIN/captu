use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use glob;

use crate::config::Config;
use crate::ts::subtitle;

/// Compute the capture window for a caption, in seconds.
///
/// Returns `(pre_seek, win_start, win_end)`:
/// - `pre_seek`: decode start position (6 s before the caption, clamped to 0)
/// - `win_start`: first sample point (1.5 s after the caption start)
/// - `win_end`: last sample point (caption end, or win_start + 0.5 s minimum)
fn caption_window(pts_start_ms: i64, pts_end_ms: i64) -> (f64, f64, f64) {
    // Cap the upper end.  A corrupt PTS would otherwise yield an `-ss` argument
    // of ~2e14 seconds, which makes ffmpeg seek past EOF and emit no frames.
    // Only the upper bound is capped: the low end is already handled by the
    // `.max(0.0)` on `pre_seek` below and by `frame_indices`, and clamping it
    // here would change how small/negative timestamps behave.  Capping silently
    // keeps this a total function; callers that must reject such captions
    // outright do so before reaching the capture pipeline.
    let max = crate::ts::pts::MAX_PLAUSIBLE_PTS_MS;
    let pts_start_sec = pts_start_ms.min(max) as f64 / 1000.0;
    let pts_end_sec = pts_end_ms.min(max) as f64 / 1000.0;
    let pre_seek = (pts_start_sec - 6.0).max(0.0);
    let win_start = pts_start_sec + 1.5;
    let win_end = if pts_end_sec > win_start {
        pts_end_sec
    } else {
        win_start + 0.5
    };
    (pre_seek, win_start, win_end)
}

/// Wall-clock time (seconds) of the `k`-th of `count` evenly-spaced sample
/// points within `[win_start, win_end]`.
fn sample_time(win_start: f64, win_end: f64, k: usize, count: usize) -> f64 {
    if count <= 1 {
        win_start
    } else {
        win_start + k as f64 * (win_end - win_start) / (count - 1) as f64
    }
}

/// Wall-clock target time (seconds) of the representative preview frame:
/// the middle sample point of the contact-sheet sequence.
fn preview_target_sec(pts_start_ms: i64, pts_end_ms: i64, count: usize) -> f64 {
    let (_, win_start, win_end) = caption_window(pts_start_ms, pts_end_ms);
    sample_time(win_start, win_end, count / 2, count)
}

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
    let (pre_seek, win_start, win_end) = caption_window(pts_start_ms, pts_end_ms);

    (0..count)
        .map(|k| {
            let rel = sample_time(win_start, win_end, k, count) - pre_seek;
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

/// Path where a subtitle-free single-frame preview JPEG is cached.
///
/// Used by search results before full contact-sheet thumbnails are generated.
pub fn preview_path(cache_dir: &Path, stem: &str, id: i64) -> PathBuf {
    cache_dir
        .join(stem)
        .join("preview")
        .join(format!("{}.jpg", id))
}

/// Remove cached thumbs/full JPEGs and the subtitle PNG for one caption,
/// forcing regeneration on the next /thumb or /full request.
///
/// The `thumbnails` table row is intentionally left intact so the user's
/// frame selection is preserved across the recapture.
pub fn clear_caption_cache(cache_dir: &Path, stem: &str, id: i64) -> Result<()> {
    // Remove thumbs/{id}_*.jpg
    let thumb_pattern = cache_dir
        .join(stem)
        .join("thumbs")
        .join(format!("{}_*.jpg", id));
    for entry in glob::glob(thumb_pattern.to_str().unwrap_or(""))?.flatten() {
        std::fs::remove_file(&entry)?;
    }

    // Remove full/{id}_*.jpg
    let full_pattern = cache_dir
        .join(stem)
        .join("full")
        .join(format!("{}_*.jpg", id));
    for entry in glob::glob(full_pattern.to_str().unwrap_or(""))?.flatten() {
        std::fs::remove_file(&entry)?;
    }

    // Remove sub/{id}.png
    let sub_png = cache_dir.join(stem).join("sub").join(format!("{}.png", id));
    if sub_png.exists() {
        std::fs::remove_file(&sub_png)?;
    }

    // Remove preview/{id}.jpg
    let preview = preview_path(cache_dir, stem, id);
    if preview.exists() {
        std::fs::remove_file(&preview)?;
    }

    Ok(())
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

/// Build the ffmpeg `select` filter expression for the given frame indices.
///
/// Returns a string like `eq(n\,0)+eq(n\,3)+eq(n\,7)` (backslash-escaped comma
/// required by the ffmpeg filter syntax).  Returns an empty string for an empty
/// frame list, which would produce an invalid select filter — callers are expected
/// to guard against count=0 before reaching this point.
fn build_select_expr(frame_nums: &[u64]) -> String {
    frame_nums
        .iter()
        .map(|n| format!("eq(n\\,{})", n))
        .collect::<Vec<_>>()
        .join("+")
}

/// Build the complete ffmpeg argument list from `p`.
///
/// `-ss` is placed before `-i` for fast NAS-based seek (CLAUDE.md convention).
/// When a subtitle PNG is provided, a `-filter_complex` overlay pipeline is used;
/// otherwise a simpler `-vf` chain suffices.
fn build_ffmpeg_args(p: &CaptureParams<'_>) -> Vec<String> {
    let input_url = format!("file:{}", p.ts_path.to_str().unwrap_or(""));
    let pre_seek_str = format!("{:.6}", p.pre_seek);
    let dur_str = format!("{:.6}", p.dur);
    let q_str = p.quality.to_string();
    let select_expr = build_select_expr(p.frame_nums);
    // Stop decoding once the last selected frame has been emitted instead of
    // running to the end of `-t dur`.
    let frames_str = p.frame_nums.len().to_string();

    if let Some(sub) = p.sub_png {
        let sub_str = sub.to_str().unwrap_or("").to_string();
        // bwdif runs before select so its temporal references stay intact;
        // scale runs after select so only the selected frames are scaled.
        // Scale the subtitle PNG to the output dimensions in case it was
        // rendered at full resolution (1920×1080) but the target is smaller.
        let filter = format!(
            "[0:v]bwdif=mode=send_frame,select='{}',setpts=N/FRAME_RATE/TB,scale={}:{},setsar=1[v];\
             [1:v]scale={}:{}[s];\
             [v][s]overlay=eof_action=repeat[out]",
            select_expr,
            p.width,
            p.height,
            p.width,
            p.height,
        );
        vec![
            "-y".into(),
            "-ss".into(),
            pre_seek_str,
            "-t".into(),
            dur_str,
            "-i".into(),
            input_url,
            "-i".into(),
            sub_str,
            "-filter_complex".into(),
            filter,
            "-map".into(),
            "[out]".into(),
            "-fps_mode".into(),
            "vfr".into(),
            "-frames:v".into(),
            frames_str,
            "-q:v".into(),
            q_str,
            p.out_pattern.into(),
        ]
    } else {
        let vf = format!(
            "bwdif=mode=send_frame,select='{}',setpts=N/FRAME_RATE/TB,scale={}:{},setsar=1",
            select_expr, p.width, p.height
        );
        vec![
            "-y".into(),
            "-ss".into(),
            pre_seek_str,
            "-t".into(),
            dur_str,
            "-i".into(),
            input_url,
            "-vf".into(),
            vf,
            "-fps_mode".into(),
            "vfr".into(),
            "-frames:v".into(),
            frames_str,
            "-q:v".into(),
            q_str,
            p.out_pattern.into(),
        ]
    }
}

/// Build the ffmpeg argument list for a single-frame direct-seek capture
/// (subtitle-free preview).
///
/// Unlike `build_ffmpeg_args`, this seeks straight to the target time and
/// grabs the first frame there (`-frames:v 1`), so ffmpeg only decodes from
/// the preceding keyframe — a few frames instead of the whole caption window.
/// The resulting frame may differ from contact-sheet frame n by a few frames,
/// which is invisible at preview size and acceptable for display-only use.
///
/// `-ss` is placed before `-i` for fast NAS-based seek (CLAUDE.md convention).
fn build_preview_args(
    ts_path: &Path,
    target_sec: f64,
    width: u32,
    height: u32,
    quality: u32,
    out_path: &str,
) -> Vec<String> {
    let input_url = format!("file:{}", ts_path.to_str().unwrap_or(""));
    // bwdif on a lone frame falls back to spatial interpolation, which is
    // indistinguishable at thumbnail size.
    let vf = format!("bwdif=mode=send_frame,scale={}:{},setsar=1", width, height);
    vec![
        "-y".into(),
        "-ss".into(),
        format!("{:.6}", target_sec),
        "-i".into(),
        input_url,
        "-vf".into(),
        vf,
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        quality.to_string(),
        out_path.into(),
    ]
}

/// Run the ffmpeg pipeline described by `p` and return the raw output.
///
/// Filter chain:
///   bwdif=mode=send_frame, select='eq(n,X)+…', setpts=N/FRAME_RATE/TB, scale=WxH, setsar=1
///
/// bwdif=mode=send_frame deinterlaces terrestrial 1080i sources without
/// changing the frame count (1 input frame → 1 output frame), so the
/// frame_indices / select='eq(n,X)' approach remains valid.  bwdif runs
/// before select (temporal references intact); scale runs after select so
/// only the selected frames pay the scaling cost.  `-frames:v N` stops the
/// decode as soon as the last selected frame has been emitted.
///
/// When a subtitle PNG is provided it is scaled to the output dimensions
/// before being overlaid (required when the PNG was rendered at full
/// resolution but the output is a smaller thumbnail).
// Spawns a real ffmpeg process: requires a TS file on NAS and the ffmpeg binary.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
fn run_ffmpeg(p: &CaptureParams<'_>) -> Result<std::process::Output> {
    let args = build_ffmpeg_args(p);
    Ok(Command::new("ffmpeg").args(&args).output()?)
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Generate contact-sheet thumbnails (small, display-only) for all frames.
///
/// Output: `cache/{stem}/thumbs/{id}_{n:02}.jpg`
/// Resolution: `cfg.thumb_width × cfg.thumb_height`
/// Quality: `cfg.thumb_quality`
// Requires a real TS file and ffmpeg; delegates to run_ffmpeg.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
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

    let (pre_seek, _, win_end) = caption_window(pts_start_ms, pts_end_ms);
    let dur = (win_end - pre_seek) + 0.5;

    // Terrestrial broadcast: 29.97 fps = 30000/1001.
    let fps = 30_000.0_f64 / 1001.0;
    let frame_nums = frame_indices(pts_start_ms, pts_end_ms, count, fps);

    let sub_png_opt =
        subtitle::ensure_caption_png(&cfg.capture, cache_dir, ts_path, id, pts_start_ms)?;

    let tmp_pattern = thumbs_dir.join("_tmp_%d.jpg");
    let tmp_str = tmp_pattern.to_str().unwrap_or("").to_string();

    let out = run_ffmpeg(&CaptureParams {
        ts_path,
        pre_seek,
        dur,
        frame_nums: &frame_nums,
        sub_png: sub_png_opt.as_deref(),
        width: cfg.capture.thumb_width,
        height: cfg.capture.thumb_height,
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
// Requires a real TS file and ffmpeg; delegates to run_ffmpeg.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
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
    let (pre_seek, _, win_end) = caption_window(pts_start_ms, pts_end_ms);
    let dur = (win_end - pre_seek) + 0.5;

    let fps = 30_000.0_f64 / 1001.0;
    let all_frames = frame_indices(pts_start_ms, pts_end_ms, count, fps);

    // Select only frame n from the pre-computed sequence.
    let frame_num = all_frames.get(n as usize).copied().unwrap_or_else(|| {
        tracing::warn!(
            "full: frame index {} out of range for caption {}, using 0",
            n,
            id
        );
        0
    });

    let sub_png_opt =
        subtitle::ensure_caption_png(&cfg.capture, cache_dir, ts_path, id, pts_start_ms)?;

    let dst_str = dst.to_str().unwrap_or("").to_string();

    let out = run_ffmpeg(&CaptureParams {
        ts_path,
        pre_seek,
        dur,
        frame_nums: &[frame_num],
        sub_png: sub_png_opt.as_deref(),
        width: cfg.capture.width,
        height: cfg.capture.height,
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

/// Generate a subtitle-free single-frame preview JPEG for search result display.
///
/// Output: `cache/{stem}/preview/{id}.jpg`
/// Resolution: `cfg.thumb_width × cfg.thumb_height` (same as contact-sheet thumbs)
/// Quality: `cfg.thumb_quality`
///
/// Unlike `ensure_thumbnails`, no subtitle PNG is overlaid and ffmpeg seeks
/// directly to the representative sample time instead of decoding the whole
/// caption window — only a few frames (from the preceding keyframe) are
/// decoded, keeping on-demand generation from search results fast.  The frame
/// may differ from contact-sheet frame n by a few frames, which is invisible
/// at preview size.  When the full contact-sheet thumbnails are later
/// generated via `ensure_thumbnails`, the search page naturally switches to
/// the subtitle-composited `/thumb` URL on the next render.
// Requires a real TS file and ffmpeg.
// Confirmed separately (integration / manual). Not included in the coverage gate.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn ensure_preview(
    cfg: &Config,
    ts_path: &Path,
    id: i64,
    pts_start_ms: i64,
    pts_end_ms: i64,
) -> Result<()> {
    let stem = ts_stem(ts_path);
    let cache_dir = Path::new(&cfg.paths.cache_dir);

    let dst = preview_path(cache_dir, &stem, id);
    if dst.exists() {
        return Ok(());
    }

    let preview_dir = cache_dir.join(&stem).join("preview");
    std::fs::create_dir_all(&preview_dir)?;

    let count = cfg.capture.thumb_count as usize;
    let target_sec = preview_target_sec(pts_start_ms, pts_end_ms, count);

    let dst_str = dst.to_str().unwrap_or("").to_string();
    let args = build_preview_args(
        ts_path,
        target_sec,
        cfg.capture.thumb_width,
        cfg.capture.thumb_height,
        cfg.capture.thumb_quality,
        &dst_str,
    );

    let out = Command::new("ffmpeg").args(&args).output()?;

    if !out.status.success() {
        bail!(
            "preview pipeline failed for {} (caption {}):\n  exit: {}\n  stderr:\n{}",
            ts_path.display(),
            id,
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }

    if !dst.exists() {
        bail!("preview pipeline produced no output for caption {}", id,);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_ffmpeg_args, build_preview_args, build_select_expr, caption_window, frame_indices,
        full_path, preview_path, preview_target_sec, thumb_path, ts_stem, CaptureParams,
    };
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

    // ── preview_path ───────────────────────────────────────────────────────────

    #[test]
    fn preview_path_format() {
        let p = preview_path(Path::new("/cache"), "ep01", 42);
        assert_eq!(p, Path::new("/cache/ep01/preview/42.jpg"));
    }

    #[test]
    fn preview_path_different_ids() {
        let p1 = preview_path(Path::new("/cache"), "ep01", 1);
        let p2 = preview_path(Path::new("/cache"), "ep01", 999);
        assert_eq!(p1, Path::new("/cache/ep01/preview/1.jpg"));
        assert_eq!(p2, Path::new("/cache/ep01/preview/999.jpg"));
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
            assert_eq!(
                *f, 0,
                "frame index must clamp to 0 for negative relative time"
            );
        }
    }

    #[test]
    fn frame_indices_zero_pts() {
        // pts_start=0: pre_seek clamped to 0, win_start=1.5s → positive relative time
        let frames = frame_indices(0, 5_000, 3, FPS);
        assert!(frames[0] > 0);
    }

    // ── caption_window / preview_target_sec ────────────────────────────────────

    #[test]
    fn caption_window_normal() {
        let (pre_seek, win_start, win_end) = caption_window(10_000, 20_000);
        assert!((pre_seek - 4.0).abs() < 1e-9);
        assert!((win_start - 11.5).abs() < 1e-9);
        assert!((win_end - 20.0).abs() < 1e-9);
    }

    #[test]
    fn caption_window_pre_seek_clamped_to_zero() {
        // pts_start < 6 s → pre_seek clamps to 0
        let (pre_seek, _, _) = caption_window(2_000, 8_000);
        assert_eq!(pre_seek, 0.0);
    }

    #[test]
    fn caption_window_short_caption_min_width() {
        // Caption ends before win_start → window widens to win_start + 0.5
        let (_, win_start, win_end) = caption_window(10_000, 10_500);
        assert!((win_end - (win_start + 0.5)).abs() < 1e-9);
    }

    #[test]
    fn caption_window_clamps_huge_pts() {
        // The corrupt value produced by the pre-fix 33-bit underflow would give
        // `-ss 204963822940342`, which makes ffmpeg emit no frames.
        let max_sec = crate::ts::pts::MAX_PLAUSIBLE_PTS_MS as f64 / 1000.0;
        let (pre_seek, win_start, win_end) = caption_window(204_963_822_946_342_000, i64::MAX);
        for v in [pre_seek, win_start, win_end] {
            assert!(v.is_finite(), "component must stay finite");
            assert!(
                v >= 0.0 && v <= max_sec + 2.0,
                "component out of range: {v}"
            );
        }
        assert!(pre_seek <= max_sec);
    }

    #[test]
    fn frame_indices_huge_pts_bounded() {
        let idx = frame_indices(204_963_822_946_342_000, i64::MAX, 6, 30.0);
        let limit = (crate::ts::pts::MAX_PLAUSIBLE_PTS_MS / 1000) as u64 * 30;
        assert!(
            idx.iter().all(|&i| i <= limit),
            "frame index escaped: {idx:?}"
        );
    }

    #[test]
    fn preview_target_sec_huge_pts_bounded() {
        let t = preview_target_sec(204_963_822_946_342_000, i64::MAX, 6);
        let max_sec = crate::ts::pts::MAX_PLAUSIBLE_PTS_MS as f64 / 1000.0;
        assert!(
            t.is_finite() && t >= 0.0 && t <= max_sec + 2.0,
            "target: {t}"
        );
    }

    #[test]
    fn preview_target_is_middle_sample() {
        // count=6 → rep_idx=3 of samples over [11.5, 20.0]
        let t = preview_target_sec(10_000, 20_000, 6);
        let expected = 11.5 + 3.0 * (20.0 - 11.5) / 5.0;
        assert!((t - expected).abs() < 1e-9);
    }

    #[test]
    fn preview_target_count_one_is_win_start() {
        let t = preview_target_sec(10_000, 20_000, 1);
        assert!((t - 11.5).abs() < 1e-9);
    }

    // ── build_select_expr ──────────────────────────────────────────────────────

    #[test]
    fn select_expr_empty_frames() {
        // Empty frame list produces an empty string (no valid select filter)
        assert_eq!(build_select_expr(&[]), "");
    }

    #[test]
    fn select_expr_single_frame() {
        assert_eq!(build_select_expr(&[5]), "eq(n\\,5)");
    }

    #[test]
    fn select_expr_multiple_frames_joined_with_plus() {
        assert_eq!(
            build_select_expr(&[0, 3, 7]),
            "eq(n\\,0)+eq(n\\,3)+eq(n\\,7)"
        );
    }

    // ── build_ffmpeg_args ──────────────────────────────────────────────────────

    #[test]
    fn ffmpeg_args_ss_before_input() {
        // -ss must appear before -i to enable fast NAS-based seek (CLAUDE.md rule)
        let ts = Path::new("/mnt/video.ts");
        let p = CaptureParams {
            ts_path: ts,
            pre_seek: 10.0,
            dur: 5.0,
            frame_nums: &[0],
            sub_png: None,
            width: 640,
            height: 360,
            quality: 4,
            out_pattern: "/tmp/out.jpg",
        };
        let args = build_ffmpeg_args(&p);
        let ss_pos = args
            .iter()
            .position(|a| a == "-ss")
            .expect("-ss must be present");
        let i_pos = args
            .iter()
            .position(|a| a == "-i")
            .expect("-i must be present");
        assert!(ss_pos < i_pos, "-ss must come before -i");
    }

    #[test]
    fn ffmpeg_args_no_sub_uses_vf() {
        let ts = Path::new("/mnt/video.ts");
        let p = CaptureParams {
            ts_path: ts,
            pre_seek: 10.0,
            dur: 5.0,
            frame_nums: &[0, 3],
            sub_png: None,
            width: 640,
            height: 360,
            quality: 4,
            out_pattern: "/tmp/out%d.jpg",
        };
        let args = build_ffmpeg_args(&p);
        assert!(
            args.contains(&"-vf".to_string()),
            "no-sub path must use -vf"
        );
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert!(!args.contains(&"-i".to_string().repeat(2))); // only one -i
        let vf_idx = args.iter().position(|a| a == "-vf").unwrap();
        assert!(
            args[vf_idx + 1].contains("eq(n\\,0)+eq(n\\,3)"),
            "select expr must be embedded in -vf"
        );
        // Decode stops after the last selected frame
        let frames_idx = args
            .iter()
            .position(|a| a == "-frames:v")
            .expect("-frames:v must be present");
        assert_eq!(args[frames_idx + 1], "2");
    }

    #[test]
    fn ffmpeg_args_filter_order_bwdif_select_scale() {
        // bwdif must run before select (temporal references intact) and scale
        // after select (only selected frames are scaled).
        let p = CaptureParams {
            ts_path: Path::new("/mnt/video.ts"),
            pre_seek: 10.0,
            dur: 5.0,
            frame_nums: &[0, 3],
            sub_png: None,
            width: 640,
            height: 360,
            quality: 4,
            out_pattern: "/tmp/out%d.jpg",
        };
        let args = build_ffmpeg_args(&p);
        let vf_idx = args.iter().position(|a| a == "-vf").unwrap();
        let vf = &args[vf_idx + 1];
        let bwdif_pos = vf.find("bwdif").expect("bwdif in filter");
        let select_pos = vf.find("select").expect("select in filter");
        let scale_pos = vf.find("scale").expect("scale in filter");
        assert!(bwdif_pos < select_pos, "bwdif must precede select");
        assert!(select_pos < scale_pos, "select must precede scale");
    }

    #[test]
    fn ffmpeg_args_with_sub_uses_filter_complex() {
        let ts = Path::new("/mnt/video.ts");
        let sub = Path::new("/tmp/sub.png");
        let p = CaptureParams {
            ts_path: ts,
            pre_seek: 10.0,
            dur: 5.0,
            frame_nums: &[2],
            sub_png: Some(sub),
            width: 1920,
            height: 1080,
            quality: 2,
            out_pattern: "/tmp/out.jpg",
        };
        let args = build_ffmpeg_args(&p);
        assert!(
            args.contains(&"-filter_complex".to_string()),
            "sub path must use -filter_complex"
        );
        assert!(!args.contains(&"-vf".to_string()));
        // Both -i flags: one for TS, one for subtitle PNG
        assert_eq!(args.iter().filter(|a| a.as_str() == "-i").count(), 2);
        // overlay mapping
        assert!(args.contains(&"-map".to_string()));
        // Decode stops after the single selected frame
        let frames_idx = args
            .iter()
            .position(|a| a == "-frames:v")
            .expect("-frames:v must be present");
        assert_eq!(args[frames_idx + 1], "1");
    }

    // ── build_preview_args ─────────────────────────────────────────────────────

    #[test]
    fn preview_args_ss_before_input() {
        let args = build_preview_args(Path::new("/mnt/video.ts"), 54.8, 640, 360, 4, "/tmp/p.jpg");
        let ss_pos = args.iter().position(|a| a == "-ss").expect("-ss present");
        let i_pos = args.iter().position(|a| a == "-i").expect("-i present");
        assert!(ss_pos < i_pos, "-ss must come before -i");
        assert_eq!(args[ss_pos + 1], "54.800000");
    }

    #[test]
    fn preview_args_single_frame_no_select() {
        let args = build_preview_args(Path::new("/mnt/video.ts"), 54.8, 640, 360, 4, "/tmp/p.jpg");
        let frames_idx = args
            .iter()
            .position(|a| a == "-frames:v")
            .expect("-frames:v must be present");
        assert_eq!(args[frames_idx + 1], "1");
        // Direct seek: no select / setpts / -t — decode must not span the window
        assert!(!args.iter().any(|a| a.contains("select")));
        assert!(!args.iter().any(|a| a.contains("setpts")));
        assert!(!args.contains(&"-t".to_string()));
    }

    #[test]
    fn preview_args_scale_and_quality() {
        let args = build_preview_args(Path::new("/mnt/video.ts"), 1.0, 640, 360, 4, "/tmp/p.jpg");
        let vf_idx = args.iter().position(|a| a == "-vf").expect("-vf present");
        assert!(args[vf_idx + 1].contains("scale=640:360"));
        assert!(args[vf_idx + 1].contains("bwdif=mode=send_frame"));
        let q_idx = args.iter().position(|a| a == "-q:v").expect("-q:v present");
        assert_eq!(args[q_idx + 1], "4");
        assert_eq!(args.last().unwrap(), "/tmp/p.jpg");
    }
}
