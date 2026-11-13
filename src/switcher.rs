//! Alt+Tab: a list of every window, most recently used first, in the middle of the focused
//! monitor. Hold Alt and press Tab to move down (Shift+Tab up), let go of Alt to switch.
//! Arrows and Enter work too, Escape cancels, and the mouse can pick a row.
//!
//! While it's open no app has the keyboard, as in other desktops; otherwise letting go of Alt
//! would look like a lone Alt tap to the app (Firefox opens its menu bar on that).

use std::cell::RefCell;

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    desktop::Window,
    output::Output,
    utils::{IsAlive, Logical, Point, Rectangle, Transform},
};

use crate::{
    config::{Dir, Mods},
    layout::Rect,
    menu::MenuKey,
    state::{State, toplevel_ids},
    text::{Canvas, Text},
};

/// Space inside the panel's edge.
const PAD: i32 = 8;
const MIN_WIDTH: i32 = 360;
const MAX_WIDTH: i32 = 640;

struct Row {
    title: String,
    app_id: String,
    /// Where the window lives: its workspace, and monitor when there are several.
    place: String,
}

pub struct Switcher {
    /// The windows, in the same order as `rows`.
    windows: Vec<Window>,
    rows: Vec<Row>,
    selected: usize,
    /// Modifiers held when it opened; letting go of all of them picks the selection.
    hold: Mods,
    output: Output,
    /// Top-left corner, global logical coordinates.
    loc: Point<i32, Logical>,
    size: (i32, i32),
    row_h: i32,
    /// How many rows fit; the list scrolls to keep the selection in view.
    visible: usize,
    /// Last painted image: (scale, selected, image).
    cache: RefCell<Option<(i32, usize, MemoryRenderBuffer)>>,
}

impl Switcher {
    pub fn on_output(&self, output: &Output) -> bool {
        &self.output == output
    }

    fn rect(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(self.loc, self.size.into())
    }

    /// The first row shown, so the selection is always on screen.
    fn first(&self) -> usize {
        first_visible(self.selected, self.visible)
    }

    /// Each shown row's index and rectangle, relative to the panel's corner.
    fn shown(&self) -> Vec<(usize, Rect)> {
        let first = self.first();
        (first..(first + self.visible).min(self.rows.len()))
            .enumerate()
            .map(|(k, i)| {
                (
                    i,
                    Rect::new(PAD, PAD + k as i32 * self.row_h, self.size.0 - 2 * PAD, self.row_h),
                )
            })
            .collect()
    }

    fn row_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let (x, y) = (pos.x - self.loc.x as f64, pos.y - self.loc.y as f64);
        self.shown()
            .into_iter()
            .find(|(_, r)| x >= r.x as f64 && x < (r.x + r.w) as f64 && y >= r.y as f64 && y < (r.y + r.h) as f64)
            .map(|(i, _)| i)
    }

    fn step(&mut self, dir: Dir) {
        let n = self.rows.len();
        self.selected = match dir {
            Dir::Next => (self.selected + 1) % n,
            Dir::Prev => (self.selected + n - 1) % n,
        };
    }
}

fn first_visible(selected: usize, visible: usize) -> usize {
    (selected + 1).saturating_sub(visible.max(1))
}

impl State {
    /// The `window-switcher` action: open the switcher, or move along if it's already open.
    pub fn window_switcher(&mut self, dir: Dir) {
        if let Some(s) = &mut self.switcher {
            s.step(dir);
            self.needs_redraw = true;
            return;
        }
        self.open_switcher(dir);
    }

    fn open_switcher(&mut self, dir: Dir) {
        self.close_menu();
        // Most recently focused first, then any window that never had focus.
        let mut windows: Vec<Window> = self
            .focus_history
            .iter()
            .filter(|w| self.monitor_of(w).is_some())
            .cloned()
            .collect();
        for m in &self.monitors {
            for ws in &m.workspaces {
                for e in &ws.entries {
                    if !windows.contains(&e.window) {
                        windows.push(e.window.clone());
                    }
                }
            }
        }
        if windows.is_empty() {
            return;
        }
        self.load_ui_text();
        let Some((_, text)) = &self.ui_text else { return };
        let Some(output) = self.output() else { return };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };

        let several = self.monitors.len() > 1;
        let rows: Vec<Row> = windows
            .iter()
            .map(|w| {
                let (app_id, title) = toplevel_ids(w);
                let mon = self.monitor_of(w);
                let ws = mon.and_then(|m| self.monitors[m].workspaces.iter().position(|ws| ws.contains(w)));
                let place = match (mon, ws) {
                    (Some(m), Some(ws)) if several => format!("{} · {}", self.monitors[m].output.name(), ws + 1),
                    (_, Some(ws)) => format!("workspace {}", ws + 1),
                    _ => String::new(),
                };
                let app_id = app_id.unwrap_or_default();
                let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| app_id.clone());
                Row { title, app_id, place }
            })
            .collect();

        let theme = &self.config.theme;
        let row_h = (theme.font_size * 3.2).round().max(36.0) as i32;
        let widest = rows
            .iter()
            .map(|r| text.width(&r.title, theme.font_size) + text.width(&r.place, theme.font_size * 0.85))
            .fold(0.0f32, f32::max) as i32;
        let w = (widest + row_h + 64 + 2 * PAD).clamp(MIN_WIDTH, MAX_WIDTH.min(geo.size.w - 32).max(MIN_WIDTH));
        let fit = ((geo.size.h * 7 / 10 - 2 * PAD) / row_h).max(1) as usize;
        let visible = rows.len().min(fit);
        let h = visible as i32 * row_h + 2 * PAD;
        let loc = Point::from((geo.loc.x + (geo.size.w - w) / 2, geo.loc.y + (geo.size.h - h) / 2));

        let mut hold = crate::config::mods_from_state(&self.keyboard().modifier_state());
        hold.shift = false;
        let n = rows.len();
        self.switcher = Some(Switcher {
            windows,
            rows,
            // Next starts on the previous window, so a quick Alt+Tab flips between two.
            selected: match dir {
                Dir::Next => 1.min(n - 1),
                Dir::Prev => n - 1,
            },
            hold,
            output,
            loc,
            size: (w, h),
            row_h,
            visible,
            cache: RefCell::new(None),
        });
        // No app gets keys while choosing; see the module comment.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let keyboard = self.keyboard();
        keyboard.set_focus(self, None, serial);
        self.needs_redraw = true;
    }

    /// Switch to the selected window.
    pub fn switcher_pick(&mut self) {
        let Some(s) = self.switcher.take() else { return };
        self.needs_redraw = true;
        if let Some(w) = s.windows.get(s.selected).filter(|w| w.alive()) {
            self.activate_window(w);
        }
        // activate_window skips a window that's already focused; give the keyboard back anyway.
        self.refocus();
    }

    pub fn switcher_cancel(&mut self) {
        if self.switcher.take().is_some() {
            self.needs_redraw = true;
            self.refocus();
        }
    }

    pub fn switcher_key(&mut self, key: MenuKey) {
        match key {
            MenuKey::Close => self.switcher_cancel(),
            MenuKey::Pick => self.switcher_pick(),
            MenuKey::Up | MenuKey::Left => self.window_switcher(Dir::Prev),
            MenuKey::Down | MenuKey::Right => self.window_switcher(Dir::Next),
            MenuKey::Workspace(_) => {}
        }
    }

    /// After each key event: once every modifier that opened it is let go, switch.
    pub fn switcher_check_release(&mut self) {
        let Some(s) = &self.switcher else { return };
        let hold = s.hold;
        if hold == Mods::default() {
            // Opened without modifiers: only Enter, a click or Escape close it.
            return;
        }
        let m = crate::config::mods_from_state(&self.keyboard().modifier_state());
        let still = (hold.alt && m.alt) || (hold.logo && m.logo) || (hold.ctrl && m.ctrl);
        if !still {
            self.switcher_pick();
        }
    }

    pub fn over_switcher(&self, pos: Point<f64, Logical>) -> bool {
        self.switcher.as_ref().is_some_and(|s| s.rect().to_f64().contains(pos))
    }

    /// Pointer over a row selects it.
    pub fn switcher_motion(&mut self, pos: Point<f64, Logical>) {
        let Some(s) = &mut self.switcher else { return };
        if let Some(i) = s.row_at(pos)
            && i != s.selected
        {
            s.selected = i;
            self.needs_redraw = true;
        }
    }

    /// A click while open: on a row switches to it, anywhere else cancels. Always used up.
    pub fn switcher_press(&mut self, pos: Point<f64, Logical>) {
        let Some(s) = &mut self.switcher else { return };
        match s.row_at(pos) {
            Some(i) => {
                s.selected = i;
                self.switcher_pick();
            }
            None if s.rect().to_f64().contains(pos) => {}
            None => self.switcher_cancel(),
        }
    }

    /// Keep the most-recently-used order up to date. Runs once per event-loop pass.
    pub fn track_focus_history(&mut self) {
        self.focus_history.retain(|w| w.alive());
        if self.switcher.is_some() || self.focused_layer.is_some() || self.is_locked() {
            return;
        }
        let Some(w) = self.monitors.get(self.active_mon).and_then(|m| m.ws().focus.clone()) else {
            return;
        };
        if self.focus_history.first() != Some(&w) {
            self.focus_history.retain(|x| *x != w);
            self.focus_history.insert(0, w);
        }
    }

    /// The switcher's image for `output`, if it's open there.
    pub fn switcher_buffer(&self, output: &Output, scale: i32) -> Option<(Point<i32, Logical>, MemoryRenderBuffer)> {
        let s = self.switcher.as_ref().filter(|s| &s.output == output)?;
        let (_, text) = self.ui_text.as_ref()?;
        let scale = scale.max(1);
        let mut cache = s.cache.borrow_mut();
        if let Some((sc, sel, buffer)) = &*cache
            && *sc == scale
            && *sel == s.selected
        {
            return Some((s.loc, buffer.clone()));
        }
        let canvas = paint(s, text, &self.config.theme, scale, &|app_id: &str, px: u32| {
            self.bar.icon(app_id, px)
        });
        let buffer = MemoryRenderBuffer::from_slice(
            &canvas.to_premultiplied_rgba(),
            Fourcc::Abgr8888,
            (canvas.w, canvas.h),
            scale,
            Transform::Normal,
            None,
        );
        *cache = Some((scale, s.selected, buffer.clone()));
        Some((s.loc, buffer))
    }
}

type IconLookup<'a> = dyn Fn(&str, u32) -> Option<std::rc::Rc<crate::icons::Icon>> + 'a;

fn paint(s: &Switcher, text: &Text, theme: &crate::theme::Theme, sc: i32, icon: &IconLookup<'_>) -> Canvas {
    let p = &theme.palette;
    let px = theme.font_size * sc as f32;
    let radius = theme.radius * sc as f32;
    let (w, h) = (s.size.0 * sc, s.size.1 * sc);
    let mut canvas = Canvas::new(w, h, [0.0; 4]);
    canvas.fill_rounded_rect(0, 0, w, h, radius + sc as f32, p.separator);
    canvas.fill_rounded_rect(sc, sc, w - 2 * sc, h - 2 * sc, radius, p.surface);
    let row_radius = (radius - PAD as f32 * sc as f32 / 2.0).max(2.0 * sc as f32);

    for (i, r) in s.shown() {
        let row = &s.rows[i];
        let r = Rect::new(r.x * sc, r.y * sc, r.w * sc, r.h * sc);
        let selected = i == s.selected;
        if selected {
            canvas.fill_rounded_rect(r.x, r.y, r.w, r.h, row_radius, p.selected);
            let bar_h = r.h / 2;
            canvas.fill_rounded_rect(r.x, r.y + (r.h - bar_h) / 2, 3 * sc, bar_h, 1.5 * sc as f32, p.accent);
        }
        // Icon, or the app's first letter on a tile.
        let icon_px = (r.h - 12 * sc).max(8 * sc);
        let ix = r.x + 12 * sc;
        let iy = r.y + (r.h - icon_px) / 2;
        match icon(&row.app_id, icon_px as u32) {
            Some(icon) => canvas.draw_image(ix, iy, icon.size, &icon.rgba),
            None => {
                canvas.fill_rounded_rect(ix, iy, icon_px, icon_px, row_radius.min(icon_px as f32 / 3.0), p.hover);
                let letter: String = row
                    .app_id
                    .chars()
                    .chain(row.title.chars())
                    .next()
                    .map(|c| c.to_uppercase().collect())
                    .unwrap_or_default();
                let size = icon_px as f32 * 0.6;
                let lx = ix as f32 + (icon_px as f32 - text.width(&letter, size)) / 2.0;
                let baseline = text.centered_baseline(iy as f32, icon_px as f32, size);
                text.draw(&mut canvas, &letter, size, (lx, baseline), p.text, (ix, ix + icon_px));
            }
        }
        // Where it lives, on the right; the title gets the rest.
        let right = r.x + r.w - 12 * sc;
        let small = px * 0.85;
        let place_w = text.width(&row.place, small).ceil() as i32;
        let place_x = right - place_w;
        let baseline = text.centered_baseline(r.y as f32, r.h as f32, px);
        text.draw(
            &mut canvas,
            &row.place,
            small,
            (place_x as f32, baseline),
            p.subtext,
            (place_x, right),
        );
        let tx = ix + icon_px + 12 * sc;
        let t_right = place_x - 16 * sc;
        if t_right > tx {
            let title = text.fit(&row.title, px, (t_right - tx) as f32);
            let color = if selected { p.text } else { p.muted };
            text.draw(&mut canvas, &title, px, (tx as f32, baseline), color, (tx, t_right));
        }
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_stays_in_view() {
        assert_eq!(first_visible(0, 5), 0);
        assert_eq!(first_visible(4, 5), 0);
        assert_eq!(first_visible(5, 5), 1);
        assert_eq!(first_visible(9, 5), 5);
        assert_eq!(first_visible(3, 0), 3);
    }

    /// `SWITCHER_PREVIEW=/path/out.rgba cargo test switcher_preview` writes the switcher as raw RGBA.
    #[test]
    fn switcher_preview() {
        let Some(path) = std::env::var_os("SWITCHER_PREVIEW") else {
            return;
        };
        let text = Text::load("sans").expect("font");
        let config = crate::config::Config::default();
        let bar = crate::bar::Bar::new(&config.bar);
        let rows: Vec<Row> = [
            ("uaio@host: ~/git/kiln", "Alacritty", "DP-4 · 1"),
            ("Start Page - Vivaldi", "vivaldi-stable", "DP-5 · 1"),
            ("switcher.rs - kiln - Visual Studio Code", "code", "DP-4 · 2"),
            ("Steam", "steam", "HDMI-A-5 · 1"),
            (
                "A window with a very long title that has to be cut short somewhere",
                "",
                "DP-4 · 3",
            ),
        ]
        .into_iter()
        .map(|(t, a, p)| Row {
            title: t.into(),
            app_id: a.into(),
            place: p.into(),
        })
        .collect();
        let row_h = (config.theme.font_size * 3.2).round().max(36.0) as i32;
        let s = Switcher {
            windows: Vec::new(),
            visible: rows.len(),
            size: (560, rows.len() as i32 * row_h + 2 * PAD),
            rows,
            selected: 1,
            hold: Mods::default(),
            output: Output::new(
                "test".into(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: String::new(),
                    model: String::new(),
                },
            ),
            loc: Point::default(),
            row_h,
            cache: RefCell::new(None),
        };
        let scale = std::env::var("SWITCHER_PREVIEW_SCALE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let canvas = paint(&s, &text, &config.theme, scale, &|a: &str, px: u32| bar.icon(a, px));
        std::fs::write(&path, canvas.to_premultiplied_rgba()).unwrap();
        eprintln!("{}x{}", canvas.w, canvas.h);
    }
}
