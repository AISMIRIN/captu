// Safe Rust wrappers for libaribcaption (ARIB STD-B24 caption decoder/renderer).
//
// All raw `aribcc_*` symbols are provided by the `aribcaption-sys` crate via
// bindgen; this crate exposes only the safe, RAII-managed API.

use std::ffi::CStr;

/// Sentinel: caption duration is undetermined (ARIBCC_DURATION_INDEFINITE).
pub const DURATION_INDEFINITE: i64 = i64::MAX;

/// Sentinel: no PTS available (ARIBCC_PTS_NOPTS = i64::MIN).
pub const PTS_NOPTS: i64 = i64::MIN;

// ── High-level rendered image ──────────────────────────────────────────────

/// A single rendered ARIB subtitle bitmap fragment, positioned in the frame.
/// The bitmap is RGBA8888 format, row-major, with `stride` bytes per row.
#[derive(Debug, Clone)]
pub struct RenderedImage {
    pub dst_x: i32,
    pub dst_y: i32,
    pub width: i32,
    pub height: i32,
    /// Bytes per row (may be larger than width * 4 due to alignment).
    pub stride: i32,
    /// Raw RGBA8888 pixel data, `stride * height` bytes.
    pub rgba: Vec<u8>,
}

// ── Context ────────────────────────────────────────────────────────────────

/// RAII wrapper for `aribcc_context_t`.
pub struct Context {
    ptr: *mut aribcaption_sys::aribcc_context_t,
}

// aribcc objects are not thread-safe per the library docs; keep Send/Sync
// gated to explicitly single-threaded use (the PoC runs on one thread).
unsafe impl Send for Context {}

impl Context {
    pub fn new() -> Option<Self> {
        let ptr = unsafe { aribcaption_sys::aribcc_context_alloc() };
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut aribcaption_sys::aribcc_context_t {
        self.ptr
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { aribcaption_sys::aribcc_context_free(self.ptr) }
    }
}

// ── Decoder ────────────────────────────────────────────────────────────────

/// RAII wrapper for `aribcc_decoder_t`.
///
/// The decoder accumulates internal state across successive `decode()` calls,
/// so all PES packets for a TS file must be fed in presentation order.
pub struct Decoder {
    ptr: *mut aribcaption_sys::aribcc_decoder_t,
}

unsafe impl Send for Decoder {}

impl Decoder {
    /// Create and initialise a decoder for Japanese ISDB-T (profile A, caption stream).
    ///
    /// The decoder is initialised with library defaults. Call
    /// [`set_replace_msz_fullwidth_japanese`] after construction to adjust
    /// whether MSZ half-width characters are substituted in the decoded text.
    pub fn new(ctx: &Context) -> Option<Self> {
        let ptr = unsafe { aribcaption_sys::aribcc_decoder_alloc(ctx.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let ok = unsafe {
            aribcaption_sys::aribcc_decoder_initialize(
                ptr,
                aribcaption_sys::aribcc_encoding_scheme_t_ARIBCC_ENCODING_SCHEME_AUTO,
                aribcaption_sys::aribcc_captiontype_t_ARIBCC_CAPTIONTYPE_CAPTION,
                aribcaption_sys::aribcc_profile_t_ARIBCC_PROFILE_A,
                aribcaption_sys::aribcc_languageid_t_ARIBCC_LANGUAGEID_FIRST,
            )
        };
        if ok {
            Some(Self { ptr })
        } else {
            unsafe { aribcaption_sys::aribcc_decoder_free(ptr) };
            None
        }
    }

    /// Control whether full-width Japanese characters written in MSZ (medium
    /// size) mode are replaced with their half-width equivalents.
    ///
    /// When `replace` is `false`, full-width ー (U+30FC) is preserved rather
    /// than being substituted with half-width ｰ (U+FF70). The library default
    /// is `true` (replace). Callers that need the original full-width form
    /// should set this to `false` explicitly.
    pub fn set_replace_msz_fullwidth_japanese(&mut self, replace: bool) {
        unsafe {
            aribcaption_sys::aribcc_decoder_set_replace_msz_fullwidth_japanese(self.ptr, replace)
        }
    }

    /// Decode one ARIB PES payload (the raw bytes starting with `data_identifier`
    /// 0x80, exactly as they appear in the PES packet data area).
    ///
    /// `pts_ms` is the PTS in milliseconds, relative to the start of the stream
    /// (same reference as `captions.pts_start`).
    ///
    /// Returns `Some(caption)` when a complete caption event was decoded.
    pub fn decode(&mut self, pes: &[u8], pts_ms: i64) -> Option<AribCaption> {
        let mut cap: aribcaption_sys::aribcc_caption_t = unsafe { std::mem::zeroed() };
        let status = unsafe {
            aribcaption_sys::aribcc_decoder_decode(
                self.ptr,
                pes.as_ptr(),
                pes.len(),
                pts_ms,
                &mut cap,
            )
        };
        if status == aribcaption_sys::aribcc_decode_status_t_ARIBCC_DECODE_STATUS_GOT_CAPTION {
            Some(AribCaption { inner: cap })
        } else {
            // No caption or error — no cleanup needed for zeroed struct.
            None
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { aribcaption_sys::aribcc_decoder_free(self.ptr) }
    }
}

// ── AribCaption ────────────────────────────────────────────────────────────

/// RAII holder for a decoded `aribcc_caption_t`.
///
/// Passed to `Renderer::append_caption` and automatically cleaned up on drop.
pub struct AribCaption {
    pub inner: aribcaption_sys::aribcc_caption_t,
}

impl AribCaption {
    /// The caption text in UTF-8, with ruby (furigana) excluded.
    /// Returns an empty string if the internal pointer is null.
    pub fn text(&self) -> String {
        if self.inner.text.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(self.inner.text) }
            .to_string_lossy()
            .into_owned()
    }

    /// Presentation timestamp in milliseconds (stream-relative).
    pub fn pts_ms(&self) -> i64 {
        self.inner.pts
    }

    /// Caption duration in milliseconds.
    /// Returns `None` when duration is undetermined (`ARIBCC_DURATION_INDEFINITE`).
    pub fn duration_ms(&self) -> Option<i64> {
        if self.inner.wait_duration == DURATION_INDEFINITE {
            None
        } else {
            Some(self.inner.wait_duration)
        }
    }

    /// True when this caption event carries the CLEARSCREEN flag.
    pub fn is_clear_screen(&self) -> bool {
        (self.inner.flags
            & aribcaption_sys::aribcc_captionflags_t_ARIBCC_CAPTIONFLAGS_CLEARSCREEN)
            != 0
    }
}

impl Drop for AribCaption {
    fn drop(&mut self) {
        unsafe { aribcaption_sys::aribcc_caption_cleanup(&mut self.inner) }
    }
}

// ── Renderer ───────────────────────────────────────────────────────────────

/// RAII wrapper for `aribcc_renderer_t`.
pub struct Renderer {
    ptr: *mut aribcaption_sys::aribcc_renderer_t,
}

unsafe impl Send for Renderer {}

impl Renderer {
    /// Create and initialise a renderer using fontconfig + freetype (Linux default).
    pub fn new(ctx: &Context, frame_width: i32, frame_height: i32) -> Option<Self> {
        let ptr = unsafe { aribcaption_sys::aribcc_renderer_alloc(ctx.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let ok = unsafe {
            aribcaption_sys::aribcc_renderer_initialize(
                ptr,
                aribcaption_sys::aribcc_captiontype_t_ARIBCC_CAPTIONTYPE_CAPTION,
                aribcaption_sys::aribcc_fontprovider_type_t_ARIBCC_FONTPROVIDER_TYPE_FONTCONFIG,
                aribcaption_sys::aribcc_textrenderer_type_t_ARIBCC_TEXTRENDERER_TYPE_FREETYPE,
            )
        };
        if !ok {
            unsafe { aribcaption_sys::aribcc_renderer_free(ptr) };
            return None;
        }
        let ok2 = unsafe {
            aribcaption_sys::aribcc_renderer_set_frame_size(ptr, frame_width, frame_height)
        };
        if !ok2 {
            unsafe { aribcaption_sys::aribcc_renderer_free(ptr) };
            return None;
        }
        Some(Self { ptr })
    }

    /// Feed a decoded caption into the renderer's internal timeline.
    pub fn append_caption(&mut self, caption: &AribCaption) -> bool {
        unsafe { aribcaption_sys::aribcc_renderer_append_caption(self.ptr, &caption.inner) }
    }

    /// Render the caption active at `pts_ms` (milliseconds from stream start).
    ///
    /// Returns the list of RGBA image fragments to composite onto the video frame.
    /// An empty vec means no caption is active at this PTS.
    pub fn render(&mut self, pts_ms: i64) -> Vec<RenderedImage> {
        let mut result: aribcaption_sys::aribcc_render_result_t =
            unsafe { std::mem::zeroed() };
        let status =
            unsafe { aribcaption_sys::aribcc_renderer_render(self.ptr, pts_ms, &mut result) };

        if status != aribcaption_sys::aribcc_render_status_t_ARIBCC_RENDER_STATUS_GOT_IMAGE
            && status
                != aribcaption_sys::aribcc_render_status_t_ARIBCC_RENDER_STATUS_GOT_IMAGE_UNCHANGED
        {
            return vec![];
        }

        // Copy image data out of the library-owned buffer before cleanup.
        let count = result.image_count as usize;
        let images = unsafe { std::slice::from_raw_parts(result.images, count) };
        let rendered: Vec<RenderedImage> = images
            .iter()
            .map(|img| {
                let size = (img.stride * img.height) as usize;
                let rgba =
                    unsafe { std::slice::from_raw_parts(img.bitmap, size) }.to_vec();
                RenderedImage {
                    dst_x: img.dst_x,
                    dst_y: img.dst_y,
                    width: img.width,
                    height: img.height,
                    stride: img.stride,
                    rgba,
                }
            })
            .collect();

        unsafe { aribcaption_sys::aribcc_render_result_cleanup(&mut result) };

        rendered
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { aribcaption_sys::aribcc_renderer_free(self.ptr) }
    }
}
