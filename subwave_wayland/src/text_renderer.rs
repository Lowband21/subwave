//! Render subtitle text strings to pre-multiplied ARGB8888 bitmaps.
//!
//! Uses `ab_glyph` with a system TrueType font.  The rendered output is
//! a full-canvas-sized transparent image with white, outlined text
//! positioned at the bottom-center — ready to push directly to the
//! Wayland subtitle subsurface via `attach_subtitle_frame`.

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};
use once_cell::sync::OnceCell;
use std::sync::Mutex;

/// Common system font paths (bold preferred for subtitle readability).
const FONT_SEARCH_PATHS: &[&str] = &[
    // Debian / Ubuntu
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    // Arch / Fedora
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Bold.ttf",
    // Noto
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    // NixOS (Nix store symlink farms)
    "/run/current-system/sw/share/X11/fonts/DejaVuSans-Bold.ttf",
    "/run/current-system/sw/share/X11/fonts/DejaVuSans.ttf",
];

static FONT_DATA: OnceCell<Vec<u8>> = OnceCell::new();

fn load_font_data() -> Option<&'static [u8]> {
    FONT_DATA
        .get_or_try_init(|| {
            for path in FONT_SEARCH_PATHS {
                if let Ok(data) = std::fs::read(path) {
                    log::info!("[text-renderer] Loaded font: {path}");
                    return Ok(data);
                }
            }
            // Also try fc-match as a last resort
            if let Ok(output) = std::process::Command::new("fc-match")
                .args(["--format=%{file}", "sans:bold"])
                .output()
            {
                let path = String::from_utf8_lossy(&output.stdout);
                let path = path.trim();
                if !path.is_empty() {
                    if let Ok(data) = std::fs::read(path) {
                        log::info!("[text-renderer] Loaded font via fc-match: {path}");
                        return Ok(data);
                    }
                }
            }
            log::warn!("[text-renderer] No system font found — text subtitles unavailable");
            Err(())
        })
        .ok()
        .map(|v| v.as_slice())
}

/// Shared renderer instance (holds nothing except confirmation that a
/// font is loadable).  Actual rendering is stateless.
pub struct TextRenderer;

/// Scratch buffers reused across frames to avoid repeated allocation.
struct ScratchBuffers {
    /// Per-pixel alpha from glyph rasterisation (single channel).
    alpha: Vec<u8>,
    /// Dilated outline alpha (single channel).
    outline: Vec<u8>,
}

static SCRATCH: OnceCell<Mutex<ScratchBuffers>> = OnceCell::new();

fn scratch(canvas_size: usize) -> &'static Mutex<ScratchBuffers> {
    SCRATCH.get_or_init(|| {
        Mutex::new(ScratchBuffers {
            alpha: vec![0u8; canvas_size],
            outline: vec![0u8; canvas_size],
        })
    })
}

impl TextRenderer {
    /// Returns `None` if no usable font is found on the system.
    pub fn new() -> Option<Self> {
        load_font_data().map(|_| TextRenderer)
    }

    /// Render `text` onto a `canvas_w × canvas_h` pre-multiplied ARGB8888
    /// buffer.  Text is white with a dark outline, centred horizontally,
    /// near the bottom of the canvas.  Multi-line text (split on `\n`) is
    /// supported.
    ///
    /// Returns `None` if the font failed to parse (should not happen if
    /// `new()` succeeded).
    pub fn render(&self, text: &str, canvas_w: usize, canvas_h: usize) -> Option<Vec<u8>> {
        let font_data = load_font_data()?;
        let font = FontRef::try_from_slice(font_data).ok()?;

        // Strip common HTML-ish subtitle tags (<i>, <b>, <font …>, etc.)
        let clean = strip_tags(text);
        let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return None;
        }

        // Scale: ~4.5% of canvas height, clamped to a sane range.
        let px = (canvas_h as f32 * 0.045).clamp(18.0, 80.0);
        let scale = PxScale::from(px);
        let scaled = font.as_scaled(scale);

        let line_height = scaled.height() + scaled.line_gap();
        let ascent = scaled.ascent();

        // ── Layout all lines ──────────────────────────────────────────
        let mut laid_out: Vec<Vec<Glyph>> = Vec::with_capacity(lines.len());
        let mut line_widths: Vec<f32> = Vec::with_capacity(lines.len());

        for line in &lines {
            let mut glyphs = Vec::new();
            let mut x = 0.0f32;
            let mut prev: Option<ab_glyph::GlyphId> = None;
            for ch in line.chars() {
                let gid = font.glyph_id(ch);
                if let Some(p) = prev {
                    x += scaled.kern(p, gid);
                }
                glyphs.push(gid.with_scale_and_position(scale, point(x, 0.0)));
                x += scaled.h_advance(gid);
                prev = Some(gid);
            }
            line_widths.push(x);
            laid_out.push(glyphs);
        }

        let total_text_h = lines.len() as f32 * line_height;
        let margin_bottom = (canvas_h as f32 * 0.06).max(12.0);
        let block_top = (canvas_h as f32 - margin_bottom - total_text_h).max(0.0);

        // ── Rasterise into a single-channel alpha buffer ──────────────
        let canvas_size = canvas_w * canvas_h;
        let scratch_mtx = scratch(canvas_size);
        let mut bufs = scratch_mtx.lock().unwrap();
        // Ensure buffers are large enough and zeroed.
        bufs.alpha.resize(canvas_size, 0);
        bufs.alpha.fill(0);
        bufs.outline.resize(canvas_size, 0);
        bufs.outline.fill(0);

        for (i, glyphs) in laid_out.iter().enumerate() {
            let lw = line_widths[i];
            let x_off = ((canvas_w as f32 - lw) / 2.0).max(0.0);
            let y_off = block_top + i as f32 * line_height + ascent;

            for glyph in glyphs {
                let positioned = Glyph {
                    position: point(glyph.position.x + x_off, glyph.position.y + y_off),
                    ..glyph.clone()
                };
                if let Some(og) = font.outline_glyph(positioned) {
                    let bb = og.px_bounds();
                    og.draw(|rx, ry, cov| {
                        let px = bb.min.x as i32 + rx as i32;
                        let py = bb.min.y as i32 + ry as i32;
                        if px >= 0
                            && (px as usize) < canvas_w
                            && py >= 0
                            && (py as usize) < canvas_h
                        {
                            let idx = py as usize * canvas_w + px as usize;
                            // Max-blend so overlapping glyphs don't over-brighten.
                            let v = (cov * 255.0) as u8;
                            bufs.alpha[idx] = bufs.alpha[idx].max(v);
                        }
                    });
                }
            }
        }

        // ── Build outline by dilating the alpha channel ───────────────
        // A 3×3 max-filter gives a ~2px outline at typical scales.
        // Two passes (dilate twice) give ~4 px which is readable at 4K.
        let passes = if px >= 36.0 { 2 } else { 1 };
        // Copy alpha → outline as starting point (split borrow via index copy)
        for i in 0..canvas_size {
            bufs.outline[i] = bufs.alpha[i];
        }
        for _ in 0..passes {
            // We dilate in-place using a temporary read from `alpha`.
            // Swap roles each pass: outline is the latest dilated version.
            let src = bufs.outline.clone(); // TODO: avoid clone with double-buffer
            for y in 0..canvas_h {
                for x in 0..canvas_w {
                    let mut mx = src[y * canvas_w + x];
                    for dy in -1i32..=1 {
                        for dx in -1i32..=1 {
                            let nx = x as i32 + dx;
                            let ny = y as i32 + dy;
                            if nx >= 0
                                && (nx as usize) < canvas_w
                                && ny >= 0
                                && (ny as usize) < canvas_h
                            {
                                mx = mx.max(src[ny as usize * canvas_w + nx as usize]);
                            }
                        }
                    }
                    bufs.outline[y * canvas_w + x] = mx;
                }
            }
        }

        // ── Composite into pre-multiplied ARGB8888 ────────────────────
        // Outline = black (0,0,0) at outline alpha
        // Foreground = white (255,255,255) at glyph alpha
        // Standard "over" compositing with pre-multiplied values.
        let stride = canvas_w * 4;
        let mut argb = vec![0u8; stride * canvas_h];

        for y in 0..canvas_h {
            for x in 0..canvas_w {
                let idx = y * canvas_w + x;
                let fg_a = bufs.alpha[idx];
                let ol_a = bufs.outline[idx];
                if ol_a == 0 {
                    continue;
                }

                // Background layer: black outline
                // Pre-multiplied black: (B=0, G=0, R=0, A=ol_a)
                let mut r = 0u16;
                let mut g = 0u16;
                let mut b = 0u16;
                let mut a = ol_a as u16;

                // Foreground layer: white text  ("over" onto outline)
                // Pre-multiplied white: (B=fg_a, G=fg_a, R=fg_a, A=fg_a)
                if fg_a > 0 {
                    let fa = fg_a as u16;
                    let inv = 255 - fa;
                    b = fa + (b * inv / 255);
                    g = fa + (g * inv / 255);
                    r = fa + (r * inv / 255);
                    a = fa + (a * inv / 255);
                }

                let off = y * stride + x * 4;
                // ARGB8888 little-endian: [B, G, R, A]
                argb[off] = b as u8;
                argb[off + 1] = g as u8;
                argb[off + 2] = r as u8;
                argb[off + 3] = a as u8;
            }
        }

        Some(argb)
    }
}

/// Strip HTML-like tags that SRT/WebVTT subtitle text may contain.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(ch);
        }
    }
    out
}
