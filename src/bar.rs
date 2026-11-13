//! The built-in bar: workspace buttons, one tab per window, status widgets (tray icons, media,
//! network, battery, brightness), a row of buttons at the end (layout, the notifications bell,
//! settings, power), and optionally a clock.
//!
//! The bar is painted on the CPU into a single image, and only repainted when what it shows
//! changes (titles, focus, workspace, hover). Layout is computed the same way for painting and
//! for hit testing, so clicks always land on what's drawn.

use std::{cell::RefCell, collections::HashMap};

use kiln_config::theme::Palette;

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
    /// A tray icon, by index into the bus's tray items.
    Tray(usize),
    /// What's playing.
    Media,
    /// Click opens the Sound app, right-click mutes, scroll changes the volume.
    Sound,
    Network,
    /// Click opens the Bluetooth app, right-click turns it on or off.
    Bluetooth,
    Battery,
    Brightness,
    /// Unread notifications and do not disturb.
    Bell,
}

/// A status widget as drawn: which part it is, how wide, and what it shows.
#[derive(Debug, Clone, PartialEq)]
struct StatusItem {
    part: Part,
    /// Logical width on a top or bottom bar.
    w: i32,
    look: Look,
}

impl StatusItem {
    /// Shows text as well as an icon, so it gets a full-width row on a side bar.
    fn has_label(&self) -> bool {
        matches!(self.look, Look::Media { .. } | Look::Icon { label: Some(_), .. })
    }
}

/// How far a side bar's status rows sit in from its edges, like the window rows.
const SIDE_INSET: i32 = 4;

#[derive(Debug, Clone, PartialEq)]
enum Look {
    Tray {
        icon_name: String,
        theme_path: String,
        pixmap: Option<(i32, i32, Vec<u8>)>,
    },
    /// A symbolic icon, with a short label after it (like "80%").
    Icon {
        name: String,
        label: Option<String>,
    },
    Media {
        text: String,
        playing: bool,
    },
    Bell {
        unread: bool,
        dnd: bool,
    },
}

/// Widest the media widget gets, in logical pixels.
const MEDIA_MAX_W: i32 = 240;

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

/// The buttons at the end of the bar, in order: left to right on a top or bottom bar, spread
/// over the bottom row of a side bar. New buttons go here.
const END_BUTTONS: [Part; 3] = [Part::Layout, Part::Settings, Part::Power];

/// The end buttons for this bar: the notifications bell, when shown, goes right after the
/// layout button.
fn end_buttons(m: &Model) -> Vec<Part> {
    let mut parts = END_BUTTONS.to_vec();
    if m.status.iter().any(|i| i.part == Part::Bell) {
        parts.insert(1, Part::Bell);
    }
    parts
}

/// What the settings button runs, and the app id its window has.
const SETTINGS_APP: &str = "kiln-settings";
const BLUETOOTH_APP: &str = "kiln-bluetooth";
const SOUND_APP: &str = "kiln-sound";

/// One workspace button.
#[derive(Debug, Clone, PartialEq, Default)]
struct Ws {
    active: bool,
    /// One entry per window, in order: its app id (or title if it has none), for the icon.
    apps: Vec<String>,
    /// From the config: shown instead of the number when set.
    name: String,
    icon: String,
    /// Width of `name` in logical pixels, measured when the model is built.
    name_w: i32,
}

impl Ws {
    fn labelled(&self) -> bool {
        !self.name.is_empty() || !self.icon.is_empty()
    }
}

/// Room for a workspace's label (its number, or its name and icon) on a bar `h` high.
fn ws_label_w(h: i32, ws: &Ws, vertical: bool) -> i32 {
    let number = if vertical { WS_LABEL_W_SIDE } else { WS_LABEL_W };
    if !ws.labelled() {
        return number;
    }
    let mut w = 8;
    if !ws.icon.is_empty() {
        w += ws_icon_size(h);
        if !ws.name.is_empty() {
            w += 5;
        }
    }
    (w + ws.name_w + if ws.name.is_empty() { 2 } else { 8 }).max(number)
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
        return ws_label_w(h, ws, false).max(h);
    }
    let icon = ws_icon_size(h);
    let (shown, more) = ws_icons_shown(ws.apps.len(), WS_MAX_ICONS);
    let mut w = ws_label_w(h, ws, false) + shown as i32 * (icon + WS_ICON_GAP);
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
    /// Status widgets, in order.
    status: Vec<StatusItem>,
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
    minimized: bool,
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

    /// Any icon by name (status and device icons too), at `px` pixels.
    pub fn any_icon(&self, name: &str, px: u32, tint: [u8; 4]) -> Option<std::rc::Rc<crate::icons::Icon>> {
        self.icons.borrow_mut().any(name, px, tint)
    }

    /// A symbolic icon at `px` pixels, in one color (used by the bar's own painting too).
    pub fn symbolic(&self, name: &str, px: u32, color: [u8; 4]) -> Option<std::rc::Rc<crate::icons::Icon>> {
        self.icons.borrow_mut().symbolic(name, px, color)
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
        for part in end_buttons(m).into_iter().rev() {
            let w = if part == Part::Layout { h + 8 } else { h };
            end -= w;
            parts.push((Rect::new(end, 0, w, h), part));
        }
        if m.clock.is_some() {
            let w = clock_width(m.config.font_size);
            end -= w;
            parts.push((Rect::new(end, 0, w, h), Part::Clock));
        }
        // Status widgets before the clock, as long as they leave room for the workspaces.
        for item in m.status.iter().rev().filter(|i| i.part != Part::Bell) {
            if end - item.w < x + 40 {
                break;
            }
            end -= item.w;
            parts.push((Rect::new(end, 0, item.w, h), item.part));
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

    /// Top to bottom: workspaces, one row per window, then the status block at the bottom: a
    /// row of small icons (tray, network, Bluetooth; wrapping if there are many), one full-width
    /// row per widget with a label (media, sound, battery, brightness), the buttons spread over
    /// one row, and the clock under everything.
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

        let mut bottom = height - row;
        if m.clock.is_some() {
            parts.push((Rect::new(0, bottom, w, row), Part::Clock));
            bottom -= row;
        }

        // The status block's rows, top to bottom, as (x, width, part). Inset like the window
        // rows, so their hover highlights line up.
        let (x0, inner) = (SIDE_INSET, w - 2 * SIDE_INSET);
        let mut rows: Vec<Vec<(i32, i32, Part)>> = Vec::new();
        let icons: Vec<Part> = m
            .status
            .iter()
            .filter(|i| i.part != Part::Bell && !i.has_label())
            .map(|i| i.part)
            .collect();
        let per_row = (inner / row).max(1) as usize;
        for chunk in icons.chunks(per_row) {
            rows.push(chunk.iter().zip(0..).map(|(&p, i)| (x0 + i * row, row, p)).collect());
        }
        for item in m.status.iter().filter(|i| i.has_label()) {
            rows.push(vec![(x0, inner, item.part)]);
        }
        let buttons = end_buttons(m);
        let n = buttons.len() as i32;
        rows.push(
            buttons
                .iter()
                .zip(0..)
                .map(|(&p, i)| {
                    let (a, b) = (x0 + inner * i / n, x0 + inner * (i + 1) / n);
                    (a, b - a, p)
                })
                .collect(),
        );
        // From the bottom up: the buttons always, the rest while there's room under the
        // workspaces.
        for (k, r) in rows.iter().rev().enumerate() {
            if k > 0 && bottom < y + row {
                break;
            }
            for &(x, iw, part) in r {
                parts.push((Rect::new(x, bottom, iw, row), part));
            }
            bottom -= row;
        }
        // `bottom` is now the row above the top one used.
        bottom += row;

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

        // And one above a side bar's status block.
        if vertical {
            let top = parts
                .iter()
                .filter(|(_, p)| !matches!(p, Part::Workspace(_) | Part::Tab(_) | Part::Clock))
                .map(|(r, _)| r.y)
                .min();
            if let Some(y) = top {
                canvas.fill_rect(8 * s, (y - 4) * s, bw - 16 * s, s, p.separator);
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

                    // The number (or name and icon), then an icon per window. Empty workspaces
                    // on a top bar are just the label, centered.
                    let label_w = ws_label_w(r.h / s, ws, vertical) * s;
                    let label_rect = match (vertical, ws.apps.is_empty()) {
                        (false, true) => r,
                        (false, false) => Rect::new(r.x + s, r.y, label_w, r.h),
                        (true, _) => Rect::new(pill.x, r.y, label_w, r.h),
                    };
                    if ws.labelled() {
                        // Centered in an empty button, otherwise from its left edge.
                        let x = if ws.apps.is_empty() && !vertical {
                            label_rect.x + (label_rect.w - label_w) / 2 + 8 * s
                        } else {
                            label_rect.x + 8 * s
                        };
                        let mut x = x;
                        if !ws.icon.is_empty() {
                            let icon = ws_icon_size(r.h / s) * s;
                            let y = r.y + (r.h - icon) / 2;
                            self.draw_app_icon(&mut canvas, text, &ws.icon, (x, y), icon, radius, p);
                            x += icon + 5 * s;
                        }
                        if !ws.name.is_empty() {
                            let size = px * 0.9;
                            let baseline = text.centered_baseline(r.y as f32, r.h as f32, size);
                            let right = label_rect.x + label_rect.w;
                            text.draw(
                                &mut canvas,
                                &ws.name,
                                size,
                                (x as f32, baseline),
                                label_color,
                                (x, right),
                            );
                        }
                    } else {
                        centered(&mut canvas, &(i + 1).to_string(), label_rect, px * 0.9, label_color);
                    }
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
                        let color = match (tab.focused, tab.minimized) {
                            (true, _) => p.text,
                            (false, true) => p.subtext,
                            (false, false) => p.muted,
                        };
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
                Part::Tray(_)
                | Part::Media
                | Part::Sound
                | Part::Network
                | Part::Bluetooth
                | Part::Battery
                | Part::Brightness
                | Part::Bell => {
                    if let Some(item) = m.status.iter().find(|i| i.part == part) {
                        self.paint_status(&mut canvas, text, item, r, hovered, s, m);
                    }
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

    /// One status widget in its rectangle (already scaled).
    #[allow(clippy::too_many_arguments)]
    fn paint_status(
        &self,
        canvas: &mut Canvas,
        text: &Text,
        item: &StatusItem,
        r: Rect,
        hovered: bool,
        s: i32,
        m: &Model,
    ) {
        let p = &m.config.palette;
        let radius = m.config.radius * s as f32;
        let pill = Rect::new(r.x + 2 * s, r.y + 3 * s, r.w - 4 * s, r.h - 6 * s);
        if hovered {
            canvas.fill_rounded_rect(pill.x, pill.y, pill.w, pill.h, radius, p.hover);
        }
        let color = if hovered { p.text } else { p.muted };
        let tint = to_u8(color);
        let icon_px = ws_icon_size(r.h / s) * s;
        let px = m.config.font_size * s as f32 * 0.9;
        let iy = r.y + (r.h - icon_px) / 2;
        // Icon, then a label, left-aligned from a small inset; or centered without a label.
        let label = |canvas: &mut Canvas, t: &str, x: i32| {
            let right = r.x + r.w - 6 * s;
            if right <= x {
                return;
            }
            let fitted = text.fit(t, px, (right - x) as f32);
            let baseline = text.centered_baseline(r.y as f32, r.h as f32, px);
            text.draw(canvas, &fitted, px, (x as f32, baseline), color, (x, right));
        };
        match &item.look {
            Look::Tray {
                icon_name,
                theme_path,
                pixmap,
            } => {
                let size = ((r.h / s - 10).clamp(12, 22)) * s;
                let (x, y) = (r.x + (r.w - size) / 2, r.y + (r.h - size) / 2);
                let from_path = (!theme_path.is_empty() && !icon_name.is_empty())
                    .then(|| {
                        ["png", "svg"].iter().find_map(|ext| {
                            let f = std::path::Path::new(theme_path).join(format!("{icon_name}.{ext}"));
                            f.is_file().then(|| f.to_string_lossy().to_string())
                        })
                    })
                    .flatten();
                let icon = from_path
                    .and_then(|f| self.icons.borrow_mut().get(&f, size as u32))
                    .or_else(|| {
                        (!icon_name.is_empty())
                            .then(|| self.any_icon(icon_name, size as u32, tint))
                            .flatten()
                    })
                    .or_else(|| {
                        let (w, h, argb) = pixmap.as_ref()?;
                        Some(std::rc::Rc::new(kiln_icons::scaled(
                            &argb_to_rgba(argb),
                            *w as u32,
                            *h as u32,
                            size as u32,
                        )))
                    });
                match icon {
                    Some(icon) => canvas.draw_image(x, y, icon.size, &icon.rgba),
                    None => canvas.fill_rounded_rect(x, y, size, size, radius.min(size as f32 / 3.0), p.hover),
                }
            }
            Look::Icon {
                name,
                label: text_label,
            } => {
                let icon = self.symbolic(name, icon_px as u32, tint);
                match (text_label, icon) {
                    (None, Some(icon)) => canvas.draw_image(r.x + (r.w - icon_px) / 2, iy, icon.size, &icon.rgba),
                    (Some(t), icon) => {
                        let mut x = r.x + 7 * s;
                        if let Some(icon) = icon {
                            canvas.draw_image(x, iy, icon.size, &icon.rgba);
                            x += icon_px + 4 * s;
                        }
                        label(canvas, t, x);
                    }
                    (None, None) => {}
                }
            }
            Look::Media { text: t, playing } => {
                let name = if *playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                };
                let mut x = r.x + 7 * s;
                if let Some(icon) = self.symbolic(name, icon_px as u32, tint) {
                    canvas.draw_image(x, iy, icon.size, &icon.rgba);
                }
                x += icon_px + 4 * s;
                label(canvas, t, x);
            }
            Look::Bell { unread, dnd } => {
                let name = if *dnd {
                    "notifications-disabled-symbolic"
                } else {
                    "preferences-system-notifications-symbolic"
                };
                let (x, y) = (r.x + (r.w - icon_px) / 2, iy);
                if let Some(icon) = self.symbolic(name, icon_px as u32, tint) {
                    canvas.draw_image(x, y, icon.size, &icon.rgba);
                }
                if *unread {
                    let d = 6 * s;
                    canvas.fill_rounded_rect(x + icon_px - d + s, y - s, d, d, d as f32 / 2.0, p.accent);
                }
            }
        }
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
    let (w, h) = (14 * s, 10 * s);
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
        self.open_app(SETTINGS_APP);
    }

    /// Opens one of our apps, or brings it forward if it's already open (`app` is both its
    /// command and its app id).
    fn open_app(&mut self, app: &str) {
        let open = self
            .monitors
            .iter()
            .flat_map(|m| &m.workspaces)
            .flat_map(|ws| &ws.entries)
            .find(|e| toplevel_ids(&e.window).0.as_deref() == Some(app))
            .map(|e| e.window.clone());
        match open {
            Some(w) => self.activate_window(&w),
            None => self.spawn(app),
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
                name_w: self.bar.text.as_ref().map_or(0, |t| {
                    t.width(&w.label.name, self.config.bar.font_size * 0.9).ceil() as i32
                }),
                name: w.label.name.clone(),
                icon: w.label.icon.clone(),
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
                    minimized: e.minimized,
                }
            })
            .collect();
        let name = m.output.name();
        let cfg = &self.config.bar;
        // Status widgets sit with the clock; with no clock on any connected monitor, every bar
        // has them.
        let any_clock = self.monitors.iter().any(|m| cfg.clock_on(&m.output.name()));
        let status = if !any_clock || cfg.clock_on(&name) {
            self.status_items(size.1.min(cfg.height))
        } else {
            Vec::new()
        };
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
            status,
        }
    }

    /// The status widgets to show, for a bar (or side bar row) `h` high.
    fn status_items(&self, h: i32) -> Vec<StatusItem> {
        use crate::{config::Widget, status::Network};
        let px = self.config.bar.font_size * 0.9;
        let text_w = |t: &str| self.bar.text.as_ref().map_or(0, |x| x.width(t, px).ceil() as i32);
        let icon = ws_icon_size(h);
        let labelled = |name: String, label: String| StatusItem {
            part: Part::Clock,
            w: 7 + icon + 4 + text_w(&label) + 8,
            look: Look::Icon {
                name,
                label: Some(label),
            },
        };
        let mut out = Vec::new();
        for w in &self.config.bar.widgets {
            match w {
                Widget::Tray => {
                    for (i, t) in self.bus.tray.iter().enumerate() {
                        out.push(StatusItem {
                            part: Part::Tray(i),
                            w: h,
                            look: Look::Tray {
                                icon_name: t.icon_name.clone(),
                                theme_path: t.icon_theme_path.clone(),
                                pixmap: t.pixmap.clone(),
                            },
                        });
                    }
                }
                Widget::Media => {
                    if let Some(m) = &self.bus.media {
                        let text = match (m.artist.is_empty(), m.title.is_empty()) {
                            (false, false) => format!("{} – {}", m.artist, m.title),
                            (true, false) => m.title.clone(),
                            _ => m.player.trim_start_matches(crate::dbus::mpris::PREFIX).to_string(),
                        };
                        let w = (7 + icon + 4 + text_w(&text) + 8).min(MEDIA_MAX_W);
                        out.push(StatusItem {
                            part: Part::Media,
                            w,
                            look: Look::Media {
                                text,
                                playing: m.playing,
                            },
                        });
                    }
                }
                Widget::Sound => {
                    if self.sound.present {
                        let v = self.sound.volume;
                        let name = match (self.sound.muted || v <= 0.001, v) {
                            (true, _) => "audio-volume-muted-symbolic",
                            (false, ..0.34) => "audio-volume-low-symbolic",
                            (false, ..0.67) => "audio-volume-medium-symbolic",
                            (false, _) => "audio-volume-high-symbolic",
                        };
                        out.push(StatusItem {
                            part: Part::Sound,
                            ..labelled(name.into(), format!("{}%", (v * 100.0).round() as i32))
                        });
                    }
                }
                Widget::Network => {
                    let name = match self.status.network {
                        Network::Offline => "network-offline-symbolic",
                        Network::Wired => "network-wired-symbolic",
                        Network::Wireless(q) => match q {
                            0..=19 => "network-wireless-signal-none-symbolic",
                            20..=39 => "network-wireless-signal-weak-symbolic",
                            40..=59 => "network-wireless-signal-ok-symbolic",
                            60..=79 => "network-wireless-signal-good-symbolic",
                            _ => "network-wireless-signal-excellent-symbolic",
                        },
                    };
                    out.push(StatusItem {
                        part: Part::Network,
                        w: h,
                        look: Look::Icon {
                            name: name.into(),
                            label: None,
                        },
                    });
                }
                Widget::Bluetooth => {
                    if self.bluetooth.present {
                        let name = match (self.bluetooth.powered, self.bluetooth.connected) {
                            (false, _) => "bluetooth-disabled-symbolic",
                            (true, 0) => "bluetooth-symbolic",
                            (true, _) => "bluetooth-active-symbolic",
                        };
                        out.push(StatusItem {
                            part: Part::Bluetooth,
                            w: h,
                            look: Look::Icon {
                                name: name.into(),
                                label: None,
                            },
                        });
                    }
                }
                Widget::Battery => {
                    if let Some(b) = self.status.battery {
                        let level = (b.percent as u32 + 5) / 10 * 10;
                        let name = match (b.full, b.charging) {
                            (true, _) => "battery-level-100-charged-symbolic".to_string(),
                            (false, true) => format!("battery-level-{level}-charging-symbolic"),
                            (false, false) => format!("battery-level-{level}-symbolic"),
                        };
                        out.push(StatusItem {
                            part: Part::Battery,
                            ..labelled(name, format!("{}%", b.percent))
                        });
                    }
                }
                Widget::Brightness => {
                    if let Some(b) = &self.status.brightness {
                        out.push(StatusItem {
                            part: Part::Brightness,
                            ..labelled("display-brightness-symbolic".into(), format!("{}%", b.percent()))
                        });
                    }
                }
                Widget::Notifications => {
                    if self.config.notifications.enabled {
                        out.push(StatusItem {
                            part: Part::Bell,
                            w: h,
                            look: Look::Bell {
                                unread: self.notifications.unread() > 0,
                                dnd: self.notifications.dnd,
                            },
                        });
                    }
                }
            }
        }
        out
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

    /// The focused monitor's bar parts as "Part x y w h" in global coordinates, for tests.
    pub fn bar_parts_debug(&self) -> Vec<String> {
        let mon = self.active_mon;
        let Some(rect) = self.bar_rect(mon) else {
            return Vec::new();
        };
        let model = self.bar_model(mon, (rect.size.w, rect.size.h), 1);
        self.bar
            .layout(&model)
            .into_iter()
            .map(|(r, p)| format!("{p:?} {} {} {} {}", r.x + rect.loc.x, r.y + rect.loc.y, r.w, r.h))
            .collect()
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
            (Part::Tray(i), _) => {
                let Some(item) = self.bus.tray.get(i).cloned() else {
                    return;
                };
                let pos = self.seat.get_pointer().unwrap().current_location();
                let (x, y) = (pos.x as i32, pos.y as i32);
                let menu = item.has_menu && (button == BTN_RIGHT || (button == BTN_LEFT && item.item_is_menu));
                if menu {
                    self.tray_menu_request = Some((item.id.clone(), pos, mon));
                    self.bus_command(crate::dbus::Command::TrayMenu { item: item.id });
                } else if button == BTN_LEFT {
                    self.bus_command(crate::dbus::Command::TrayActivate { item: item.id, x, y });
                } else {
                    self.bus_command(crate::dbus::Command::TraySecondary { item: item.id, x, y });
                }
            }
            (Part::Media, BTN_LEFT) => {
                self.bus_command(crate::dbus::Command::Media(crate::dbus::mpris::MediaCommand::PlayPause))
            }
            (Part::Media, BTN_MIDDLE | BTN_RIGHT) => {
                self.bus_command(crate::dbus::Command::Media(crate::dbus::mpris::MediaCommand::Next))
            }
            (Part::Bell, BTN_LEFT) => {
                let pos = self.seat.get_pointer().unwrap().current_location();
                self.open_notifications_menu(mon, pos);
            }
            (Part::Bell, BTN_RIGHT) => self.toggle_dnd(),
            (Part::Bell, BTN_MIDDLE) => self.clear_notifications(),
            (Part::Layout, BTN_LEFT | BTN_RIGHT) => self.run_action(Action::Layout(LayoutChoice::Next)),
            (Part::Settings, BTN_LEFT) => self.open_settings(),
            (Part::Sound, BTN_LEFT) => self.open_app(SOUND_APP),
            (Part::Sound, BTN_RIGHT) => self.change_volume(crate::config::VolumeChange::ToggleMute),
            (Part::Bluetooth, BTN_LEFT) => self.open_app(BLUETOOTH_APP),
            (Part::Bluetooth, BTN_RIGHT) => self.toggle_bluetooth(),
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
        let lands = if to_idx > from {
            p >= end - size
        } else {
            p < start + size
        };
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
        // Widgets that take scrolling: volume, brightness, media (skip tracks), tray icons.
        if let Some((_, part)) = self.bar_part_at(pos) {
            use crate::dbus::{Command, mpris::MediaCommand};
            match part {
                Part::Sound => {
                    if steps != 0 {
                        self.change_volume(crate::config::VolumeChange::Step(-steps * 5));
                    }
                    return true;
                }
                Part::Brightness => {
                    if steps != 0 {
                        self.change_brightness(-steps);
                    }
                    return true;
                }
                Part::Media => {
                    match steps.signum() {
                        1 => self.bus_command(Command::Media(MediaCommand::Next)),
                        -1 => self.bus_command(Command::Media(MediaCommand::Previous)),
                        _ => {}
                    }
                    return true;
                }
                Part::Tray(i) => {
                    if steps != 0
                        && let Some(item) = self.bus.tray.get(i)
                    {
                        let item = item.id.clone();
                        self.bus_command(Command::TrayScroll {
                            item,
                            delta: steps * 120,
                        });
                    }
                    return true;
                }
                _ => {}
            }
        }
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

/// A palette color as 0-255 RGBA, for tinting icons.
fn to_u8(c: Color) -> [u8; 4] {
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Tray icon pixels (ARGB32, big-endian) as premultiplied RGBA.
fn argb_to_rgba(argb: &[u8]) -> Vec<u8> {
    argb.chunks_exact(4)
        .flat_map(|p| {
            let [a, r, g, b] = [p[0], p[1], p[2], p[3]];
            let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
            [m(r), m(g), m(b), a]
        })
        .collect()
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
                ..Default::default()
            })
            .collect(),
            tabs: tabs
                .iter()
                .map(|(t, f)| Tab {
                    title: t.to_string(),
                    app_id: "Alacritty".into(),
                    focused: *f,
                    minimized: false,
                })
                .collect(),
            layout: LayoutKind::Tile,
            hover,
            active_monitor: true,
            clock: None,
            status: Vec::new(),
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
    fn side_bar_groups_icons_labelled_widgets_and_buttons() {
        let bar = Bar::new(&Config::default().bar);
        let icon = |part: Part| StatusItem {
            part,
            w: 28,
            look: Look::Icon {
                name: "x".into(),
                label: None,
            },
        };
        let labelled = |part: Part| StatusItem {
            part,
            w: 64,
            look: Look::Icon {
                name: "x".into(),
                label: Some("65%".into()),
            },
        };
        let mut m = model((180, 1400), BarPosition::Left, &[], None);
        m.clock = Some("7:48 AM".into());
        // Config order mixes them up: tray, media, sound, bluetooth, brightness, bell.
        m.status = vec![
            icon(Part::Tray(0)),
            StatusItem {
                part: Part::Media,
                w: 240,
                look: Look::Media {
                    text: "Master Boot Record".into(),
                    playing: true,
                },
            },
            labelled(Part::Sound),
            icon(Part::Bluetooth),
            labelled(Part::Brightness),
            StatusItem {
                part: Part::Bell,
                w: 28,
                look: Look::Bell {
                    unread: false,
                    dnd: false,
                },
            },
        ];
        let parts = bar.layout(&m);
        let at = |part: Part| parts.iter().find(|(_, p)| *p == part).unwrap().0;
        let row = m.config.height;
        // Clock at the very bottom, the buttons right above it, spread over the whole row.
        assert_eq!(at(Part::Clock).y, 1400 - row);
        let buttons = end_buttons(&m);
        assert_eq!(buttons, [Part::Layout, Part::Bell, Part::Settings, Part::Power]);
        for b in &buttons {
            assert_eq!(at(*b).y, 1400 - 2 * row, "{b:?}");
        }
        assert_eq!(at(Part::Layout).x, SIDE_INSET);
        assert_eq!(at(Part::Power).x + at(Part::Power).w, 180 - SIDE_INSET);
        // Labelled widgets get a full-width row each, in config order, above the buttons.
        for (k, part) in [Part::Media, Part::Sound, Part::Brightness].into_iter().enumerate() {
            let r = at(part);
            assert_eq!((r.x, r.w), (SIDE_INSET, 180 - 2 * SIDE_INSET), "{part:?}");
            assert_eq!(r.y, 1400 - (5 - k as i32) * row, "{part:?}");
        }
        // The icons share the top row, from the left.
        let (tray, bt) = (at(Part::Tray(0)), at(Part::Bluetooth));
        assert_eq!((tray.y, bt.y), (1400 - 6 * row, 1400 - 6 * row));
        assert_eq!((tray.x, bt.x), (SIDE_INSET, SIDE_INSET + row));

        // More icons than fit in a row wrap onto a second one.
        m.status = (0..8).map(|i| icon(Part::Tray(i))).collect();
        let parts = bar.layout(&m);
        let mut ys: Vec<i32> = parts
            .iter()
            .filter(|(_, p)| matches!(p, Part::Tray(_)))
            .map(|(r, _)| r.y)
            .collect();
        ys.dedup();
        assert_eq!(ys.len(), 2);
    }

    #[test]
    fn bell_goes_after_the_layout_button() {
        let bar = Bar::new(&Config::default().bar);
        let mut m = model((1200, 28), BarPosition::Top, &[], None);
        m.status = vec![StatusItem {
            part: Part::Bell,
            w: 28,
            look: Look::Bell {
                unread: false,
                dnd: false,
            },
        }];
        let parts = bar.layout(&m);
        let x = |part: Part| parts.iter().find(|(_, p)| *p == part).unwrap().0;
        assert_eq!(x(Part::Bell).x, x(Part::Layout).x + x(Part::Layout).w);
        assert_eq!(x(Part::Settings).x, x(Part::Bell).x + 28);
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
                ("uaio@host: ~/git/kiln", true),
                ("Start Page - Vivaldi", false),
                (
                    "bar.rs - kiln - A very long window title that will need to be cut short",
                    false,
                ),
                ("Steam", false),
            ],
            Some(Part::Tab(1)),
        );
        let mut m = m;
        m.clock = Some(clock_text());
        let row = size.1.min(m.config.height);
        m.status = vec![
            StatusItem {
                part: Part::Tray(0),
                w: row,
                look: Look::Tray {
                    icon_name: "org.qbittorrent.qBittorrent".into(),
                    theme_path: String::new(),
                    pixmap: None,
                },
            },
            StatusItem {
                part: Part::Media,
                w: 200,
                look: Look::Media {
                    text: "Master Boot Record – Internet Protocol".into(),
                    playing: true,
                },
            },
            StatusItem {
                part: Part::Sound,
                w: 64,
                look: Look::Icon {
                    name: "audio-volume-medium-symbolic".into(),
                    label: Some("65%".into()),
                },
            },
            StatusItem {
                part: Part::Bluetooth,
                w: row,
                look: Look::Icon {
                    name: "bluetooth-active-symbolic".into(),
                    label: None,
                },
            },
            StatusItem {
                part: Part::Battery,
                w: 64,
                look: Look::Icon {
                    name: "battery-good-symbolic".into(),
                    label: Some("80%".into()),
                },
            },
            StatusItem {
                part: Part::Bell,
                w: row,
                look: Look::Bell {
                    unread: true,
                    dnd: false,
                },
            },
        ];
        // Two named workspaces, one with an icon too.
        for (i, name, icon) in [(0, "web", "firefox"), (2, "chat", "")] {
            m.workspaces[i].name = name.into();
            m.workspaces[i].icon = icon.into();
            m.workspaces[i].name_w = text.width(name, m.config.font_size * 0.9).ceil() as i32;
        }
        for (tab, app) in m.tabs.iter_mut().zip(["Alacritty", "vivaldi-stable", "code", "XClock"]) {
            tab.app_id = app.into();
        }
        let canvas = bar.paint_canvas(&text, &m);
        std::fs::write(&path, canvas.to_premultiplied_rgba()).unwrap();
        eprintln!("{}x{}", canvas.w, canvas.h);
    }
}
