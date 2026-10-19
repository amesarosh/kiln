//! CPU text and shape drawing for compositor-drawn UI (the bar, and later title bars and menus).

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};

use crate::config::Color;

pub struct Text {
    font: FontVec,
}

impl Text {
    /// Load a font by fontconfig name ("sans", "Noto Sans") or by file path.
    pub fn load(name: &str) -> Option<Self> {
        for path in wt_config::fonts::candidates(name) {
            let Ok(data) = std::fs::read(&path) else { continue };
            match FontVec::try_from_vec(data) {
                Ok(font) => {
                    tracing::info!("ui font: {}", path.display());
                    return Some(Self { font });
                }
                Err(e) => tracing::warn!("{}: not a usable font: {e}", path.display()),
            }
        }
        tracing::error!("no usable font found for \"{name}\"; the bar is disabled");
        None
    }

    pub fn width(&self, text: &str, px: f32) -> f32 {
        let font = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                w += font.kern(p, id);
            }
            w += font.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Shorten `text` with an ellipsis so it fits in `max` pixels.
    pub fn fit(&self, text: &str, px: f32, max: f32) -> String {
        if self.width(text, px) <= max {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
            if self.width(&candidate, px) <= max {
                return candidate;
            }
        }
        String::new()
    }

    /// Baseline that vertically centers a line of text in a box of height `h` starting at `y`.
    pub fn centered_baseline(&self, y: f32, h: f32, px: f32) -> f32 {
        let font = self.font.as_scaled(PxScale::from(px));
        let (ascent, descent) = (font.ascent(), font.descent());
        y + (h - (ascent - descent)) / 2.0 + ascent
    }

    /// Draw `text` with its left edge and baseline at `at`, clipped to `clip` (x0, x1).
    pub fn draw(&self, canvas: &mut Canvas, text: &str, px: f32, at: (f32, f32), color: Color, clip: (i32, i32)) {
        let (x, baseline) = at;
        let scale = PxScale::from(px);
        let font = self.font.as_scaled(scale);
        let mut caret = x;
        let mut prev = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                caret += font.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, point(caret, baseline));
            caret += font.h_advance(id);
            prev = Some(id);
            let Some(outline) = self.font.outline_glyph(glyph) else {
                continue;
            };
            let b = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px_x = b.min.x as i32 + gx as i32;
                let px_y = b.min.y as i32 + gy as i32;
                if px_x >= clip.0 && px_x < clip.1 {
                    canvas.blend(px_x, px_y, color, coverage);
                }
            });
        }
    }
}

/// An RGBA image in straight (non-premultiplied) alpha, converted when uploaded.
pub struct Canvas {
    pub w: i32,
    pub h: i32,
    px: Vec<Color>,
}

impl Canvas {
    pub fn new(w: i32, h: i32, fill: Color) -> Self {
        Self {
            w,
            h,
            px: vec![fill; (w.max(0) * h.max(0)) as usize],
        }
    }

    pub fn blend(&mut self, x: i32, y: i32, src: Color, coverage: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = src[3] * coverage.clamp(0.0, 1.0);
        let dst = &mut self.px[(y * self.w + x) as usize];
        let out_a = a + dst[3] * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        for i in 0..3 {
            dst[i] = (src[i] * a + dst[i] * dst[3] * (1.0 - a)) / out_a;
        }
        dst[3] = out_a;
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                self.blend(xx, yy, color, 1.0);
            }
        }
    }

    /// A rectangle with rounded, anti-aliased corners. `r` is clamped to fit.
    pub fn fill_rounded_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = r.clamp(0.0, w.min(h) as f32 / 2.0);
        let (x0, y0) = (x as f32 + r, y as f32 + r);
        let (x1, y1) = ((x + w) as f32 - r, (y + h) as f32 - r);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let (px, py) = (xx as f32 + 0.5, yy as f32 + 0.5);
                // Distance outside the inner rectangle; only corners have any.
                let dx = (x0 - px).max(px - x1).max(0.0);
                let dy = (y0 - py).max(py - y1).max(0.0);
                let coverage = if r > 0.0 {
                    (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                if coverage > 0.0 {
                    self.blend(xx, yy, color, coverage);
                }
            }
        }
    }

    /// Draw a square premultiplied-RGBA image with its top-left corner at (x, y).
    pub fn draw_image(&mut self, x: i32, y: i32, size: u32, rgba: &[u8]) {
        for (i, p) in rgba.chunks_exact(4).enumerate() {
            let a = p[3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            let (ix, iy) = ((i as u32 % size) as i32, (i as u32 / size) as i32);
            let straight = [
                p[0] as f32 / 255.0 / a,
                p[1] as f32 / 255.0 / a,
                p[2] as f32 / 255.0 / a,
                1.0,
            ];
            self.blend(x + ix, y + iy, straight, a);
        }
    }

    /// Bytes for `Fourcc::Abgr8888` (R, G, B, A in memory), premultiplied as the renderer expects.
    pub fn to_premultiplied_rgba(&self) -> Vec<u8> {
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        self.px
            .iter()
            .flat_map(|c| [to_u8(c[0] * c[3]), to_u8(c[1] * c[3]), to_u8(c[2] * c[3]), to_u8(c[3])])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_over_opaque_stays_opaque() {
        let mut c = Canvas::new(1, 1, [0.0, 0.0, 0.0, 1.0]);
        c.blend(0, 0, [1.0, 1.0, 1.0, 1.0], 0.5);
        let px = c.to_premultiplied_rgba();
        assert_eq!(px[3], 255);
        assert!((127..=128).contains(&px[0]));
    }

    #[test]
    fn out_of_bounds_is_ignored() {
        let mut c = Canvas::new(2, 2, [0.0; 4]);
        c.fill_rect(-5, -5, 100, 100, [1.0, 0.0, 0.0, 1.0]);
        c.blend(9, 9, [1.0; 4], 1.0);
        assert_eq!(c.to_premultiplied_rgba().len(), 16);
    }
}
