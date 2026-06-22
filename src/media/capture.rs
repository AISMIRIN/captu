use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Path where a contact-sheet thumbnail is cached.
pub fn thumb_path(cache_dir: &Path, stem: &str, id: i64, n: u32) -> PathBuf {
    cache_dir
        .join(stem)
        .join("thumbs")
        .join(format!("{}_{:02}.jpg", id, n))
}

/// Generate contact-sheet thumbnails using a stored subtitle PNG overlay.
///
/// # Pipeline
/// ```text
/// ffmpeg (stage 1)                   ffmpeg (stage 2)
///   -ss pre_seek                       -i pipe:0
///   -i ts                              [-i sub.png]
///   -vf scale=WxH,setsar=1            -filter_complex "[0:v]select='...',setpts=…[v];
///   -c:v mjpeg -q:v 2                               [v][1:v]overlay=eof_action=repeat[out]"
///   -f matroska pipe:1                 -map [out] -fps_mode vfr -q:v {q} _tmp_%d.jpg
/// ```
///
/// The subtitle PNG (`cache/{stem}/sub/{id}.png`) is rendered on-demand by
/// `subtitle::ensure_caption_png` at first access, so the thumbnail pipeline
/// never reads the TS subtitle stream directly.  This eliminates PTS-discontinuity
/// misalignment that plagued seek-based bitmap overlay approaches.
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
    let pts_end_sec = pts_end_ms as f64 / 1000.0;

    let pre_seek = (pts_start_sec - 6.0).max(0.0);
    let win_start = pts_start_sec + 1.5;
    let win_end = if pts_end_sec > win_start {
        pts_end_sec
    } else {
        win_start + 0.5
    };
    let dur = (win_end - pre_seek) + 0.5;

    // Frame indices in the MJPEG stream (terrestrial = 29.97 fps = 30000/1001).
    let fps = 30_000.0_f64 / 1001.0;
    let frame_nums = frame_indices(pts_start_ms, pts_end_ms, count, fps);

    let w = cfg.capture.width;
    let h = cfg.capture.height;
    let q = cfg.capture.jpeg_quality;

    let input_url = format!("file:{}", ts_path.to_str().unwrap_or(""));

    // Ensure the subtitle PNG exists for this caption (on-demand rendering).
    let sub_png_opt = subtitle::ensure_caption_png(&cfg.capture, cache_dir, ts_path, id, pts_start_ms)?;
    let has_sub = sub_png_opt.is_some();

    // Stage 1: decode video segment to MJPEG pipe (no subtitle processing).
    // -ss before -i for fast keyframe seek; short -t guards against reading the whole file.
    let mut enc = Command::new("ffmpeg")
        .args([
            "-y",
            "-ss", &format!("{:.6}", pre_seek),
            "-t",  &format!("{:.6}", dur),
            "-i",  &input_url,
            "-vf", &format!("scale={}:{},setsar=1", w, h),
            "-c:v", "mjpeg",
            "-q:v", "2",
            "-an",
            "-f", "matroska",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let enc_stdout = enc.stdout.take().expect("enc stdout was piped");
    let enc_stderr_pipe = enc.stderr.take().expect("enc stderr was piped");

    // Drain enc stderr in a background thread so the OS pipe buffer never stalls enc.
    let enc_stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut pipe = enc_stderr_pipe;
        pipe.read_to_end(&mut buf).ok();
        buf
    });

    // Stage 2: select frames from the MJPEG stream and composite subtitle PNG if present.
    let select_expr = frame_nums
        .iter()
        .map(|n| format!("eq(n\\,{})", n))
        .collect::<Vec<_>>()
        .join("+");

    let tmp_pattern = thumbs_dir.join("_tmp_%d.jpg");
    let tmp_str = tmp_pattern.to_str().unwrap_or("").to_string();

    let dec = if has_sub {
        let sub_str = sub_png_opt.as_ref().unwrap().to_str().unwrap_or("").to_string();
        let filter = format!(
            "[0:v]select='{}',setpts=N/FRAME_RATE/TB[v];[v][1:v]overlay=eof_action=repeat[out]",
            select_expr
        );
        Command::new("ffmpeg")
            .args([
                "-y",
                "-i", "pipe:0",
                "-i", &sub_str,
                "-filter_complex", &filter,
                "-map", "[out]",
                "-fps_mode", "vfr",
                "-q:v", &q.to_string(),
                &tmp_str,
            ])
            .stdin(Stdio::from(enc_stdout))
            .stderr(Stdio::piped())
            .spawn()?
    } else {
        let vf = format!("select='{}',setpts=N/FRAME_RATE/TB", select_expr);
        Command::new("ffmpeg")
            .args([
                "-y",
                "-i", "pipe:0",
                "-vf", &vf,
                "-fps_mode", "vfr",
                "-q:v", &q.to_string(),
                &tmp_str,
            ])
            .stdin(Stdio::from(enc_stdout))
            .stderr(Stdio::piped())
            .spawn()?
    };

    // Wait for dec first — it drives consumption of enc's stdout.
    let dec_out = dec.wait_with_output()?;
    let enc_status = enc.wait()?;
    let enc_stderr = enc_stderr_thread.join().unwrap_or_default();

    if !enc_status.success() || !dec_out.status.success() {
        bail!(
            "thumbnail pipeline failed for {}:\n  enc exit: {}\n  enc stderr:\n{}\n  dec exit: {}\n  dec stderr:\n{}",
            ts_path.display(),
            enc_status,
            String::from_utf8_lossy(&enc_stderr),
            dec_out.status,
            String::from_utf8_lossy(&dec_out.stderr),
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

#[cfg(test)]
mod tests {
    use super::{frame_indices, thumb_path, ts_stem};
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
