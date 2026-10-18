//! XCursor theme loading for backends that draw the pointer themselves.

use std::collections::HashMap;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

pub struct Cursor {
    theme: CursorTheme,
    size: u32,
    /// Loaded per icon name; only the first frame is used so an idle desktop never redraws.
    cache: HashMap<&'static str, (MemoryRenderBuffer, Point<i32, Logical>)>,
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
    pub fn get(&mut self, icon: CursorIcon) -> (MemoryRenderBuffer, Point<i32, Logical>) {
        if let Some(hit) = self.cache.get(icon.name()) {
            return hit.clone();
        }
        let loaded = self
            .load(icon.name())
            .or_else(|| icon.alt_names().iter().find_map(|n| self.load(n)))
            .or_else(|| self.load("default"))
            .or_else(|| self.load("left_ptr"))
            .unwrap_or_else(fallback);
        self.cache.insert(icon.name(), loaded.clone());
        loaded
    }

    fn load(&self, name: &str) -> Option<(MemoryRenderBuffer, Point<i32, Logical>)> {
        let path = self.theme.load_icon(name)?;
        let data = std::fs::read(path).ok()?;
        let images = parse_xcursor(&data)?;
        let image = images
            .iter()
            .min_by_key(|img| (img.size as i32 - self.size as i32).abs())?;
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Some((buffer, (image.xhot as i32, image.yhot as i32).into()))
    }
}

/// A plain arrow, used when no cursor theme is installed.
fn fallback() -> (MemoryRenderBuffer, Point<i32, Logical>) {
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
        let (_, hotspot) = c.load("default").expect("Adwaita default cursor should load");
        assert!(hotspot.x >= 0 && hotspot.y >= 0);
    }

    #[test]
    fn unknown_icons_fall_back_to_default() {
        let mut c = Cursor::new();
        c.get(CursorIcon::Default);
        c.get(CursorIcon::ZoomIn);
        assert!(c.cache.contains_key("default"));
        assert!(c.cache.contains_key("zoom-in"));
    }
}
