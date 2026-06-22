// ARIB STD-B24 caption extraction via libaribcaption.
//
// Extraction (ingest time):
//   `extract_captions` reads the TS sequentially, saves a compact PES blob to
//   `cache/{stem}/captions.pes`, and returns caption text + timestamps for DB
//   insertion.  No rendering is done here.
//
// Rendering (on-demand, thumbnail time):
//   `ensure_caption_png` loads the PES blob, replays the full decode sequence,
//   renders the subtitle at a given PTS, and writes the PNG to
//   `cache/{stem}/sub/{id}.png`.  Subsequent calls return the cached file.
//
// By separating these concerns, ingest avoids the cost of rendering 100s of
// bitmaps per TS file, while the thumbnail pipeline still has correct bitmaps
// (PTS-discontinuity-safe) on first access.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::pes;
use crate::config::CaptureConfig;

/// A single caption event extracted from a TS file.
pub struct Caption {
    pub pts_start_ms: i64,
    pub pts_end_ms: i64,
    pub text: String,
}

// ── RGBA canvas compositing ────────────────────────────────────────────────

/// Composite `images` from libaribcaption onto a transparent `w × h` RGBA canvas
/// and return the flat buffer.
pub(crate) fn composite_rgba(
    images: &[aribcaption_sys::RenderedImage],
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut canvas = vec![0u8; w * h * 4];
    for img in images {
        let src_x = img.dst_x.max(0) as usize;
        let src_y = img.dst_y.max(0) as usize;
        let img_w = img.width.max(0) as usize;
        let img_h = img.height.max(0) as usize;
        let stride = img.stride.max(0) as usize;

        for row in 0..img_h {
            let dst_row = src_y + row;
            if dst_row >= h {
                break;
            }
            let src_off = row * stride;
            if src_off + img_w * 4 > img.rgba.len() {
                break;
            }
            for col in 0..img_w {
                let dst_col = src_x + col;
                if dst_col >= w {
                    break;
                }
                let s = src_off + col * 4;
                let d = (dst_row * w + dst_col) * 4;
                let sa = img.rgba[s + 3] as u32;
                if sa == 0 {
                    continue;
                }
                if sa == 255 {
                    canvas[d..d + 4].copy_from_slice(&img.rgba[s..s + 4]);
                } else {
                    let inv = 255 - sa;
                    canvas[d]     = ((img.rgba[s]     as u32 * sa + canvas[d]     as u32 * inv) / 255) as u8;
                    canvas[d + 1] = ((img.rgba[s + 1] as u32 * sa + canvas[d + 1] as u32 * inv) / 255) as u8;
                    canvas[d + 2] = ((img.rgba[s + 2] as u32 * sa + canvas[d + 2] as u32 * inv) / 255) as u8;
                    canvas[d + 3] = (sa + canvas[d + 3] as u32 * inv / 255).min(255) as u8;
                }
            }
        }
    }
    canvas
}

/// Encode a flat RGBA8888 buffer (`w × h × 4` bytes) as a PNG `Vec<u8>`.
pub(crate) fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::encode_png;

    #[test]
    fn encode_png_roundtrip_dimensions() {
        // Encode a 4×2 solid red RGBA image and decode it back to verify dimensions.
        let w = 4u32;
        let h = 2u32;
        // RGBA: all pixels are opaque red
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let png_bytes = encode_png(&rgba, w, h).expect("encode should succeed");

        // Decode with the png crate to verify the output is valid
        let decoder = png::Decoder::new(png_bytes.as_slice());
        let mut reader = decoder.read_info().expect("PNG decode failed");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("PNG frame read failed");

        assert_eq!(info.width, w);
        assert_eq!(info.height, h);
        assert_eq!(info.color_type, png::ColorType::Rgba);
    }

    #[test]
    fn encode_png_1x1_transparent() {
        // Fully transparent single pixel
        let png_bytes = encode_png(&[0u8, 0, 0, 0], 1, 1).expect("encode should succeed");
        let decoder = png::Decoder::new(png_bytes.as_slice());
        let mut reader = decoder.read_info().expect("PNG decode failed");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        reader.next_frame(&mut buf).expect("frame read failed");
        assert_eq!(buf[3], 0, "alpha channel should be 0 (transparent)");
    }

    #[test]
    fn encode_png_nonempty() {
        let rgba = vec![128u8; 8 * 8 * 4]; // 8×8 grey
        let bytes = encode_png(&rgba, 8, 8).expect("encode should succeed");
        // PNG files start with the 8-byte PNG signature
        assert!(bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
    }
}

// ── Ingest-time extraction ─────────────────────────────────────────────────

/// Extract all ARIB caption events from a TS file.
///
/// `caption_pid` must be the elementary PID returned by `pes::scan_psi`
/// (or `None` when the TS has no caption stream).  Passing it in avoids
/// re-reading the PAT/PMT — callers should run `scan_psi` once and share
/// the result between `extract_epg` and `extract_captions`.
///
/// Saves the raw PES packet list to `cache/{stem}/captions.pes` for later
/// on-demand rendering.  Returns caption text and timestamps for DB insertion
/// only — no bitmaps are rendered here.
pub fn extract_captions(
    ts_path: &Path,
    cache_dir: &Path,
    caption_pid: Option<u16>,
) -> Result<Vec<Caption>> {
    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let caption_pid = match caption_pid {
        Some(pid) => pid,
        None => {
            tracing::debug!("no ARIB caption PID in {}", ts_path.display());
            return Ok(vec![]);
        }
    };

    let pes_list = pes::demux_caption_pes(ts_path, caption_pid);
    if pes_list.is_empty() {
        return Ok(vec![]);
    }

    // Persist the PES blob so the renderer can replay without re-reading the TS.
    let blob_path = cache_dir.join(&stem).join("captions.pes");
    pes::write_pes_blob(&blob_path, &pes_list)?;

    // Decode text + timestamps for DB (no rendering).
    let ctx = aribcaption_sys::Context::new()
        .ok_or_else(|| anyhow::anyhow!("aribcc_context_alloc failed"))?;
    let mut decoder = aribcaption_sys::Decoder::new(&ctx)
        .ok_or_else(|| anyhow::anyhow!("aribcc_decoder init failed"))?;

    struct RawCaption {
        pts_ms: i64,
        duration_ms: Option<i64>,
        text: String,
    }

    let mut raw: Vec<RawCaption> = Vec::new();

    for pes_pkt in &pes_list {
        if let Some(cap) = decoder.decode(&pes_pkt.payload, pes_pkt.pts_ms) {
            let text = cap.text().trim().to_string();
            if text.is_empty() {
                continue;
            }
            raw.push(RawCaption {
                pts_ms: cap.pts_ms(),
                duration_ms: cap.duration_ms(),
                text,
            });
        }
    }

    if raw.is_empty() {
        return Ok(vec![]);
    }

    // Compute pts_end: duration if known, otherwise next caption's pts or +5000 ms.
    let count = raw.len();
    let mut result: Vec<Caption> = Vec::with_capacity(count);

    for i in 0..count {
        let pts_start_ms = raw[i].pts_ms;
        let pts_end_ms = match raw[i].duration_ms {
            Some(dur) => pts_start_ms + dur,
            None => {
                if i + 1 < count {
                    raw[i + 1].pts_ms
                } else {
                    pts_start_ms + 5000
                }
            }
        };

        if pts_end_ms <= pts_start_ms {
            continue;
        }

        result.push(Caption {
            pts_start_ms,
            pts_end_ms,
            text: raw[i].text.clone(),
        });
    }

    Ok(result)
}

// ── On-demand rendering ────────────────────────────────────────────────────

/// Returns the path to the subtitle PNG for the given caption, generating it
/// on first access (lazy cache).
///
/// Replays the full PES sequence from `cache/{stem}/captions.pes` in memory
/// and renders the subtitle active at `pts_start_ms`.  The result is written
/// to `cache/{stem}/sub/{id}.png` and reused on subsequent calls.
///
/// Returns `None` when no subtitle is visible at the given PTS (e.g. the TS
/// has no caption stream, or the renderer produced no pixels).
pub fn ensure_caption_png(
    cfg: &CaptureConfig,
    cache_dir: &Path,
    ts_path: &Path,
    id: i64,
    pts_start_ms: i64,
) -> Result<Option<PathBuf>> {
    let stem = ts_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let png_path = cache_dir.join(&stem).join("sub").join(format!("{}.png", id));

    // Fast path: already rendered.
    if png_path.exists() {
        return Ok(Some(png_path));
    }

    // Load PES blob (written by extract_captions at ingest time).
    let blob_path = cache_dir.join(&stem).join("captions.pes");
    if !blob_path.exists() {
        tracing::debug!("no captions.pes for {} — no subtitle PNG", ts_path.display());
        return Ok(None);
    }

    let pes_list = pes::read_pes_blob(&blob_path)?;
    if pes_list.is_empty() {
        return Ok(None);
    }

    // Replay full PES sequence through decoder + renderer.
    // This is fast (memory-only, μs-range per packet) and ensures the renderer
    // has the correct accumulated state (DRCS, graphic sets, management data).
    let ctx = aribcaption_sys::Context::new()
        .ok_or_else(|| anyhow::anyhow!("aribcc_context_alloc failed"))?;
    let mut decoder = aribcaption_sys::Decoder::new(&ctx)
        .ok_or_else(|| anyhow::anyhow!("aribcc_decoder init failed"))?;
    let mut renderer = aribcaption_sys::Renderer::new(&ctx, cfg.width as i32, cfg.height as i32)
        .ok_or_else(|| anyhow::anyhow!("aribcc_renderer init failed"))?;

    for pes_pkt in &pes_list {
        if let Some(cap) = decoder.decode(&pes_pkt.payload, pes_pkt.pts_ms) {
            renderer.append_caption(&cap);
        }
    }

    // Render the subtitle active at pts_start_ms.
    let images = renderer.render(pts_start_ms);
    if images.is_empty() {
        tracing::debug!("renderer returned no images at pts={}ms for caption {}", pts_start_ms, id);
        return Ok(None);
    }

    let rgba = composite_rgba(&images, cfg.width as usize, cfg.height as usize);

    // Skip fully-transparent composites.
    if rgba.iter().skip(3).step_by(4).all(|&a| a == 0) {
        return Ok(None);
    }

    // Write PNG, creating the sub/ directory if needed.
    if let Some(parent) = png_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let png_data = encode_png(&rgba, cfg.width, cfg.height)?;
    std::fs::write(&png_path, &png_data)?;
    tracing::debug!("rendered subtitle PNG for caption {} at pts={}ms", id, pts_start_ms);

    Ok(Some(png_path))
}
