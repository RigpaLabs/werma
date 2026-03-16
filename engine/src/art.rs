use std::sync::{Mutex, OnceLock};

// Embedded PNG — compiled in at build time
const PNG_DATA: &[u8] = include_bytes!("../assets/werma.png");

const MIN_ART_WIDTH: usize = 20;
const MAX_ART_WIDTH: usize = 80;

// Decoded pixel buffer: (width, height, rgba_bytes)
static DECODED: OnceLock<(u32, u32, Vec<u8>)> = OnceLock::new();

// Cache: last rendered (term_width, string)
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

/// True if this pixel should be treated as transparent (use terminal background).
#[inline]
fn is_transparent(r: u8, g: u8, b: u8, a: u8) -> bool {
    // Explicit alpha channel or near-black / near-dark-navy pixels
    a < 32 || (r < 20 && g < 20 && b < 50)
}

/// Render the mascot as a halfblock string, centered for `term_width` columns.
/// Returns an empty string if term_width is too small or decoding failed.
pub fn render_art(term_width: usize) -> String {
    if term_width < MIN_ART_WIDTH {
        return String::new();
    }

    // Fast path: return cached render if width hasn't changed
    if let Ok(guard) = CACHE.lock() {
        if guard.0 == term_width && !guard.1.is_empty() {
            return guard.1.clone();
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
            let (br, bg, bb, ba) = if bot_y < dst_h_px {
                sample_pixel(rgba, *src_w, *src_h, col, bot_y, dst_w, dst_h_px)
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

    // Store in cache
    if let Ok(mut guard) = CACHE.lock() {
        *guard = (term_width, buf.clone());
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_art_too_narrow_returns_empty() {
        assert_eq!(render_art(10), "");
        assert_eq!(render_art(0), "");
    }

    #[test]
    fn render_art_normal_width_returns_nonempty() {
        let s = render_art(80);
        // Should produce ANSI output with newlines
        assert!(!s.is_empty());
        assert!(s.contains('\n'));
    }

    #[test]
    fn render_art_caches_result() {
        let first = render_art(80);
        let second = render_art(80);
        assert_eq!(first, second);
    }

    #[test]
    fn render_art_different_widths() {
        let narrow = render_art(40);
        let wide = render_art(120);
        // Wider terminal should produce more content
        assert!(wide.len() > narrow.len());
    }
}
