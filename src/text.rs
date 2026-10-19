//! CPU text and shape drawing for compositor-drawn UI (the bar, the switcher and menus).
//!
//! Glyphs are hinted (snapped to the pixel grid by the font's own instructions, or the
//! autohinter) and placed at whole pixels, so stems come out as solid lines instead of
//! being smeared across two pixels. `ab_glyph` supplies the metrics and kerning.
//!
//! When fontconfig sets a subpixel order (as most desktops do), glyphs are drawn at three
//! times the width and each color stripe of a pixel gets its own coverage, like FreeType's
//! LCD rendering. Small text keeps its shape that way instead of turning to gray mush.
//!
//! Text is mixed into the background with a gamma curve and a slight contrast push (see
//! `text_coverage`), as FreeType and Skia do, so light text on a dark bar keeps solid edges
//! instead of looking thin and faint.

use std::cell::RefCell;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use ab_glyph_rasterizer::{Point, Rasterizer, point};
use skrifa::{
    FontRef, GlyphId, MetadataProvider,
    instance::{LocationRef, Size},
    outline::{DrawSettings, HintingInstance, HintingOptions, OutlinePen, SmoothMode, Target},
};

use crate::config::Color;
use wt_config::fonts::Subpixel;

/// FreeType's default LCD filter, spreading each subpixel over its neighbours to tame color fringes.
const LCD_FILTER: [f32; 5] = [0x08 as f32 / 256.0, 0x4D as f32 / 256.0, 0x56 as f32 / 256.0, 0x4D as f32 / 256.0, 0x08 as f32 / 256.0];

pub struct Text {
    font: FontVec,
    /// The raw font file, for hinted outlines.
    data: Vec<u8>,
    /// Hinting set up for the last pixel size drawn; rebuilding it runs the font's setup programs.
    hinting: RefCell<Option<(f32, HintingInstance)>>,
    subpixel: Subpixel,
}

impl Text {
    /// Load a font by fontconfig name ("sans", "Noto Sans") or by file path.
    pub fn load(name: &str) -> Option<Self> {
        for path in wt_config::fonts::candidates(name) {
            let Ok(data) = std::fs::read(&path) else { continue };
            if FontRef::from_index(&data, 0).is_err() {
                tracing::warn!("{}: not a usable font", path.display());
                continue;
            }
            match FontVec::try_from_vec(data.clone()) {
                Ok(font) => {
                    let subpixel = wt_config::fonts::subpixel(name);
                    tracing::info!("ui font: {} (subpixel: {subpixel:?})", path.display());
                    return Some(Self { font, data, hinting: RefCell::new(None), subpixel });
                }
                Err(e) => tracing::warn!("{}: not a usable font: {e}", path.display()),
            }
        }
        tracing::error!("no usable font found for \"{name}\"; the bar is disabled");
        None
    }

    /// Kerning before `id` and its advance after, both rounded so every glyph starts on a
    /// whole pixel. `width` and `draw` must agree on these.
    fn spacing(font: &ab_glyph::PxScaleFont<&FontVec>, prev: Option<ab_glyph::GlyphId>, id: ab_glyph::GlyphId) -> (f32, f32) {
        let kern = prev.map_or(0.0, |p| font.kern(p, id));
        (kern.round(), font.h_advance(id).round())
    }

    pub fn width(&self, text: &str, px: f32) -> f32 {
        let font = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev = None;
        for c in text.chars() {
            let id = font.glyph_id(c);
            let (kern, advance) = Self::spacing(&font, prev, id);
            w += kern + advance;
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
        let (x, baseline) = (at.0.round(), at.1.round());
        let font = self.font.as_scaled(PxScale::from(px));
        let Ok(sk) = FontRef::from_index(&self.data, 0) else { return };
        let outlines = sk.outline_glyphs();
        // ab_glyph sizes text by ascent-to-descent height; skrifa by the em square.
        let ppem = px * self.font.units_per_em().unwrap_or(1000.0) / self.font.height_unscaled();

        let mut hinting = self.hinting.borrow_mut();
        if hinting.as_ref().is_none_or(|(size, _)| *size != ppem) {
            let options = HintingOptions::from(Target::Smooth {
                mode: if self.subpixel == Subpixel::None { SmoothMode::Normal } else { SmoothMode::Lcd },
                symmetric_rendering: true,
                // Keep the font's own advances, so `width` (from ab_glyph) stays exact.
                preserve_linear_metrics: true,
            });
            *hinting = HintingInstance::new(&outlines, Size::new(ppem), LocationRef::default(), options)
                .map(|h| (ppem, h))
                .inspect_err(|e| tracing::warn!("text hinting unavailable: {e}"))
                .ok();
        }

        let mut caret = x;
        let mut prev = None;
        let mut path = Path::default();
        for c in text.chars() {
            let id = font.glyph_id(c);
            let (kern, advance) = Self::spacing(&font, prev, id);
            caret += kern;
            let origin = caret;
            caret += advance;
            prev = Some(id);

            let Some(glyph) = outlines.get(GlyphId::new(id.0 as u32)) else { continue };
            let settings = match hinting.as_ref() {
                Some((_, h)) => DrawSettings::hinted(h, false),
                None => DrawSettings::unhinted(Size::new(ppem), LocationRef::default()),
            };
            path.clear();
            if glyph.draw(settings, &mut path).is_err() {
                continue;
            }
            match self.subpixel {
                Subpixel::None => path.fill(origin, baseline, |px_x, px_y, coverage| {
                    if px_x >= clip.0 && px_x < clip.1 {
                        canvas.blend_text(px_x, px_y, color, coverage);
                    }
                }),
                order => path.fill_lcd(origin, baseline, |px_x, px_y, [a, b, c]| {
                    if px_x >= clip.0 && px_x < clip.1 {
                        let rgb = if order == Subpixel::Bgr { [c, b, a] } else { [a, b, c] };
                        canvas.blend_text_lcd(px_x, px_y, color, rgb);
                    }
                }),
            }
        }
    }
}

/// One glyph's outline in pixels, y pointing up from the baseline.
#[derive(Default)]
struct Path {
    segments: Vec<Segment>,
    start: (f32, f32),
    last: (f32, f32),
}

enum Segment {
    Line([(f32, f32); 2]),
    Quad([(f32, f32); 3]),
    Cubic([(f32, f32); 4]),
}

impl Path {
    fn clear(&mut self) {
        self.segments.clear();
    }

    /// Bounds of the outline in pixels as (x0, y0, x1, y1), y pointing down; None if empty.
    fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let points = self.segments.iter().flat_map(|s| match s {
            Segment::Line(p) => p.as_slice(),
            Segment::Quad(p) => p.as_slice(),
            Segment::Cubic(p) => p.as_slice(),
        });
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(px, py) in points {
            x0 = x0.min(px);
            x1 = x1.max(px);
            y0 = y0.min(-py);
            y1 = y1.max(-py);
        }
        (x0 <= x1 && y0 <= y1).then_some((x0, y0, x1, y1))
    }

    fn rasterize(&self, w: usize, h: usize, to: impl Fn(&(f32, f32)) -> Point) -> Rasterizer {
        let mut r = Rasterizer::new(w, h);
        for s in &self.segments {
            match s {
                Segment::Line([a, b]) => r.draw_line(to(a), to(b)),
                Segment::Quad([a, b, c]) => r.draw_quad(to(a), to(b), to(c)),
                Segment::Cubic([a, b, c, d]) => r.draw_cubic(to(a), to(b), to(c), to(d)),
            }
        }
        r
    }

    /// Like `fill`, but with a coverage per subpixel stripe (left, middle, right) of each pixel.
    /// `x` must be a whole pixel.
    fn fill_lcd(&self, x: f32, baseline: f32, mut put: impl FnMut(i32, i32, [f32; 3])) {
        let Some((x0, y0, x1, y1)) = self.bounds() else { return };
        // One spare pixel each side: the filter spreads coverage two subpixels outwards.
        let (left, top) = (x0.floor() - 1.0, y0.floor());
        let wpx = (x1.ceil() - left) as usize + 2;
        let (w, h) = (wpx * 3, (y1.ceil() - top) as usize + 1);
        let r = self.rasterize(w, h, |&(px, py)| point((px - left) * 3.0, -py - top));
        let mut cov = vec![0.0f32; w * h];
        r.for_each_pixel_2d(|gx, gy, c| cov[gy as usize * w + gx as usize] = c.min(1.0));

        let (ox, oy) = ((x + left) as i32, (baseline + top) as i32);
        let mut row = vec![0.0f32; w];
        for gy in 0..h {
            let line = &cov[gy * w..(gy + 1) * w];
            for (i, out) in row.iter_mut().enumerate() {
                *out = LCD_FILTER
                    .iter()
                    .enumerate()
                    .filter_map(|(k, f)| (i + k).checked_sub(2).and_then(|j| line.get(j)).map(|c| c * f))
                    .sum();
            }
            for p in 0..wpx {
                let rgb = [row[p * 3], row[p * 3 + 1], row[p * 3 + 2]];
                if rgb.iter().any(|&c| c > 0.0) {
                    put(ox + p as i32, oy + gy as i32, rgb);
                }
            }
        }
    }

    /// Rasterize with the glyph origin at pixel (`x`, `baseline`), calling `put` per covered pixel.
    fn fill(&self, x: f32, baseline: f32, mut put: impl FnMut(i32, i32, f32)) {
        let Some((x0, y0, x1, y1)) = self.bounds() else { return }; // e.g. a space
        let (left, top) = (x0.floor(), y0.floor());
        let (w, h) = ((x1.ceil() - left) as usize + 1, (y1.ceil() - top) as usize + 1);
        let r = self.rasterize(w, h, |&(px, py)| point(px - left, -py - top));
        let (ox, oy) = ((x + left) as i32, (baseline + top) as i32);
        r.for_each_pixel_2d(|gx, gy, coverage| {
            if coverage > 0.0 {
                put(ox + gx as i32, oy + gy as i32, coverage);
            }
        });
    }
}

impl OutlinePen for Path {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = (x, y);
        self.last = (x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.segments.push(Segment::Line([self.last, (x, y)]));
        self.last = (x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.segments.push(Segment::Quad([self.last, (cx0, cy0), (x, y)]));
        self.last = (x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.segments.push(Segment::Cubic([self.last, (cx0, cy0), (cx1, cy1), (x, y)]));
        self.last = (x, y);
    }

    fn close(&mut self) {
        if self.last != self.start {
            self.segments.push(Segment::Line([self.last, self.start]));
        }
        self.last = self.start;
    }
}

/// Gamma text is mixed in. 2.2 is true linear light; a bit lower keeps light-on-dark text
/// from looking bold, as FreeType's and Skia's text gamma do.
const TEXT_GAMMA: f32 = 1.8;
/// How much partly covered pixels are pushed towards solid, for crisper edges.
const TEXT_CONTRAST: f32 = 0.2;
/// Extra darkening for dark text, which mixing in linear light otherwise thins out.
const DARK_TEXT_BOOST: f32 = 0.4;

fn to_linear(c: f32) -> f32 {
    c.clamp(0.0, 1.0).powf(TEXT_GAMMA)
}

fn to_srgb(c: f32) -> f32 {
    c.clamp(0.0, 1.0).powf(1.0 / TEXT_GAMMA)
}

/// Glyph coverage adjusted for display: a contrast push, plus more for dark text
/// (the full boost for black, none for white).
fn text_coverage(c: f32, src: Color) -> f32 {
    let lum = 0.2126 * to_linear(src[0]) + 0.7152 * to_linear(src[1]) + 0.0722 * to_linear(src[2]);
    let c = c.clamp(0.0, 1.0).powf(1.0 / (1.0 + DARK_TEXT_BOOST * (1.0 - lum)));
    (c + TEXT_CONTRAST * c * (1.0 - c)).min(1.0)
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

    /// Blend text: like `blend`, but mixed in linear light with the coverage tuned by
    /// `text_coverage`, so glyph edges keep their weight on any background.
    pub fn blend_text(&mut self, x: i32, y: i32, src: Color, coverage: f32) {
        self.blend_text_lcd(x, y, src, [coverage; 3]);
    }

    /// Blend text with a separate coverage per color channel (subpixel text). Only an opaque
    /// pixel can take that; over a see-through one the channels are averaged instead.
    pub fn blend_text_lcd(&mut self, x: i32, y: i32, src: Color, rgb: [f32; 3]) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let dst = &mut self.px[(y * self.w + x) as usize];
        let rgb = if dst[3] < 0.999 { [(rgb[0] + rgb[1] + rgb[2]) / 3.0; 3] } else { rgb };
        let rgb = rgb.map(|c| text_coverage(c, src) * src[3]);
        // Straight alpha: the channels share one alpha, so use their average for it.
        let a = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
        let out_a = a + dst[3] * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        for i in 0..3 {
            let mixed = to_linear(src[i]) * rgb[i] + to_linear(dst[i]) * dst[3] * (1.0 - rgb[i]);
            dst[i] = to_srgb(mixed / (rgb[i] + dst[3] * (1.0 - rgb[i])).max(f32::EPSILON));
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
    fn light_text_edges_are_brighter_than_plain_mixing() {
        let mut c = Canvas::new(1, 1, [0.0, 0.0, 0.0, 1.0]);
        c.blend_text(0, 0, [1.0, 1.0, 1.0, 1.0], 0.5);
        let px = c.to_premultiplied_rgba();
        assert_eq!(px[3], 255);
        assert!(px[0] > 160, "half-covered white on black: {}", px[0]);
    }

    #[test]
    fn full_coverage_text_is_exact() {
        let mut c = Canvas::new(1, 1, [0.2, 0.3, 0.4, 1.0]);
        c.blend_text(0, 0, [0.9, 0.8, 0.7, 1.0], 1.0);
        let px = c.to_premultiplied_rgba();
        assert_eq!(&px[..3], &[230, 204, 179]);
    }

    #[test]
    fn out_of_bounds_is_ignored() {
        let mut c = Canvas::new(2, 2, [0.0; 4]);
        c.fill_rect(-5, -5, 100, 100, [1.0, 0.0, 0.0, 1.0]);
        c.blend(9, 9, [1.0; 4], 1.0);
        assert_eq!(c.to_premultiplied_rgba().len(), 16);
    }
}

