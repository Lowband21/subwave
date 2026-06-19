//! Render subtitle text strings to pre-multiplied ARGB8888 bitmaps.
//!
//! Uses `ab_glyph` with a system TrueType font.  The rendered output is
//! a full-canvas-sized transparent image with white text and a dark
//! shadow, positioned at the bottom-center — ready for Wayland subtitle
//! subsurface attachment.
//!
//! Rendering is designed to be fast enough to run from the UI/tick path when
//! a scheduled text cue becomes due.  No per-pixel dilation or multi-pass
//! filters — just two glyph passes (shadow + foreground).

use ab_glyph::{point, Font, FontRef, Glyph, PxScale, ScaleFont};
use once_cell::sync::OnceCell;

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

impl TextRenderer {
    /// Returns `None` if no usable font is found on the system.
    pub fn new() -> Option<Self> {
        load_font_data().map(|_| TextRenderer)
    }

    /// Render `text` onto a `canvas_w × canvas_h` pre-multiplied ARGB8888
    /// buffer.  White text with a dark shadow, centred horizontally, near
    /// the bottom of the canvas.  Multi-line text (split on `\n`) is
    /// supported.
    ///
    /// Performance: two glyph-rasterisation passes (shadow + foreground),
    /// no per-pixel post-processing.  Typically <1 ms for a couple of
    /// subtitle lines at 1080p.
    ///
    /// Returns `None` if the font failed to parse or text is empty.
    pub fn render(&self, text: &str, canvas_w: usize, canvas_h: usize) -> Option<Vec<u8>> {
        let font_data = load_font_data()?;
        let font = FontRef::try_from_slice(font_data).ok()?;

        // Strip common HTML-ish subtitle tags (<i>, <b>, <font …>, etc.), then
        // decode character references used by text/x-raw/Pango subtitle payloads.
        let clean = clean_subtitle_text(text);
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

        // Shadow offset in pixels (scales with font size).
        let shadow_dx = (px / 16.0).max(1.0).round() as i32;
        let shadow_dy = shadow_dx;

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

        let stride = canvas_w * 4;
        let mut argb = vec![0u8; stride * canvas_h];

        // ── Pass 1: shadow (dark, offset) ─────────────────────────────
        self.rasterise_lines(
            &font,
            &laid_out,
            &line_widths,
            canvas_w,
            canvas_h,
            block_top,
            ascent,
            line_height,
            shadow_dx,
            shadow_dy,
            &mut argb,
            |off, cov, buf| {
                // Pre-multiplied black at ~70% of glyph coverage.
                let a = (cov * 0.7 * 255.0) as u8;
                // "max" blend so overlapping shadow glyphs don't darken twice.
                // Black premul: B=0 G=0 R=0 A=a
                buf[off + 3] = buf[off + 3].max(a);
            },
        );

        // ── Pass 2: foreground (white, no offset) ─────────────────────
        self.rasterise_lines(
            &font,
            &laid_out,
            &line_widths,
            canvas_w,
            canvas_h,
            block_top,
            ascent,
            line_height,
            0,
            0,
            &mut argb,
            |off, cov, buf| {
                // Pre-multiplied white "over" whatever is already there.
                let fa = (cov * 255.0) as u8;
                if fa == 0 {
                    return;
                }
                let inv = 255u16 - fa as u16;
                // ARGB8888 little-endian: [B, G, R, A]
                buf[off] = (fa as u16 + (buf[off] as u16 * inv / 255)) as u8;
                buf[off + 1] = (fa as u16 + (buf[off + 1] as u16 * inv / 255)) as u8;
                buf[off + 2] = (fa as u16 + (buf[off + 2] as u16 * inv / 255)) as u8;
                buf[off + 3] = (fa as u16 + (buf[off + 3] as u16 * inv / 255)) as u8;
            },
        );

        Some(argb)
    }

    /// Rasterise laid-out glyph lines into `buf` via a caller-supplied
    /// pixel callback `emit(byte_offset, coverage_0_to_1, buf)`.
    #[allow(clippy::too_many_arguments)]
    fn rasterise_lines(
        &self,
        font: &FontRef<'_>,
        laid_out: &[Vec<Glyph>],
        line_widths: &[f32],
        canvas_w: usize,
        canvas_h: usize,
        block_top: f32,
        ascent: f32,
        line_height: f32,
        dx: i32,
        dy: i32,
        buf: &mut [u8],
        emit: impl Fn(usize, f32, &mut [u8]),
    ) {
        let stride = canvas_w * 4;
        for (i, glyphs) in laid_out.iter().enumerate() {
            let lw = line_widths[i];
            let x_off = ((canvas_w as f32 - lw) / 2.0).max(0.0);
            let y_off = block_top + i as f32 * line_height + ascent;

            for glyph in glyphs {
                let positioned = Glyph {
                    position: point(
                        glyph.position.x + x_off + dx as f32,
                        glyph.position.y + y_off + dy as f32,
                    ),
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
                            let off = py as usize * stride + px as usize * 4;
                            emit(off, cov, buf);
                        }
                    });
                }
            }
        }
    }
}

/// Normalize subtitle text before layout.
fn clean_subtitle_text(input: &str) -> String {
    // Strip tags before decoding entities so escaped literal angle brackets
    // (e.g. `&lt;not a tag&gt;`) survive as visible text.
    decode_subtitle_entities(&strip_tags(input))
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

/// Decode XML/HTML character references in subtitle text.
///
/// GStreamer's `text/x-raw` subtitle path can carry Pango/XML-ish markup where
/// punctuation and symbols are represented as character references (for example
/// `&apos;`, `&rsquo;`, `&#39;`, or `&#x2019;`).  Use quick-xml's HTML5 entity table
/// for named references, plus generic decimal/hex numeric references so the full
/// Unicode range can be represented without maintaining a hand-written list.
/// Unknown or malformed references are preserved verbatim instead of causing the
/// whole subtitle line to remain escaped.
fn decode_subtitle_entities(input: &str) -> String {
    if !input.as_bytes().contains(&b'&') {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    while let Some(relative_start) = input[pos..].find('&') {
        let start = pos + relative_start;
        out.push_str(&input[pos..start]);

        let entity_start = start + 1;
        let Some(relative_end) = input[entity_start..].find(';') else {
            out.push_str(&input[start..]);
            return out;
        };
        let end = entity_start + relative_end;
        let entity = &input[entity_start..end];

        if push_decoded_subtitle_entity(entity, &mut out) {
            pos = end + 1;
        } else {
            // Preserve the ampersand and continue scanning after it.  This keeps
            // unknown entities intact while still allowing a later valid entity
            // on the same line to decode (e.g. `AT&T&apos;s`).
            out.push('&');
            pos = entity_start;
        }
    }

    out.push_str(&input[pos..]);
    out
}

fn push_decoded_subtitle_entity(entity: &str, out: &mut String) -> bool {
    if let Some(ch) = decode_numeric_subtitle_entity(entity) {
        out.push(ch);
        true
    } else if let Some(value) = quick_xml::escape::resolve_predefined_entity(entity) {
        out.push_str(value);
        true
    } else {
        false
    }
}

fn decode_numeric_subtitle_entity(entity: &str) -> Option<char> {
    let number = entity.strip_prefix('#')?;
    let (digits, radix) = number
        .strip_prefix('x')
        .or_else(|| number.strip_prefix('X'))
        .map_or((number, 10), |hex| (hex, 16));

    if digits.is_empty() || matches!(digits.as_bytes().first(), Some(b'+' | b'-')) {
        return None;
    }

    let codepoint = u32::from_str_radix(digits, radix).ok()?;
    (codepoint != 0)
        .then_some(codepoint)
        .and_then(char::from_u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_and_numeric_subtitle_entities() {
        assert_eq!(
            clean_subtitle_text(
                "It&apos;s &quot;fine&quot; &amp; dandy &#39;&#x2019;&#X2026; &rsquo;"
            ),
            "It's \"fine\" & dandy '’… ’"
        );
    }

    #[test]
    fn preserves_unknown_or_malformed_entities_while_decoding_later_valid_ones() {
        assert_eq!(
            clean_subtitle_text("AT&T&apos;s &madeup; &unfinished"),
            "AT&T's &madeup; &unfinished"
        );
    }

    #[test]
    fn strips_tags_before_decoding_escaped_angle_brackets() {
        assert_eq!(
            clean_subtitle_text("<i>&lt;literal&gt; &apos;quoted&apos;</i>"),
            "<literal> 'quoted'"
        );
    }
}
