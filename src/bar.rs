//! The built-in bar: workspace buttons, one tab per window, a layout button and a launcher button.
//!
//! The bar is painted on the CPU into a single image, and only repainted when what it shows
//! changes (titles, focus, workspace, hover). Layout is computed the same way for painting and
//! for hit testing, so clicks always land on what's drawn.

use std::{cell::RefCell, collections::HashMap};

use smithay::{
    backend::{
        allocator::Fourcc,
        input::{Axis, AxisSource},
        renderer::element::memory::MemoryRenderBuffer,
    },
    desktop::{Window, layer_map_for_output},
    utils::{Logical, Point, Rectangle, Transform},
    wayland::shell::wlr_layer::Layer,
};

use crate::{
    config::{Action, BarConfig, BarPosition, Color, LayoutChoice},
    layout::{LayoutKind, Rect},
    state::{State, toplevel_ids},
    text::{Canvas, Text},
};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Scroll distance (in v120 units, one wheel notch = 120) that switches one workspace.
const SCROLL_STEP: f64 = 120.0;
/// Widest a window tab gets, in logical pixels.
const MAX_TAB_WIDTH: i32 = 260;

/// A clickable piece of the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Workspace(usize),
    /// Index into the active workspace's window list.
    Tab(usize),
    Layout,
    Launcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsLook {
    Active,
    Occupied,
    Empty,
}

/// Everything the bar's pixels depend on. If this is unchanged, the old image is reused.
#[derive(Debug, Clone, PartialEq)]
struct Model {
    /// Logical size of the bar.
    size: (i32, i32),
    scale: i32,
    position: BarPosition,
    config: BarConfig,
    workspaces: Vec<WsLook>,
    /// (title, focused)
    tabs: Vec<(String, bool)>,
    layout: LayoutKind,
    hover: Option<Part>,
    /// This monitor has keyboard focus; otherwise the focused tab is drawn dimmer.
    active_monitor: bool,
}

pub struct Bar {
    text: Option<Text>,
    font: String,
    /// Last painted image per output name.
    cached: RefCell<HashMap<String, (Model, MemoryRenderBuffer)>>,
    /// The part under the pointer, and the output it's on.
    pub hover: Option<(String, Part)>,
    /// Buttons pressed on the bar, so their release isn't sent to a client either.
    pub held_buttons: Vec<u32>,
    scroll: f64,
}

impl Bar {
    pub fn new(config: &BarConfig) -> Self {
        Self {
            text: config.enabled.then(|| Text::load(&config.font)).flatten(),
            font: config.font.clone(),
            cached: RefCell::new(HashMap::new()),
            hover: None,
            held_buttons: Vec::new(),
            scroll: 0.0,
        }
    }

    pub fn reconfigure(&mut self, config: &BarConfig) {
        if config.enabled && (self.text.is_none() || self.font != config.font) {
            self.text = Text::load(&config.font);
            self.font = config.font.clone();
        }
        self.cached.borrow_mut().clear();
    }

    /// Drop the cached image of a monitor that went away.
    pub fn forget_output(&mut self, name: &str) {
        self.cached.borrow_mut().remove(name);
        if self.hover.as_ref().is_some_and(|(n, _)| n == name) {
            self.hover = None;
        }
    }

    /// Where every part goes, in logical pixels from the bar's top-left corner.
    fn layout(&self, text: &Text, m: &Model) -> Vec<(Rect, Part)> {
        if m.position.vertical() {
            Self::layout_vertical(m)
        } else {
            Self::layout_horizontal(text, m)
        }
    }

    /// Left to right: workspaces, window tabs, layout button, launcher.
    fn layout_horizontal(text: &Text, m: &Model) -> Vec<(Rect, Part)> {
        let (width, h) = m.size;
        let mut parts = Vec::new();

        let mut x = 0;
        for i in 0..m.workspaces.len() {
            parts.push((Rect::new(x, 0, h, h), Part::Workspace(i)));
            x += h;
        }

        let plus_x = width - h;
        parts.push((Rect::new(plus_x, 0, h, h), Part::Launcher));
        let layout_w = text.width("monocle", m.config.font_size).ceil() as i32 + 24;
        let layout_x = plus_x - layout_w;
        parts.push((Rect::new(layout_x, 0, layout_w, h), Part::Layout));

        let (x0, x1) = (x + 8, layout_x - 8);
        let n = m.tabs.len() as i32;
        if n > 0 && x1 > x0 {
            let w = ((x1 - x0) / n).min(MAX_TAB_WIDTH);
            for i in 0..n {
                parts.push((Rect::new(x0 + i * w, 0, (w - 4).max(1), h), Part::Tab(i as usize)));
            }
        }
        parts
    }

    /// Top to bottom: workspaces, one row per window, and the layout button and launcher
    /// sharing the bottom row.
    fn layout_vertical(m: &Model) -> Vec<(Rect, Part)> {
        let (w, height) = m.size;
        let row = m.config.height;
        let mut parts = Vec::new();

        let mut y = 0;
        for i in 0..m.workspaces.len() {
            parts.push((Rect::new(0, y, w, row), Part::Workspace(i)));
            y += row;
        }

        let bottom = height - row;
        parts.push((Rect::new(w - row, bottom, row, row), Part::Launcher));
        parts.push((Rect::new(0, bottom, w - row, row), Part::Layout));

        y += 8;
        for i in 0..m.tabs.len() {
            if y + row > bottom - 8 {
                break;
            }
            parts.push((Rect::new(0, y, w, row), Part::Tab(i)));
            y += row + 2;
        }
        parts
    }

    fn paint(&self, text: &Text, m: &Model) -> MemoryRenderBuffer {
        let canvas = self.paint_canvas(text, m);
        MemoryRenderBuffer::from_slice(
            &canvas.to_premultiplied_rgba(),
            Fourcc::Abgr8888,
            (canvas.w, canvas.h),
            m.scale,
            Transform::Normal,
            None,
        )
    }

    fn paint_canvas(&self, text: &Text, m: &Model) -> Canvas {
        let c = &m.config;
        let p = &c.palette;
        let s = m.scale;
        let vertical = m.position.vertical();
        let px = c.font_size * s as f32;
        let (bw, bh) = (m.size.0 * s, m.size.1 * s);
        let mut canvas = Canvas::new(bw, bh, p.surface);

        // A hairline between the bar and the windows.
        match m.position {
            BarPosition::Top => canvas.fill_rect(0, bh - s, bw, s, p.separator),
            BarPosition::Bottom => canvas.fill_rect(0, 0, bw, s, p.separator),
            BarPosition::Left => canvas.fill_rect(bw - s, 0, s, bh, p.separator),
            BarPosition::Right => canvas.fill_rect(0, 0, s, bh, p.separator),
        }

        let centered = |canvas: &mut Canvas, label: &str, r: Rect, size: f32, color: Color| {
            let tw = text.width(label, size);
            let tx = r.x as f32 + (r.w as f32 - tw) / 2.0;
            let baseline = text.centered_baseline(r.y as f32, r.h as f32, size);
            text.draw(canvas, label, size, (tx, baseline), color, (r.x, r.x + r.w));
        };
        // Highlights leave a margin across the bar and a small gap between neighbours.
        let (gap, margin) = (2 * s, 4 * s);
        let pill = |r: Rect| {
            if vertical {
                Rect::new(r.x + margin, r.y + gap / 2, r.w - 2 * margin, r.h - gap)
            } else {
                Rect::new(r.x + gap, r.y + margin, r.w - 2 * gap, r.h - 2 * margin)
            }
        };
        let fill = |canvas: &mut Canvas, r: Rect, color: Color| canvas.fill_rect(r.x, r.y, r.w, r.h, color);

        for (r, part) in self.layout(text, m) {
            let r = Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
            let hovered = m.hover == Some(part);
            match part {
                Part::Workspace(i) => {
                    let look = m.workspaces[i];
                    let label_color = match look {
                        WsLook::Active => {
                            fill(&mut canvas, pill(r), p.accent);
                            p.on_accent
                        }
                        WsLook::Occupied | WsLook::Empty => {
                            if hovered {
                                fill(&mut canvas, pill(r), p.hover);
                            }
                            if look == WsLook::Occupied { p.text } else { p.subtext }
                        }
                    };
                    centered(&mut canvas, &(i + 1).to_string(), r, px, label_color);
                    // A small mark on workspaces that have windows: under the number on a
                    // horizontal bar, beside it on a vertical one.
                    if look != WsLook::Empty {
                        let mark = if look == WsLook::Active { p.on_accent } else { p.text };
                        let tw = text.width(&(i + 1).to_string(), px) as i32;
                        if vertical {
                            let x = r.x + (r.w + tw) / 2 + 4 * s;
                            canvas.fill_rect(x, r.y + r.h / 2 - s, 3 * s, s.max(2), mark);
                        } else {
                            canvas.fill_rect(r.x + r.w / 2 - 4 * s, r.y + r.h - margin - 3 * s, 8 * s, s.max(2), mark);
                        }
                    }
                }
                Part::Tab(i) => {
                    let (title, focused) = &m.tabs[i];
                    let bg = match (focused, hovered) {
                        (true, _) => p.selected,
                        (false, true) => p.hover,
                        (false, false) => p.raised,
                    };
                    let area = if vertical {
                        Rect::new(r.x + margin, r.y, r.w - 2 * margin, r.h)
                    } else {
                        Rect::new(r.x, r.y + margin, r.w, r.h - 2 * margin)
                    };
                    fill(&mut canvas, area, bg);
                    if *focused {
                        let mark = if m.active_monitor { p.accent } else { p.accent_inactive };
                        // An underline on a horizontal bar, a stripe on the left of a vertical one.
                        if vertical {
                            canvas.fill_rect(area.x, area.y, 2 * s, area.h, mark);
                        } else {
                            canvas.fill_rect(area.x, area.y + area.h - 2 * s, area.w, 2 * s, mark);
                        }
                    }
                    let pad = 8 * s;
                    let label = text.fit(title, px, (area.w - 2 * pad) as f32);
                    let color = if *focused { p.text } else { p.muted };
                    let baseline = text.centered_baseline(r.y as f32, r.h as f32, px);
                    text.draw(
                        &mut canvas,
                        &label,
                        px,
                        ((area.x + pad) as f32, baseline),
                        color,
                        (area.x, area.x + area.w - pad / 2),
                    );
                }
                Part::Layout => {
                    if hovered {
                        fill(&mut canvas, pill(r), p.hover);
                    }
                    let name = match m.layout {
                        LayoutKind::Tile => "tile",
                        LayoutKind::Monocle => "monocle",
                    };
                    centered(&mut canvas, name, r, px, if hovered { p.text } else { p.muted });
                }
                Part::Launcher => {
                    if hovered {
                        fill(&mut canvas, pill(r), p.hover);
                    }
                    centered(&mut canvas, "+", r, px * 1.4, p.text);
                }
            }
        }

        canvas
    }

    /// Turn scroll input into whole workspace steps (+1 = next, -1 = previous).
    fn scroll_steps(&mut self, source: AxisSource, amount: f64, v120: Option<f64>) -> i32 {
        let units = match (v120, source) {
            (Some(v), _) => v,
            // Touchpads: about 50px of finger travel per workspace.
            (None, AxisSource::Finger) => amount * SCROLL_STEP / 50.0,
            (None, _) => amount * SCROLL_STEP / 15.0,
        };
        self.scroll += units;
        let steps = (self.scroll / SCROLL_STEP).trunc();
        self.scroll -= steps * SCROLL_STEP;
        steps as i32
    }
}

impl State {
    /// Area left on a monitor after bars and panels from other programs, in global coordinates.
    fn layer_zone(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        let output = &self.monitors.get(mon)?.output;
        let geo = self.space.output_geometry(output)?;
        let mut zone = layer_map_for_output(output).non_exclusive_zone();
        zone.loc += geo.loc;
        Some(zone)
    }

    /// Where a monitor's bar goes, or `None` if it has none.
    pub fn bar_position(&self, mon: usize) -> Option<BarPosition> {
        self.bar.text.as_ref()?;
        self.config.bar.position_for(&self.monitors.get(mon)?.output.name())
    }

    /// Where a monitor's bar sits, whether or not it's currently hidden by a fullscreen window.
    pub fn bar_reserved(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        let cfg = &self.config.bar;
        let position = self.bar_position(mon)?;
        let zone = self.layer_zone(mon)?;
        let (loc, size) = (zone.loc, zone.size);
        let h = cfg.height.min(size.h);
        let w = cfg.width.min(size.w);
        let rect = match position {
            BarPosition::Top => Rectangle::new(loc, (size.w, h).into()),
            BarPosition::Bottom => Rectangle::new((loc.x, loc.y + size.h - h).into(), (size.w, h).into()),
            BarPosition::Left => Rectangle::new(loc, (w, size.h).into()),
            BarPosition::Right => Rectangle::new((loc.x + size.w - w, loc.y).into(), (w, size.h).into()),
        };
        Some(rect)
    }

    /// A monitor's bar rectangle when it's visible.
    pub fn bar_rect(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        self.bar_reserved(mon)
            .filter(|_| self.monitors[mon].ws().fullscreen.is_none())
    }

    /// Area for windows on a monitor: what other panels leave, minus the bar.
    pub fn work_area(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        let mut zone = self.layer_zone(mon)?;
        if let (Some(bar), Some(position)) = (self.bar_reserved(mon), self.bar_position(mon)) {
            match position {
                BarPosition::Top => {
                    zone.loc.y += bar.size.h;
                    zone.size.h -= bar.size.h;
                }
                BarPosition::Bottom => zone.size.h -= bar.size.h,
                BarPosition::Left => {
                    zone.loc.x += bar.size.w;
                    zone.size.w -= bar.size.w;
                }
                BarPosition::Right => zone.size.w -= bar.size.w,
            }
        }
        Some(zone)
    }

    pub fn over_bar(&self, pos: Point<f64, Logical>) -> bool {
        self.monitor_at(pos)
            .and_then(|mon| self.bar_rect(mon))
            .is_some_and(|r| r.to_f64().contains(pos))
    }

    /// Nothing but wallpaper under the pointer.
    pub fn over_empty_desktop(&self, pos: Point<f64, Logical>) -> bool {
        !self.over_bar(pos)
            && self.space.element_under(pos).is_none()
            && self
                .layer_under(pos)
                .is_none_or(|l| matches!(l.layer(), Layer::Background | Layer::Bottom))
    }

    fn bar_model(&self, mon: usize, size: (i32, i32), scale: i32) -> Model {
        let m = &self.monitors[mon];
        let ws = m.ws();
        let workspaces = m
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| match (i == m.active_ws, w.entries.is_empty()) {
                (true, _) => WsLook::Active,
                (false, false) => WsLook::Occupied,
                (false, true) => WsLook::Empty,
            })
            .collect();
        let tabs = ws
            .entries
            .iter()
            .map(|e| {
                let (app_id, title) = toplevel_ids(&e.window);
                let label = title
                    .filter(|t| !t.is_empty())
                    .or(app_id)
                    .unwrap_or_else(|| "untitled".into());
                (label, ws.focus.as_ref() == Some(&e.window))
            })
            .collect();
        let name = m.output.name();
        Model {
            size,
            scale,
            position: self.bar_position(mon).unwrap_or(BarPosition::Top),
            config: self.config.bar.clone(),
            workspaces,
            tabs,
            layout: ws.layout,
            hover: self.bar.hover.as_ref().filter(|(n, _)| *n == name).map(|(_, p)| *p),
            active_monitor: mon == self.active_mon,
        }
    }

    /// A monitor's bar image at the given integer scale, repainted only when needed.
    pub fn bar_buffer(&self, mon: usize, scale: i32) -> Option<MemoryRenderBuffer> {
        let rect = self.bar_rect(mon)?;
        let text = self.bar.text.as_ref()?;
        let model = self.bar_model(mon, (rect.size.w, rect.size.h), scale.max(1));
        let name = self.monitors[mon].output.name();
        let mut cached = self.bar.cached.borrow_mut();
        if let Some((m, buffer)) = cached.get(&name)
            && *m == model
        {
            return Some(buffer.clone());
        }
        let buffer = self.bar.paint(text, &model);
        cached.insert(name, (model, buffer.clone()));
        Some(buffer)
    }

    /// The monitor and bar part under a point.
    pub fn bar_part_at(&self, pos: Point<f64, Logical>) -> Option<(usize, Part)> {
        let mon = self.monitor_at(pos)?;
        let rect = self.bar_rect(mon)?;
        if !rect.to_f64().contains(pos) {
            return None;
        }
        let text = self.bar.text.as_ref()?;
        let (x, y) = ((pos.x - rect.loc.x as f64) as i32, (pos.y - rect.loc.y as f64) as i32);
        let model = self.bar_model(mon, (rect.size.w, rect.size.h), 1);
        self.bar
            .layout(text, &model)
            .into_iter()
            .find(|(r, _)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
            .map(|(_, part)| (mon, part))
    }

    /// Update which part the pointer is over; repaint if it changed.
    pub fn bar_hover(&mut self, pos: Point<f64, Logical>) {
        let hover = self
            .bar_part_at(pos)
            .map(|(mon, part)| (self.monitors[mon].output.name(), part));
        if hover != self.bar.hover {
            self.bar.hover = hover;
            self.needs_redraw = true;
        }
    }

    /// A click on monitor `mon`'s bar. Clicking a bar also focuses its monitor.
    pub fn bar_click(&mut self, mon: usize, part: Part, button: u32) {
        if mon != self.active_mon {
            self.active_mon = mon;
            self.focused_layer = None;
            self.refocus();
        }
        let tab_window =
            |state: &State, i: usize| -> Option<Window> { state.ws().entries.get(i).map(|e| e.window.clone()) };
        match (part, button) {
            (Part::Workspace(i), BTN_LEFT) => self.switch_workspace(i),
            (Part::Tab(i), BTN_LEFT) => {
                let Some(w) = tab_window(self, i) else { return };
                self.focused_layer = None;
                self.focus_window(&w);
                // In monocle only the focused window is shown, so bring it forward.
                if self.ws().layout == LayoutKind::Monocle {
                    self.arrange();
                }
            }
            (Part::Tab(i), BTN_MIDDLE) => {
                if let Some(w) = tab_window(self, i) {
                    crate::state::close_window(&w);
                }
            }
            (Part::Layout, BTN_LEFT | BTN_RIGHT) => self.run_action(Action::Layout(LayoutChoice::Next)),
            (Part::Launcher, BTN_LEFT) => {
                let cmd = self.config.bar.launcher.clone();
                if !cmd.is_empty() {
                    self.spawn(&cmd);
                }
            }
            _ => {}
        }
    }

    /// Scroll over a bar or empty desktop to change that monitor's workspace. Returns true if it was used.
    pub fn bar_scroll(
        &mut self,
        pos: Point<f64, Logical>,
        source: AxisSource,
        axis: Axis,
        amount: f64,
        v120: Option<f64>,
    ) -> bool {
        if axis != Axis::Vertical || !(self.over_bar(pos) || self.over_empty_desktop(pos)) {
            return false;
        }
        let Some(mon) = self.monitor_at(pos) else { return false };
        let steps = self.bar.scroll_steps(source, amount, v120);
        if steps != 0 {
            self.focus_monitor_at(pos);
            let m = &self.monitors[mon];
            let last = m.workspaces.len() as i32 - 1;
            let target = (m.active_ws as i32 + steps).clamp(0, last) as usize;
            self.switch_workspace(target);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn model(size: (i32, i32), position: BarPosition, tabs: &[(&str, bool)], hover: Option<Part>) -> Model {
        Model {
            size,
            scale: 1,
            position,
            config: Config::default().bar,
            workspaces: vec![
                WsLook::Occupied,
                WsLook::Active,
                WsLook::Empty,
                WsLook::Occupied,
                WsLook::Empty,
            ],
            tabs: tabs.iter().map(|(t, f)| (t.to_string(), *f)).collect(),
            layout: LayoutKind::Tile,
            hover,
            active_monitor: true,
        }
    }

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn layout_parts_do_not_overlap() {
        let Some(text) = Text::load("sans") else { return };
        let bar = Bar::new(&Config::default().bar);
        let tabs = [("one", true), ("two", false), ("three", false)];
        for (size, position) in [
            ((1200, 28), BarPosition::Top),
            ((180, 1400), BarPosition::Left),
            ((180, 1400), BarPosition::Right),
        ] {
            let parts = bar.layout(&text, &model(size, position, &tabs, None));
            for (i, (a, pa)) in parts.iter().enumerate() {
                assert!(
                    a.x >= 0 && a.y >= 0 && a.x + a.w <= size.0 && a.y + a.h <= size.1,
                    "{pa:?} {a:?}"
                );
                for (b, pb) in &parts[i + 1..] {
                    assert!(!overlaps(a, b), "{position:?}: {pa:?} {a:?} overlaps {pb:?} {b:?}");
                }
            }
            assert_eq!(parts.iter().filter(|(_, p)| matches!(p, Part::Tab(_))).count(), 3);
        }
    }

    #[test]
    fn vertical_bar_drops_tabs_that_do_not_fit() {
        let Some(text) = Text::load("sans") else { return };
        let bar = Bar::new(&Config::default().bar);
        let tabs: Vec<(&str, bool)> = (0..50).map(|_| ("w", false)).collect();
        let parts = bar.layout(&text, &model((180, 600), BarPosition::Left, &tabs, None));
        let shown = parts.iter().filter(|(_, p)| matches!(p, Part::Tab(_))).count();
        assert!(shown > 0 && shown < 50, "{shown}");
    }

    #[test]
    fn wheel_notches_step_workspaces() {
        let mut bar = Bar::new(&Config::default().bar);
        assert_eq!(bar.scroll_steps(AxisSource::Wheel, 15.0, Some(120.0)), 1);
        assert_eq!(bar.scroll_steps(AxisSource::Wheel, -15.0, Some(-240.0)), -2);
        // Half a touchpad step does nothing until the rest arrives.
        assert_eq!(bar.scroll_steps(AxisSource::Finger, 25.0, None), 0);
        assert_eq!(bar.scroll_steps(AxisSource::Finger, 25.0, None), 1);
    }

    /// `BAR_PREVIEW=/path/out.rgba cargo test bar_preview` writes the bar as raw RGBA for a quick look.
    #[test]
    fn bar_preview() {
        let Some(path) = std::env::var_os("BAR_PREVIEW") else {
            return;
        };
        let text = Text::load("sans").expect("font");
        let bar = Bar::new(&Config::default().bar);
        let vertical = std::env::var_os("BAR_PREVIEW_VERTICAL").is_some();
        let (size, position) = if vertical {
            ((180, 700), BarPosition::Left)
        } else {
            ((1400, 28), BarPosition::Top)
        };
        let m = model(
            size,
            position,
            &[
                ("uaio@host: ~/git/wayland-time", true),
                ("second terminal", false),
                (
                    "A very long window title that will certainly need to be cut short",
                    false,
                ),
            ],
            Some(Part::Tab(1)),
        );
        let canvas = bar.paint_canvas(&text, &m);
        std::fs::write(&path, canvas.to_premultiplied_rgba()).unwrap();
        eprintln!("{}x{}", canvas.w, canvas.h);
    }
}
