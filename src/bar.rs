//! The built-in bar: workspace buttons, one tab per window, a row of buttons at the end
//! (layout, settings, power), and optionally a clock.
//!
//! The bar is painted on the CPU into a single image, and only repainted when what it shows
//! changes (titles, focus, workspace, hover). Layout is computed the same way for painting and
//! for hit testing, so clicks always land on what's drawn.

use std::{cell::RefCell, collections::HashMap};

use wt_config::theme::Palette;

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
    icons::Icons,
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
/// Room for a workspace's number on its button, before its window icons.
const WS_LABEL_W: i32 = 22;
/// On a side bar the number gets a wider slot, so rows line up.
const WS_LABEL_W_SIDE: i32 = 28;
/// Space between window icons on a workspace button.
const WS_ICON_GAP: i32 = 3;
/// Most window icons a workspace button shows on a top or bottom bar; past that, the last
/// slot becomes a "+N" count.
const WS_MAX_ICONS: usize = 4;
/// Width of the "+N" count.
const WS_MORE_W: i32 = 18;
/// How far the pointer moves, in logical pixels, before a press on a workspace or tab becomes
/// a drag.
const DRAG_THRESHOLD: f64 = 4.0;

/// A clickable piece of the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Workspace(usize),
    /// Index into the active workspace's window list.
    Tab(usize),
    Layout,
    /// Opens the settings app.
    Settings,
    /// Opens the power menu.
    Power,
    /// The time. Not a button: clicks do nothing.
    Clock,
}

/// A workspace button or window tab being dragged to a new spot.
#[derive(Debug, Clone, Copy)]
pub struct BarDrag {
    mon: usize,
    /// What's being dragged, at its current spot.
    part: Part,
    start: Point<f64, Logical>,
    /// Past the threshold, so it's a drag, not just a click.
    pub moved: bool,
}

/// The buttons at the end of the bar, in order: left to right on a top or bottom bar, along the
/// bottom row of a side bar. New buttons go here. `Layout` must stay first: it's the wide one,
/// and on a side bar it takes whatever room the others leave in their row.
const END_BUTTONS: [Part; 3] = [Part::Layout, Part::Settings, Part::Power];

/// What the settings button runs, and the app id its window has.
const SETTINGS_APP: &str = "wayland-time-settings";

/// One workspace button.
#[derive(Debug, Clone, PartialEq)]
struct Ws {
    active: bool,
    /// One entry per window, in order: its app id (or title if it has none), for the icon.
    apps: Vec<String>,
}

/// Size of the window icons on workspace buttons, for a bar (or side bar row) `h` high.
fn ws_icon_size(h: i32) -> i32 {
    (h - 12).clamp(10, 18)
}

/// How many window icons fit in `slots`, and how many windows are left over for a "+N".
fn ws_icons_shown(windows: usize, slots: usize) -> (usize, usize) {
    if windows <= slots {
        (windows, 0)
    } else {
        let shown = slots.saturating_sub(1);
        (shown, windows - shown)
    }
}

/// Width of a workspace button on a top or bottom bar: square when empty, otherwise wide
/// enough for its number and window icons.
fn ws_width(h: i32, ws: &Ws) -> i32 {
    if ws.apps.is_empty() {
        return h;
    }
    let icon = ws_icon_size(h);
    let (shown, more) = ws_icons_shown(ws.apps.len(), WS_MAX_ICONS);
    let mut w = WS_LABEL_W + shown as i32 * (icon + WS_ICON_GAP);
    if more > 0 {
        w += WS_MORE_W;
    }
    // Padding at the right end, matching the pill's inset plus a little.
    (w + 7).max(h)
}

/// Everything the bar's pixels depend on. If this is unchanged, the old image is reused.
#[derive(Debug, Clone, PartialEq)]
struct Model {
    /// Logical size of the bar.
    size: (i32, i32),
    scale: i32,
    position: BarPosition,
    config: BarConfig,
    workspaces: Vec<Ws>,
    tabs: Vec<Tab>,
    layout: LayoutKind,
    hover: Option<Part>,
    /// This monitor has keyboard focus; otherwise the focused tab is drawn dimmer.
    active_monitor: bool,
    /// The time to show, if this bar has the clock. Part of the model, so the bar repaints
    /// when the minute changes.
    clock: Option<String>,
}

/// Width of the clock on a top or bottom bar: room for "12:59 PM" plus padding.
fn clock_width(font_size: f32) -> i32 {
    (font_size * 4.8) as i32 + 16
}

/// The local time as a 12-hour clock, like "3:07 PM".
fn clock_text() -> String {
    let (hour, min) = local_hour_min();
    let h12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{h12}:{min:02} {}", if hour < 12 { "AM" } else { "PM" })
}

/// Hour (0-23) and minute in the local time zone.
fn local_hour_min() -> (i32, i32) {
    // SAFETY: `time(NULL)` only returns the time, and `localtime_r` writes into our own `tm`.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return (0, 0);
        }
        (tm.tm_hour, tm.tm_min)
    }
}

/// Time left until the next minute starts, plus a little so the clock never lands just short.
fn until_next_minute() -> std::time::Duration {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let into_minute = since_epoch.as_millis() % 60_000;
    std::time::Duration::from_millis((60_000 - into_minute) as u64 + 50)
}

/// One window in the bar's list.
#[derive(Debug, Clone, PartialEq)]
struct Tab {
    title: String,
    /// For finding the icon.
    app_id: String,
    focused: bool,
}

pub struct Bar {
    text: Option<Text>,
    font: String,
    icons: RefCell<Icons>,
    /// Last painted image per output name.
    cached: RefCell<HashMap<String, (Model, MemoryRenderBuffer)>>,
    /// The part under the pointer, and the output it's on.
    pub hover: Option<(String, Part)>,
    /// Buttons pressed on the bar, so their release isn't sent to a client either.
    pub held_buttons: Vec<u32>,
    /// A workspace or tab being dragged, from left-button press until release.
    pub drag: Option<BarDrag>,
    scroll: f64,
}

impl Bar {
    pub fn new(config: &BarConfig) -> Self {
        Self {
            text: config.enabled.then(|| Text::load(&config.font)).flatten(),
            font: config.font.clone(),
            icons: RefCell::new(Icons::new(&config.icon_theme)),
            cached: RefCell::new(HashMap::new()),
            hover: None,
            held_buttons: Vec::new(),
            drag: None,
            scroll: 0.0,
        }
    }

    pub fn reconfigure(&mut self, config: &BarConfig) {
        self.icons = RefCell::new(Icons::new(&config.icon_theme));
        if config.enabled && (self.text.is_none() || self.font != config.font) {
            self.text = Text::load(&config.font);
            self.font = config.font.clone();
        }
        self.cached.borrow_mut().clear();
    }

    /// An app's icon at `px` pixels, from the icon theme.
    pub fn icon(&self, app_id: &str, px: u32) -> Option<std::rc::Rc<crate::icons::Icon>> {
        self.icons.borrow_mut().get(app_id, px)
    }

    /// Drop the cached image of a monitor that went away.
    pub fn forget_output(&mut self, name: &str) {
        self.cached.borrow_mut().remove(name);
        // Monitor indices may shift, so a drag in progress could land on the wrong bar.
        self.drag = None;
        if self.hover.as_ref().is_some_and(|(n, _)| n == name) {
            self.hover = None;
        }
    }

    /// Where every part goes, in logical pixels from the bar's top-left corner.
    fn layout(&self, m: &Model) -> Vec<(Rect, Part)> {
        if m.position.vertical() {
            Self::layout_vertical(m)
        } else {
            Self::layout_horizontal(m)
        }
    }

    /// Left to right: workspaces, window tabs, end buttons.
    fn layout_horizontal(m: &Model) -> Vec<(Rect, Part)> {
        let (width, h) = m.size;
        let mut parts = Vec::new();

        let mut x = 0;
        for (i, ws) in m.workspaces.iter().enumerate() {
            let w = ws_width(h, ws);
            parts.push((Rect::new(x, 0, w, h), Part::Workspace(i)));
            x += w;
        }

        // End buttons, placed from the right edge inwards.
        let mut end = width;
        for part in END_BUTTONS.into_iter().rev() {
            let w = if part == Part::Layout { h + 8 } else { h };
            end -= w;
            parts.push((Rect::new(end, 0, w, h), part));
        }
        if m.clock.is_some() {
            let w = clock_width(m.config.font_size);
            end -= w;
            parts.push((Rect::new(end, 0, w, h), Part::Clock));
        }

        // Room for the divider after the workspaces.
        let (x0, x1) = (x + 10, end - 6);
        let n = m.tabs.len() as i32;
        if n > 0 && x1 > x0 {
            let w = ((x1 - x0) / n).min(MAX_TAB_WIDTH);
            for i in 0..n {
                parts.push((Rect::new(x0 + i * w, 0, (w - 4).max(1), h), Part::Tab(i as usize)));
            }
        }
        parts
    }

    /// Top to bottom: workspaces, one row per window, the end buttons in the bottom row
    /// (wrapping onto more rows above it if they don't fit), and the clock under everything.
    fn layout_vertical(m: &Model) -> Vec<(Rect, Part)> {
        let (w, height) = m.size;
        let row = m.config.height;
        let mut parts = Vec::new();

        // Workspaces stacked in one column, one row each.
        let mut y = 4;
        for i in 0..m.workspaces.len() {
            parts.push((Rect::new(0, y, w, row), Part::Workspace(i)));
            y += row;
        }
        y += 4;

        // End buttons, square, placed right to left from the bottom-right corner. The layout
        // button comes last and fills what's left of its row.
        let mut bottom = height - row;
        if m.clock.is_some() {
            parts.push((Rect::new(0, bottom, w, row), Part::Clock));
            bottom -= row;
        }
        let mut x = w;
        for part in END_BUTTONS.into_iter().rev() {
            if x < row && x < w {
                x = w;
                bottom -= row;
            }
            if part == Part::Layout {
                parts.push((Rect::new(0, bottom, x, row), part));
            } else {
                x -= row;
                parts.push((Rect::new(x, bottom, row, row), part));
            }
        }

        y += 9;
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
        let radius = c.radius * s as f32;
        let (bw, bh) = (m.size.0 * s, m.size.1 * s);
        let mut canvas = Canvas::new(bw, bh, p.surface);
        let parts = self.layout(m);

        // A hairline between the bar and the windows.
        match m.position {
            BarPosition::Top => canvas.fill_rect(0, bh - s, bw, s, p.separator),
            BarPosition::Bottom => canvas.fill_rect(0, 0, bw, s, p.separator),
            BarPosition::Left => canvas.fill_rect(bw - s, 0, s, bh, p.separator),
            BarPosition::Right => canvas.fill_rect(0, 0, s, bh, p.separator),
        }

        // A short divider between the workspaces and the window list.
        let last_ws = parts
            .iter()
            .filter(|(_, p)| matches!(p, Part::Workspace(_)))
            .map(|(r, _)| *r)
            .next_back();
        if let Some(r) = last_ws {
            if vertical {
                let y = (r.y + r.h + 4) * s;
                canvas.fill_rect(8 * s, y, bw - 16 * s, s, p.separator);
            } else {
                let x = (r.x + r.w + 4) * s;
                canvas.fill_rect(x, bh / 4, s, bh / 2, p.separator);
            }
        }

        let centered = |canvas: &mut Canvas, label: &str, r: Rect, size: f32, color: Color| {
            let tw = text.width(label, size);
            let tx = r.x as f32 + (r.w as f32 - tw) / 2.0;
            let baseline = text.centered_baseline(r.y as f32, r.h as f32, size);
            text.draw(canvas, label, size, (tx, baseline), color, (r.x, r.x + r.w));
        };
        let rounded = |canvas: &mut Canvas, r: Rect, color: Color| {
            canvas.fill_rounded_rect(r.x, r.y, r.w, r.h, radius, color);
        };
        // Buttons are drawn a little inside their hit area so neighbours don't touch.
        let inset = |r: Rect, by: i32| Rect::new(r.x + by, r.y + by, r.w - 2 * by, r.h - 2 * by);

        for (r, part) in parts {
            let r = Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
            let hovered = m.hover == Some(part);
            match part {
                Part::Workspace(i) => {
                    let ws = &m.workspaces[i];
                    // Side bars: full-width rows lined up with the window rows below.
                    let pill = if vertical {
                        Rect::new(r.x + 6 * s, r.y + s, r.w - 12 * s, r.h - 2 * s)
                    } else {
                        inset(r, 3 * s)
                    };
                    let label_color = if ws.active {
                        rounded(&mut canvas, pill, p.accent);
                        p.on_accent
                    } else {
                        if hovered {
                            rounded(&mut canvas, pill, p.hover);
                        }
                        if ws.apps.is_empty() { p.subtext } else { p.text }
                    };

                    // The number, then an icon per window. Empty workspaces on a top bar are
                    // just the number, centered.
                    let label = (i + 1).to_string();
                    let label_w = if vertical { WS_LABEL_W_SIDE } else { WS_LABEL_W } * s;
                    let label_rect = match (vertical, ws.apps.is_empty()) {
                        (false, true) => r,
                        (false, false) => Rect::new(r.x + s, r.y, label_w, r.h),
                        (true, _) => Rect::new(pill.x, r.y, label_w, r.h),
                    };
                    centered(&mut canvas, &label, label_rect, px * 0.9, label_color);
                    if ws.apps.is_empty() {
                        continue;
                    }

                    let icon = ws_icon_size(r.h / s) * s;
                    let gap = WS_ICON_GAP * s;
                    let x0 = label_rect.x + label_rect.w;
                    let slots = if vertical {
                        // As many as fit in the row, keeping room for the count.
                        ((pill.x + pill.w - 4 * s - x0) / (icon + gap)).max(1) as usize
                    } else {
                        WS_MAX_ICONS
                    };
                    let (shown, more) = ws_icons_shown(ws.apps.len(), slots);
                    let y = r.y + (r.h - icon) / 2;
                    let mut x = x0;
                    for app in &ws.apps[..shown] {
                        self.draw_app_icon(&mut canvas, text, app, (x, y), icon, radius, p);
                        x += icon + gap;
                    }
                    if more > 0 {
                        let count = format!("+{more}");
                        let size = px * 0.75;
                        let color = if ws.active { p.on_accent } else { p.muted };
                        let baseline = text.centered_baseline(r.y as f32, r.h as f32, size);
                        let right = pill.x + pill.w;
                        text.draw(&mut canvas, &count, size, (x as f32, baseline), color, (x, right));
                    }
                }
                Part::Tab(i) => {
                    let tab = &m.tabs[i];
                    let area = if vertical {
                        Rect::new(r.x + 6 * s, r.y, r.w - 12 * s, r.h)
                    } else {
                        Rect::new(r.x, r.y + 3 * s, r.w, r.h - 6 * s)
                    };
                    if tab.focused {
                        rounded(&mut canvas, area, p.selected);
                    } else if hovered {
                        rounded(&mut canvas, area, p.hover);
                    }
                    let mark = if m.active_monitor { p.accent } else { p.accent_inactive };
                    if tab.focused {
                        // A small accent bar: at the bottom middle of a tab, at the left of a row.
                        if vertical {
                            let h = area.h / 2;
                            canvas.fill_rounded_rect(area.x, area.y + (area.h - h) / 2, 3 * s, h, 1.5 * s as f32, mark);
                        } else {
                            let w = (area.w / 4).clamp(12 * s, 32 * s);
                            canvas.fill_rounded_rect(
                                area.x + (area.w - w) / 2,
                                area.y + area.h - 3 * s,
                                w,
                                2 * s,
                                s as f32,
                                mark,
                            );
                        }
                    }

                    let pad = 8 * s;
                    let icon_px = ((area.h - 8 * s).max(8 * s) as u32).min(24 * s as u32);
                    let mut x = area.x + pad;
                    let y = area.y + (area.h - icon_px as i32) / 2;
                    let name = if tab.app_id.is_empty() { &tab.title } else { &tab.app_id };
                    self.draw_app_icon(&mut canvas, text, name, (x, y), icon_px as i32, radius, p);
                    x += icon_px as i32 + 6 * s;
                    let right = area.x + area.w - pad;
                    if right > x {
                        let label = text.fit(&tab.title, px, (right - x) as f32);
                        let color = if tab.focused { p.text } else { p.muted };
                        let baseline = text.centered_baseline(r.y as f32, r.h as f32, px);
                        text.draw(&mut canvas, &label, px, (x as f32, baseline), color, (x, right));
                    }
                }
                Part::Clock => {
                    let Some(time) = &m.clock else { continue };
                    if vertical {
                        // A short rule above it, like the one under the workspaces.
                        canvas.fill_rect(r.x + 8 * s, r.y, r.w - 16 * s, s, p.separator);
                    }
                    centered(&mut canvas, time, r, px, p.text);
                }
                Part::Layout | Part::Settings | Part::Power => {
                    if hovered {
                        rounded(&mut canvas, inset(r, 3 * s), p.hover);
                    }
                    let color = if hovered { p.text } else { p.muted };
                    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
                    match part {
                        Part::Layout => draw_layout_glyph(&mut canvas, m.layout, r, s, color),
                        Part::Settings => draw_gear(&mut canvas, cx as f32, cy as f32, s as f32, color),
                        _ => draw_power(&mut canvas, cx as f32, cy as f32, s as f32, color),
                    }
                }
            }
        }

        canvas
    }

    /// An app's icon as a `side`-pixel square at `at`; without one, the first letter of `name`
    /// on a rounded square.
    #[allow(clippy::too_many_arguments)]
    fn draw_app_icon(
        &self,
        canvas: &mut Canvas,
        text: &Text,
        name: &str,
        at: (i32, i32),
        side: i32,
        radius: f32,
        p: &Palette,
    ) {
        let (x, y) = at;
        if let Some(icon) = self.icons.borrow_mut().get(name, side as u32) {
            canvas.draw_image(x, y, icon.size, &icon.rgba);
            return;
        }
        canvas.fill_rounded_rect(x, y, side, side, radius.min(side as f32 / 3.0), p.hover);
        let letter: String = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_default();
        let size = side as f32 * 0.7;
        let tw = text.width(&letter, size);
        let tx = x as f32 + (side as f32 - tw) / 2.0;
        let baseline = text.centered_baseline(y as f32, side as f32, size);
        text.draw(canvas, &letter, size, (tx, baseline), p.text, (x, x + side));
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

/// A small picture of the layout: a master pane and a stack for tile (beside it) and
/// vertical (under it), one pane for monocle.
fn draw_layout_glyph(canvas: &mut Canvas, layout: LayoutKind, r: Rect, s: i32, color: Color) {
    let (w, h) = (16 * s, 12 * s);
    let (x, y) = (r.x + (r.w - w) / 2, r.y + (r.h - h) / 2);
    let radius = 1.5 * s as f32;
    match layout {
        LayoutKind::Tile => {
            let gap = 2 * s;
            let left = (w - gap) * 3 / 5;
            let right = w - gap - left;
            let half = (h - gap) / 2;
            canvas.fill_rounded_rect(x, y, left, h, radius, color);
            canvas.fill_rounded_rect(x + left + gap, y, right, half, radius, color);
            canvas.fill_rounded_rect(x + left + gap, y + half + gap, right, h - half - gap, radius, color);
        }
        LayoutKind::Vertical => {
            let gap = 2 * s;
            let top = (h - gap) * 3 / 5;
            let bottom = h - gap - top;
            let half = (w - gap) / 2;
            canvas.fill_rounded_rect(x, y, w, top, radius, color);
            canvas.fill_rounded_rect(x, y + top + gap, half, bottom, radius, color);
            canvas.fill_rounded_rect(x + half + gap, y + top + gap, w - half - gap, bottom, radius, color);
        }
        LayoutKind::Monocle => canvas.fill_rounded_rect(x, y, w, h, radius, color),
    }
}

/// A gear: a ring with eight teeth, centered on (cx, cy), anti-aliased.
fn draw_gear(canvas: &mut Canvas, cx: f32, cy: f32, s: f32, color: Color) {
    const TEETH: f32 = 8.0;
    // Tip of the teeth, rim of the body, and the hole, in logical pixels.
    let (tip, rim, hole) = (7.5 * s, 5.5 * s, 2.25 * s);
    let reach = tip.ceil() as i32 + 1;
    let (x0, y0) = (cx.floor() as i32, cy.floor() as i32);
    for y in y0 - reach..=y0 + reach {
        for x in x0 - reach..=x0 + reach {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let r = (dx * dx + dy * dy).sqrt();
            // Where this pixel is around the gear, 0..1 within one tooth period, tooth centered.
            let turn = (dy.atan2(dx) / std::f32::consts::TAU * TEETH + 0.5).rem_euclid(1.0);
            // Teeth narrow towards the tip; their half-width as a fraction of the period.
            let along = ((r - rim) / (tip - rim)).clamp(0.0, 1.0);
            let half = 0.27 - 0.09 * along;
            // Distance inside the tooth's sides (measured along the circle) and its tip.
            let side = (half - (turn - 0.5).abs()) * std::f32::consts::TAU * r / TEETH;
            let tooth = (side.min(tip - r) + 0.5).clamp(0.0, 1.0);
            let body = (rim - r + 0.5).clamp(0.0, 1.0);
            let coverage = body.max(tooth) * (r - hole + 0.5).clamp(0.0, 1.0);
            if coverage > 0.0 {
                canvas.blend(x, y, color, coverage);
            }
        }
    }
}

/// The power symbol: a ring open at the top with a bar through the gap, centered on
/// (cx, cy), anti-aliased.
fn draw_power(canvas: &mut Canvas, cx: f32, cy: f32, s: f32, color: Color) {
    // Ring radius and stroke half-width, in logical pixels; the gap's half-angle.
    let (ring, half) = (5.5 * s, 1.0 * s);
    let gap = 0.75f32;
    let reach = (ring + half).ceil() as i32 + 2;
    let (x0, y0) = (cx.floor() as i32, cy.floor() as i32);
    for y in y0 - reach..=y0 + reach {
        for x in x0 - reach..=x0 + reach {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let r = (dx * dx + dy * dy).sqrt();
            // Angle away from straight up; the ring stops `gap` either side of it, with round ends.
            let from_top = dx.atan2(-dy).abs();
            let ring_d = if from_top >= gap {
                (r - ring).abs()
            } else {
                let end = (cx + ring * gap.sin() * dx.signum(), cy - ring * gap.cos());
                ((x as f32 + 0.5 - end.0).powi(2) + (y as f32 + 0.5 - end.1).powi(2)).sqrt()
            };
            // The bar: from the centre up past the ring's top, with round ends.
            let (top, bottom) = (cy - ring - 0.5 * s, cy - 0.5 * s);
            let ny = (y as f32 + 0.5).clamp(top, bottom);
            let bar_d = (dx.powi(2) + (y as f32 + 0.5 - ny).powi(2)).sqrt();
            let coverage = (half - ring_d.min(bar_d) + 0.5).clamp(0.0, 1.0);
            if coverage > 0.0 {
                canvas.blend(x, y, color, coverage);
            }
        }
    }
}

impl State {
    /// Wake at the start of every minute so a bar with the clock repaints. Bars without one
    /// compare equal to their cached image, so this costs next to nothing.
    pub fn start_clock_timer(&mut self) {
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        let result = self
            .loop_handle
            .insert_source(Timer::from_duration(until_next_minute()), |_, _, state| {
                if state.config.bar.enabled && !state.config.bar.clock_outputs.is_empty() {
                    state.needs_redraw = true;
                }
                TimeoutAction::ToDuration(until_next_minute())
            });
        if let Err(e) = result {
            tracing::warn!("clock timer: {e}");
        }
    }

    /// Opens the settings app, or brings it forward if it's already open.
    fn open_settings(&mut self) {
        let open = self
            .monitors
            .iter()
            .flat_map(|m| &m.workspaces)
            .flat_map(|ws| &ws.entries)
            .find(|e| toplevel_ids(&e.window).0.as_deref() == Some(SETTINGS_APP))
            .map(|e| e.window.clone());
        match open {
            Some(w) => self.activate_window(&w),
            None => self.spawn(SETTINGS_APP),
        }
    }

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
            .map(|(i, w)| Ws {
                active: i == m.active_ws,
                apps: w
                    .entries
                    .iter()
                    .map(|e| {
                        let (app_id, title) = toplevel_ids(&e.window);
                        app_id.filter(|a| !a.is_empty()).or(title).unwrap_or_default()
                    })
                    .collect(),
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
                Tab {
                    title: label,
                    app_id: toplevel_ids(&e.window).0.unwrap_or_default(),
                    focused: ws.focus.as_ref() == Some(&e.window),
                }
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
            clock: self.config.bar.clock_on(&name).then(clock_text),
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
        let (x, y) = ((pos.x - rect.loc.x as f64) as i32, (pos.y - rect.loc.y as f64) as i32);
        let model = self.bar_model(mon, (rect.size.w, rect.size.h), 1);
        self.bar
            .layout(&model)
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
        if button == BTN_LEFT && matches!(part, Part::Workspace(_) | Part::Tab(_)) {
            let start = self.seat.get_pointer().unwrap().current_location();
            self.bar.drag = Some(BarDrag {
                mon,
                part,
                start,
                moved: false,
            });
        }
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
            (Part::Tab(i), BTN_RIGHT) => {
                if let Some(w) = tab_window(self, i) {
                    let pos = self.seat.get_pointer().unwrap().current_location();
                    self.open_window_menu(w, pos);
                }
            }
            (Part::Tab(i), BTN_MIDDLE) => {
                if let Some(w) = tab_window(self, i) {
                    crate::state::close_window(&w);
                }
            }
            (Part::Layout, BTN_LEFT | BTN_RIGHT) => self.run_action(Action::Layout(LayoutChoice::Next)),
            (Part::Settings, BTN_LEFT) => self.open_settings(),
            (Part::Power, BTN_LEFT | BTN_RIGHT) => {
                let pos = self.seat.get_pointer().unwrap().current_location();
                self.open_power_menu(mon, pos);
            }
            _ => {}
        }
    }

    /// Pointer motion while a workspace or tab is held: once it's moved far enough, move the
    /// held item to the spot under the pointer.
    pub fn bar_drag_motion(&mut self, pos: Point<f64, Logical>) {
        let Some(mut drag) = self.bar.drag else { return };
        if !drag.moved {
            let (dx, dy) = (pos.x - drag.start.x, pos.y - drag.start.y);
            if dx.hypot(dy) < DRAG_THRESHOLD {
                return;
            }
            drag.moved = true;
            self.bar.drag = Some(drag);
        }
        let mon = drag.mon;
        let (Some(rect), Some(position)) = (self.bar_rect(mon), self.bar_position(mon)) else {
            return;
        };
        let vertical = position.vertical();
        // Only the position along the bar counts, so the pointer can stray off the bar's edge.
        let (x, y) = if vertical {
            (rect.size.w / 2, (pos.y - rect.loc.y as f64) as i32)
        } else {
            ((pos.x - rect.loc.x as f64) as i32, rect.size.h / 2)
        };
        let model = self.bar_model(mon, (rect.size.w, rect.size.h), 1);
        let parts = self.bar.layout(&model);
        let rect_of = |part: Part| parts.iter().find(|(_, p)| *p == part).map(|(r, _)| *r);
        let Some(from_rect) = rect_of(drag.part) else { return };
        let Some((to_rect, to)) = parts
            .iter()
            .find(|(r, p)| {
                std::mem::discriminant(p) == std::mem::discriminant(&drag.part)
                    && x >= r.x
                    && x < r.x + r.w
                    && y >= r.y
                    && y < r.y + r.h
            })
            .copied()
        else {
            return;
        };
        let (from, to_idx) = match (drag.part, to) {
            (Part::Workspace(a), Part::Workspace(b)) | (Part::Tab(a), Part::Tab(b)) => (a, b),
            _ => return,
        };
        if from == to_idx {
            return;
        }
        // Move only once the pointer would be over the held item at its new spot. Workspace
        // buttons differ in width, and without this a narrow one dragged over a wide one would
        // swap back and forth.
        let (p, size, start, end) = if vertical {
            (y, from_rect.h, to_rect.y, to_rect.y + to_rect.h)
        } else {
            (x, from_rect.w, to_rect.x, to_rect.x + to_rect.w)
        };
        let lands = if to_idx > from { p >= end - size } else { p < start + size };
        if !lands {
            return;
        }
        let m = &mut self.monitors[mon];
        match drag.part {
            Part::Workspace(_) => {
                if to_idx >= m.workspaces.len() {
                    return;
                }
                let ws = m.workspaces.remove(from);
                m.workspaces.insert(to_idx, ws);
                m.active_ws = moved_index(m.active_ws, from, to_idx);
                drag.part = Part::Workspace(to_idx);
            }
            _ => {
                let entries = &mut m.ws_mut().entries;
                if to_idx >= entries.len() {
                    return;
                }
                let e = entries.remove(from);
                entries.insert(to_idx, e);
                drag.part = Part::Tab(to_idx);
            }
        }
        self.bar.drag = Some(drag);
        // Tabs are the tiling order, so the tiles follow.
        self.arrange();
        self.needs_redraw = true;
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

/// Where the item at `idx` ends up after the one at `from` is moved to `to`.
fn moved_index(idx: usize, from: usize, to: usize) -> usize {
    if idx == from {
        to
    } else if from < idx && idx <= to {
        idx - 1
    } else if to <= idx && idx < from {
        idx + 1
    } else {
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_index_follows_a_move() {
        // [a b c d], b moved to the end: [a c d b].
        assert_eq!(moved_index(1, 1, 3), 3);
        assert_eq!(moved_index(2, 1, 3), 1);
        assert_eq!(moved_index(0, 1, 3), 0);
        // d moved to the front: [d a b c].
        assert_eq!(moved_index(0, 3, 0), 1);
        assert_eq!(moved_index(3, 3, 0), 0);
    }
    use crate::config::Config;

    fn model(size: (i32, i32), position: BarPosition, tabs: &[(&str, bool)], hover: Option<Part>) -> Model {
        Model {
            size,
            scale: 1,
            position,
            config: Config::default().bar,
            workspaces: [
                (false, &["Alacritty", "vivaldi-stable"][..]),
                (true, &["Alacritty", "vivaldi-stable", "code", "XClock"][..]),
                (false, &[][..]),
                (false, &["steam", "Alacritty", "code", "a", "b", "c"][..]),
                (false, &[][..]),
            ]
            .iter()
            .map(|(active, apps)| Ws {
                active: *active,
                apps: apps.iter().map(|a| a.to_string()).collect(),
            })
            .collect(),
            tabs: tabs
                .iter()
                .map(|(t, f)| Tab {
                    title: t.to_string(),
                    app_id: "Alacritty".into(),
                    focused: *f,
                })
                .collect(),
            layout: LayoutKind::Tile,
            hover,
            active_monitor: true,
            clock: None,
        }
    }

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn layout_parts_do_not_overlap() {
        let bar = Bar::new(&Config::default().bar);
        let tabs = [("one", true), ("two", false), ("three", false)];
        for ((size, position), clock) in [
            ((1200, 28), BarPosition::Top),
            ((180, 1400), BarPosition::Left),
            ((180, 1400), BarPosition::Right),
        ]
        .into_iter()
        .flat_map(|c| [(c, false), (c, true)])
        {
            let mut m = model(size, position, &tabs, None);
            m.clock = clock.then(|| "12:59 PM".into());
            let parts = bar.layout(&m);
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
            for button in END_BUTTONS {
                assert_eq!(parts.iter().filter(|(_, p)| *p == button).count(), 1, "{button:?}");
            }
            let clocks: Vec<_> = parts.iter().filter(|(_, p)| *p == Part::Clock).collect();
            assert_eq!(clocks.len(), clock as usize);
            if position.vertical()
                && let Some((r, _)) = clocks.first()
            {
                // The very bottom of the bar.
                assert_eq!(r.y + r.h, size.1);
            }
        }
    }

    #[test]
    fn vertical_bar_drops_tabs_that_do_not_fit() {
        let bar = Bar::new(&Config::default().bar);
        let tabs: Vec<(&str, bool)> = (0..50).map(|_| ("w", false)).collect();
        let parts = bar.layout(&model((180, 600), BarPosition::Left, &tabs, None));
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

    /// `BAR_PREVIEW=/path/out.rgba cargo test bar_preview` writes the bar as raw RGBA for a quick look
    /// (`BAR_PREVIEW_FONT` picks the font, `BAR_PREVIEW_VERTICAL` a side bar).
    #[test]
    fn bar_preview() {
        let Some(path) = std::env::var_os("BAR_PREVIEW") else {
            return;
        };
        let font = std::env::var("BAR_PREVIEW_FONT").unwrap_or_else(|_| "sans".into());
        let text = Text::load(&font).expect("font");
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
                ("Start Page - Vivaldi", false),
                (
                    "bar.rs - wayland-time - A very long window title that will need to be cut short",
                    false,
                ),
                ("Steam", false),
            ],
            Some(Part::Tab(1)),
        );
        let mut m = m;
        m.clock = Some(clock_text());
        for (tab, app) in m.tabs.iter_mut().zip(["Alacritty", "vivaldi-stable", "code", "XClock"]) {
            tab.app_id = app.into();
        }
        let canvas = bar.paint_canvas(&text, &m);
        std::fs::write(&path, canvas.to_premultiplied_rgba()).unwrap();
        eprintln!("{}x{}", canvas.w, canvas.h);
    }
}
