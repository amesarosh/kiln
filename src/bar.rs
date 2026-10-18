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
    layout::LayoutKind,
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
    width: i32,
    scale: i32,
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

    /// Positions of every part, in logical pixels from the bar's left edge: (x, width, part).
    fn layout(&self, text: &Text, m: &Model) -> Vec<(i32, i32, Part)> {
        let h = m.config.height;
        let mut parts = Vec::new();

        let mut x = 0;
        for i in 0..m.workspaces.len() {
            parts.push((x, h, Part::Workspace(i)));
            x += h;
        }

        let plus_x = m.width - h;
        parts.push((plus_x, h, Part::Launcher));
        let layout_w = text.width("monocle", m.config.font_size).ceil() as i32 + 24;
        let layout_x = plus_x - layout_w;
        parts.push((layout_x, layout_w, Part::Layout));

        let (x0, x1) = (x + 8, layout_x - 8);
        let n = m.tabs.len() as i32;
        if n > 0 && x1 > x0 {
            let w = ((x1 - x0) / n).min(MAX_TAB_WIDTH);
            for i in 0..n {
                parts.push((x0 + i * w, (w - 4).max(1), Part::Tab(i as usize)));
            }
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
        let s = m.scale;
        let p = &c.palette;
        let px = c.font_size * s as f32;
        let h = c.height * s;
        let mut canvas = Canvas::new(m.width * s, h, p.surface);

        // A hairline between the bar and the windows.
        let edge_y = if c.position == BarPosition::Top { h - s } else { 0 };
        canvas.fill_rect(0, edge_y, canvas.w, s, p.separator);

        let centered = |canvas: &mut Canvas, label: &str, x: i32, w: i32, size: f32, color: Color| {
            let tw = text.width(label, size);
            let tx = x as f32 + (w as f32 - tw) / 2.0;
            let baseline = text.centered_baseline(0.0, h as f32, size);
            text.draw(canvas, label, size, (tx, baseline), color, (x, x + w));
        };

        for (x, w, part) in self.layout(text, m) {
            let (x, w) = (x * s, w * s);
            let hovered = m.hover == Some(part);
            let inset = 4 * s;
            match part {
                Part::Workspace(i) => {
                    let look = m.workspaces[i];
                    let label_color = match look {
                        WsLook::Active => {
                            canvas.fill_rect(x + 2 * s, inset, w - 4 * s, h - 2 * inset, p.accent);
                            p.on_accent
                        }
                        WsLook::Occupied | WsLook::Empty => {
                            if hovered {
                                canvas.fill_rect(x + 2 * s, inset, w - 4 * s, h - 2 * inset, p.hover);
                            }
                            if look == WsLook::Occupied { p.text } else { p.subtext }
                        }
                    };
                    centered(&mut canvas, &(i + 1).to_string(), x, w, px, label_color);
                    // A small mark under workspaces that have windows.
                    if look != WsLook::Empty {
                        let mark = if look == WsLook::Active { p.on_accent } else { p.text };
                        canvas.fill_rect(x + w / 2 - 4 * s, h - inset - 3 * s, 8 * s, s.max(2), mark);
                    }
                }
                Part::Tab(i) => {
                    let (title, focused) = &m.tabs[i];
                    let fill = match (focused, hovered) {
                        (true, _) => p.selected,
                        (false, true) => p.hover,
                        (false, false) => p.raised,
                    };
                    canvas.fill_rect(x, inset, w, h - 2 * inset, fill);
                    if *focused {
                        let underline = if m.active_monitor { p.accent } else { p.accent_inactive };
                        canvas.fill_rect(x, h - inset - 2 * s, w, 2 * s, underline);
                    }
                    let pad = 8 * s;
                    let label = text.fit(title, px, (w - 2 * pad) as f32);
                    let color = if *focused { p.text } else { p.muted };
                    let baseline = text.centered_baseline(0.0, h as f32, px);
                    text.draw(
                        &mut canvas,
                        &label,
                        px,
                        ((x + pad) as f32, baseline),
                        color,
                        (x, x + w - pad / 2),
                    );
                }
                Part::Layout => {
                    if hovered {
                        canvas.fill_rect(x, inset, w, h - 2 * inset, p.hover);
                    }
                    let name = match m.layout {
                        LayoutKind::Tile => "tile",
                        LayoutKind::Monocle => "monocle",
                    };
                    centered(&mut canvas, name, x, w, px, if hovered { p.text } else { p.muted });
                }
                Part::Launcher => {
                    if hovered {
                        canvas.fill_rect(x, inset, w, h - 2 * inset, p.hover);
                    }
                    centered(&mut canvas, "+", x, w, px * 1.4, p.text);
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

    /// Where a monitor's bar sits, whether or not it's currently hidden by a fullscreen window.
    pub fn bar_reserved(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        let cfg = &self.config.bar;
        if !cfg.enabled || self.bar.text.is_none() {
            return None;
        }
        let zone = self.layer_zone(mon)?;
        let h = cfg.height.min(zone.size.h);
        let y = match cfg.position {
            BarPosition::Top => zone.loc.y,
            BarPosition::Bottom => zone.loc.y + zone.size.h - h,
        };
        Some(Rectangle::new((zone.loc.x, y).into(), (zone.size.w, h).into()))
    }

    /// A monitor's bar rectangle when it's visible.
    pub fn bar_rect(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        self.bar_reserved(mon)
            .filter(|_| self.monitors[mon].ws().fullscreen.is_none())
    }

    /// Area for windows on a monitor: what other panels leave, minus the bar.
    pub fn work_area(&self, mon: usize) -> Option<Rectangle<i32, Logical>> {
        let mut zone = self.layer_zone(mon)?;
        if let Some(bar) = self.bar_reserved(mon) {
            zone.size.h -= bar.size.h;
            if self.config.bar.position == BarPosition::Top {
                zone.loc.y += bar.size.h;
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

    fn bar_model(&self, mon: usize, width: i32, scale: i32) -> Model {
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
            width,
            scale,
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
        let model = self.bar_model(mon, rect.size.w, scale.max(1));
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
        let x = (pos.x - rect.loc.x as f64) as i32;
        let model = self.bar_model(mon, rect.size.w, 1);
        self.bar
            .layout(text, &model)
            .into_iter()
            .find(|(px, w, _)| x >= *px && x < px + w)
            .map(|(_, _, part)| (mon, part))
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

    fn model(width: i32, tabs: &[(&str, bool)], hover: Option<Part>) -> Model {
        Model {
            width,
            scale: 1,
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

    #[test]
    fn layout_parts_do_not_overlap() {
        let Some(text) = Text::load("sans") else { return };
        let bar = Bar::new(&Config::default().bar);
        let m = model(1200, &[("one", true), ("two", false), ("three", false)], None);
        let mut parts = bar.layout(&text, &m);
        parts.sort_by_key(|p| p.0);
        for pair in parts.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
        assert_eq!(parts.last().unwrap().2, Part::Launcher);
        assert!(parts.iter().all(|(x, w, _)| *x >= 0 && x + w <= 1200));
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
        let m = model(
            1400,
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
