//! XCursor theme loading for backends that draw the pointer themselves.

use std::collections::HashMap;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

type Image = (MemoryRenderBuffer, Point<i32, Logical>);
/// Raw RGBA pixels, (width, height) and (x, y) hotspot.
pub type RawImage = (Vec<u8>, (u16, u16), (u16, u16));

pub struct Cursor {
    theme: CursorTheme,
    size: u32,
    /// Loaded per icon name and scale; only the first frame is used so an idle desktop never
    /// redraws.
    cache: HashMap<(&'static str, i32), Image>,
}

impl Cursor {
    pub fn new() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        Self {
            theme: CursorTheme::load(&name),
            size,
            cache: HashMap::new(),
        }
    }

    /// Buffer and hotspot for a named cursor, falling back to the default arrow.
    /// `scale` is the monitor's scale rounded up, so the image is sharp on HiDPI screens.
    pub fn get(&mut self, icon: CursorIcon, scale: i32) -> Image {
        let scale = scale.max(1);
        if let Some(hit) = self.cache.get(&(icon.name(), scale)) {
            return hit.clone();
        }
        let loaded = self
            .load(icon.name(), scale)
            .or_else(|| icon.alt_names().iter().find_map(|n| self.load(n, scale)))
            .or_else(|| self.load("default", scale))
            .or_else(|| self.load("left_ptr", scale))
            .unwrap_or_else(fallback);
        self.cache.insert((icon.name(), scale), loaded.clone());
        loaded
    }

    /// The theme's default arrow as raw RGBA, its size and its hotspot (for Xwayland).
    pub fn default_image(&self) -> Option<RawImage> {
        let path = self
            .theme
            .load_icon("default")
            .or_else(|| self.theme.load_icon("left_ptr"))?;
        let images = parse_xcursor(&std::fs::read(path).ok()?)?;
        let img = images.iter().min_by_key(|i| (i.size as i32 - self.size as i32).abs())?;
        Some((
            img.pixels_rgba.clone(),
            (img.width as u16, img.height as u16),
            (img.xhot as u16, img.yhot as u16),
        ))
    }

    fn load(&self, name: &str, scale: i32) -> Option<Image> {
        let path = self.theme.load_icon(name)?;
        let data = std::fs::read(path).ok()?;
        let images = parse_xcursor(&data)?;
        let want = self.size as i32 * scale;
        let image = images.iter().min_by_key(|img| (img.size as i32 - want).abs())?;
        // Pick the buffer scale from the size we actually got, so a theme without big images
        // still shows a normal-sized cursor.
        let buffer_scale = (image.size as f64 / self.size as f64).round().max(1.0) as i32;
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            buffer_scale,
            Transform::Normal,
            None,
        );
        let hotspot = (image.xhot as i32 / buffer_scale, image.yhot as i32 / buffer_scale);
        Some((buffer, hotspot.into()))
    }
}

/// A plain arrow, used when no cursor theme is installed.
fn fallback() -> Image {
    const W: usize = 12;
    const H: usize = 18;
    let mut px = vec![0u8; W * H * 4];
    for y in 0..H {
        // Left-aligned triangle, narrowing towards the bottom.
        let width = if y < 12 {
            y + 1
        } else {
            12usize.saturating_sub((y - 11) * 2)
        };
        for x in 0..width.min(W) {
            let edge = x == 0 || x + 1 == width || y + 1 == H || (y < 12 && x == y);
            let i = (y * W + x) * 4;
            let v = if edge { 0 } else { 255 };
            px[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    let buffer =
        MemoryRenderBuffer::from_slice(&px, Fourcc::Abgr8888, (W as i32, H as i32), 1, Transform::Normal, None);
    (buffer, (0, 0).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_cursor_from_installed_theme() {
        if !std::path::Path::new("/usr/share/icons/Adwaita/cursors").exists() {
            return; // no theme on this machine; the fallback path covers it
        }
        let c = Cursor {
            theme: CursorTheme::load("Adwaita"),
            size: 24,
            cache: HashMap::new(),
        };
        let (_, hotspot) = c.load("default", 1).expect("Adwaita default cursor should load");
        assert!(hotspot.x >= 0 && hotspot.y >= 0);
    }

    #[test]
    fn unknown_icons_fall_back_to_default() {
        let mut c = Cursor::new();
        c.get(CursorIcon::Default, 1);
        c.get(CursorIcon::ZoomIn, 2);
        assert!(c.cache.contains_key(&("default", 1)));
        assert!(c.cache.contains_key(&("zoom-in", 2)));
    }
}
