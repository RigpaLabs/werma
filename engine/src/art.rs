use std::sync::{Mutex, OnceLock};

// Embedded PNG — compiled in at build time
const PNG_DATA: &[u8] = include_bytes!("../assets/werma.png");

const MIN_ART_WIDTH: usize = 20;
const MAX_ART_WIDTH: usize = 80;

// Decoded pixel buffer: (width, height, rgba_bytes)
static DECODED: OnceLock<(u32, u32, Vec<u8>)> = OnceLock::new();

// Cache: last rendered (term_width, string) — only used for tick == 0 (static mode)
static CACHE: Mutex<(usize, String)> = Mutex::new((0, String::new()));

fn decode_png() -> &'static (u32, u32, Vec<u8>) {
    DECODED.get_or_init(|| {
        let decoder = png::Decoder::new(PNG_DATA);
        let mut reader = match decoder.read_info() {
            Ok(r) => r,
            Err(_) => return (0, 0, vec![]),
        };
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = match reader.next_frame(&mut buf) {
            Ok(i) => i,
            Err(_) => return (0, 0, vec![]),
        };
        let src_w = info.width;
        let src_h = info.height;
        let bytes = &buf[..info.buffer_size()];

        // Normalise to RGBA regardless of source color type
        let rgba = to_rgba(bytes, src_w, src_h, &info.color_type, info.bit_depth);
        (src_w, src_h, rgba)
    })
}

/// Convert any supported PNG color type to raw RGBA bytes.
fn to_rgba(
    src: &[u8],
    w: u32,
    h: u32,
    color_type: &png::ColorType,
    bit_depth: png::BitDepth,
) -> Vec<u8> {
    let pixels = (w * h) as usize;
    let mut out = vec![255u8; pixels * 4];

    match color_type {
        png::ColorType::Rgba => {
            out.copy_from_slice(&src[..pixels * 4]);
        }
        png::ColorType::Rgb => {
            for i in 0..pixels {
                out[i * 4] = src[i * 3];
                out[i * 4 + 1] = src[i * 3 + 1];
                out[i * 4 + 2] = src[i * 3 + 2];
                out[i * 4 + 3] = 255;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..pixels {
                let v = src[i * 2];
                out[i * 4] = v;
                out[i * 4 + 1] = v;
                out[i * 4 + 2] = v;
                out[i * 4 + 3] = src[i * 2 + 1];
            }
        }
        png::ColorType::Grayscale => {
            for i in 0..pixels {
                let v = match bit_depth {
                    png::BitDepth::Sixteen => {
                        // big-endian u16 → u8
                        let idx = i * 2;
                        if idx + 1 < src.len() {
                            (((src[idx] as u16) << 8 | src[idx + 1] as u16) >> 8) as u8
                        } else {
                            0
                        }
                    }
                    _ => src[i],
                };
                out[i * 4] = v;
                out[i * 4 + 1] = v;
                out[i * 4 + 2] = v;
                out[i * 4 + 3] = 255;
            }
        }
        // Indexed color — treat as grayscale fallback
        png::ColorType::Indexed => {
            for i in 0..pixels {
                let v = if i < src.len() { src[i] } else { 0 };
                out[i * 4] = v;
                out[i * 4 + 1] = v;
                out[i * 4 + 2] = v;
                out[i * 4 + 3] = 255;
            }
        }
    }
    out
}

/// Sample a single RGBA pixel from the source buffer using nearest-neighbor.
#[inline]
fn sample_pixel(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_x: u32,
    dst_y: u32,
    dst_w: u32,
    dst_h: u32,
) -> (u8, u8, u8, u8) {
    let sx = (dst_x as u64 * src_w as u64 / dst_w as u64).min(src_w as u64 - 1) as u32;
    let sy = (dst_y as u64 * src_h as u64 / dst_h as u64).min(src_h as u64 - 1) as u32;
    let idx = ((sy * src_w + sx) * 4) as usize;
    if idx + 3 < rgba.len() {
        (rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3])
    } else {
        (0, 0, 0, 0)
    }
}

/// Apply procedural animation effects to a pixel based on its color class and tick.
///
/// Three effects, all deterministic per (tick, x, y) — no RNG needed:
/// - Flame flicker: fire/orange pixels get random brightness shifts
/// - Eye glow pulse: bright white/blue pixels breathe on a sine wave
/// - Golden shimmer: gold/yellow armor pixels get subtle hue shifts
#[inline]
fn animate_pixel(r: u8, g: u8, b: u8, a: u8, tick: u64, x: u32, y: u32) -> (u8, u8, u8, u8) {
    if tick == 0 {
        return (r, g, b, a);
    }

    // Flame flicker: fire/orange pixels
    if r > 180 && g > 60 && g < 180 && b < 80 {
        let hash = (tick
            .wrapping_mul(7)
            .wrapping_add(x as u64 * 13)
            .wrapping_add(y as u64 * 31))
            % 40;
        let offset = hash as i16 - 20;
        let nr = (r as i16 + offset).clamp(0, 255) as u8;
        let ng = (g as i16 + offset / 2).clamp(0, 255) as u8;
        return (nr, ng, b, a);
    }

    // Eye glow pulse: bright white/light-blue pixels (~2s period at 100ms ticks = 20 ticks)
    if r > 200 && g > 200 && b > 200 {
        let phase = (tick as f64) * std::f64::consts::PI / 10.0;
        let factor = 1.0 + 0.15 * phase.sin();
        let nr = ((r as f64) * factor).clamp(0.0, 255.0) as u8;
        let ng = ((g as f64) * factor).clamp(0.0, 255.0) as u8;
        let nb = ((b as f64) * factor).clamp(0.0, 255.0) as u8;
        return (nr, ng, nb, a);
    }

    // Golden shimmer: gold/yellow armor pixels
    if r > 180 && g > 120 && g < 200 && b < 100 {
        let hash = (tick
            .wrapping_mul(3)
            .wrapping_add(x as u64 * 17)
            .wrapping_add(y as u64 * 31))
            % 20;
        let shift = hash as i16 - 10;
        let ng = (g as i16 + shift).clamp(0, 255) as u8;
        let nr = (r as i16 + shift / 2).clamp(0, 255) as u8;
        return (nr, ng, b, a);
    }

    (r, g, b, a)
}

/// True if this pixel should be treated as transparent (use terminal background).
#[inline]
fn is_transparent(r: u8, g: u8, b: u8, a: u8) -> bool {
    // Explicit alpha channel or near-black / near-dark-navy pixels
    a < 32 || (r < 20 && g < 20 && b < 50)
}

/// Render the mascot as a halfblock string, centered for `term_width` columns.
/// Pass `tick = 0` for static rendering (cached). Pass `tick > 0` for animated
/// rendering (no cache — re-rendered each frame).
/// Returns an empty string if term_width is too small or decoding failed.
pub fn render_art(term_width: usize, tick: u64) -> String {
    if term_width < MIN_ART_WIDTH {
        return String::new();
    }

    // Fast path: return cached render for static mode only
    if tick == 0 {
        if let Ok(guard) = CACHE.lock() {
            if guard.0 == term_width && !guard.1.is_empty() {
                return guard.1.clone();
            }
        }
    }

    let (src_w, src_h, rgba) = decode_png();
    if *src_w == 0 || *src_h == 0 {
        return String::new();
    }

    let dst_w = MIN_ART_WIDTH.max(term_width.saturating_sub(2).min(MAX_ART_WIDTH)) as u32;
    // Halfblock: each terminal row covers 2 pixel rows.  Preserve aspect ratio.
    let dst_h_px = (dst_w as u64 * *src_h as u64 / *src_w as u64) as u32;
    // Round up so we don't drop the last row
    let half_rows = dst_h_px.div_ceil(2);

    let pad = (term_width.saturating_sub(dst_w as usize)) / 2;
    let padding = " ".repeat(pad);

    let mut buf = String::with_capacity((dst_w as usize * 20 + 2) * half_rows as usize);

    for row in 0..half_rows {
        buf.push_str(&padding);

        let top_y = row * 2;
        let bot_y = top_y + 1;

        for col in 0..dst_w {
            let (tr, tg, tb, ta) = sample_pixel(rgba, *src_w, *src_h, col, top_y, dst_w, dst_h_px);
            let (tr, tg, tb, ta) = animate_pixel(tr, tg, tb, ta, tick, col, top_y);

            let (br, bg, bb, ba) = if bot_y < dst_h_px {
                let (br, bg, bb, ba) =
                    sample_pixel(rgba, *src_w, *src_h, col, bot_y, dst_w, dst_h_px);
                animate_pixel(br, bg, bb, ba, tick, col, bot_y)
            } else {
                (0, 0, 0, 0) // bottom row out of bounds → transparent
            };

            let top_transparent = is_transparent(tr, tg, tb, ta);
            let bot_transparent = is_transparent(br, bg, bb, ba);

            match (top_transparent, bot_transparent) {
                (true, true) => {
                    // Both transparent — emit a plain space
                    buf.push_str("\x1b[0m ");
                }
                (false, false) => {
                    // bg = top pixel color, fg = bottom pixel color, glyph = ▄
                    buf.push_str(&format!(
                        "\x1b[48;2;{tr};{tg};{tb}m\x1b[38;2;{br};{bg};{bb}m\u{2584}"
                    ));
                }
                (true, false) => {
                    // Only bottom half has color — fg color on default bg
                    buf.push_str(&format!("\x1b[49m\x1b[38;2;{br};{bg};{bb}m\u{2584}"));
                }
                (false, true) => {
                    // Only top half has color — use upper-half-block ▀ with fg
                    buf.push_str(&format!("\x1b[49m\x1b[38;2;{tr};{tg};{tb}m\u{2580}"));
                }
            }
        }

        // Reset at end of each line
        buf.push_str("\x1b[0m\n");
    }

    // Store in cache only for static mode
    if tick == 0 {
        if let Ok(mut guard) = CACHE.lock() {
            *guard = (term_width, buf.clone());
        }
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_art_too_narrow_returns_empty() {
        assert_eq!(render_art(10, 0), "");
        assert_eq!(render_art(0, 0), "");
    }

    #[test]
    fn render_art_normal_width_returns_nonempty() {
        let s = render_art(80, 0);
        // Should produce ANSI output with newlines
        assert!(!s.is_empty());
        assert!(s.contains('\n'));
    }

    #[test]
    fn render_art_caches_result() {
        let first = render_art(80, 0);
        let second = render_art(80, 0);
        assert_eq!(first, second);
    }

    #[test]
    fn render_art_different_widths() {
        let narrow = render_art(40, 0);
        let wide = render_art(120, 0);
        // Wider terminal should produce more content
        assert!(wide.len() > narrow.len());
    }

    #[test]
    fn render_art_animates() {
        // Animated frames should differ from the static frame
        let static_frame = render_art(80, 0);
        let animated_frame = render_art(80, 5);
        assert_ne!(
            static_frame, animated_frame,
            "tick=0 and tick=5 should produce different output"
        );
    }
}
